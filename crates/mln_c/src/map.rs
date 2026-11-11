//! `map.h`: maps' lifecycle, snapshots, size, command groups, styles by URL
//! or JSON, event masks and feature state.

use std::sync::Arc;

use ::mln_map::{MapMode, MapSnapshot};
use ::mln_runtime::{Applied, EventMask, FeatureState, FeatureStateSelector, MapOptions};
use feature::{Identifier, Value};
use mln_util::geo::LatLngBounds;
use mln_util::size::Size;

use crate::buffer::LentText;
use crate::completion::{self, CompletionValue, Generations, PendingCompletion};
use crate::error::Error;
use crate::generated::types::{
    MLN_FEATURE_ID_TYPE_DOUBLE, MLN_FEATURE_ID_TYPE_INT, MLN_FEATURE_ID_TYPE_STRING, MLN_FEATURE_ID_TYPE_UINT,
    MLN_FEATURE_STATE_SELECTOR_FEATURE_ID, MLN_MAP_MODE_CONTINUOUS, MLN_MAP_MODE_STATIC, MLN_MAP_MODE_TILE,
    MLN_MAP_OPTION_EVENT_MASK, MLN_MAP_OPTION_TILE_CACHE_ENABLED, MLN_TILE_LOD_MODE_DEFAULT,
    MLN_TILE_LOD_MODE_DISTANCE, mln_buffer_view, mln_camera_bounds, mln_completion, mln_diagnostic,
    mln_feature_state_selector, mln_free_camera, mln_lat_lng_bounds, mln_logical_size, mln_map, mln_map_options,
    mln_map_snapshot, mln_map_tile_settings, mln_quaternion, mln_runtime, mln_runtime_event_flags, mln_status,
    mln_vec3,
};
use crate::handles::{self, HandleKind, HandleTable};
use crate::runtime::{self, MapHandles};
use crate::{abi, buffer, geo, lock, parse_thread, projection};

abi::versioned!(mln_map_options, mln_map_snapshot, mln_feature_state_selector);

pub(crate) struct MapObject {
    pub(crate) map: ::mln_runtime::Map,
    /// The handles of the runtime's maps, which this map's entry leaves once
    /// it is released.
    handles: Arc<MapHandles>,
    /// The scale factor as the caller gave it, which the map's views report
    /// rather than the map's single-precision pixel ratio.
    scale_factor: f64,
}

static MAPS: HandleTable<MapObject> = HandleTable::new(HandleKind::Map);

/// The map that `map` names.
pub(crate) fn resolve(map: mln_map) -> Result<Arc<MapObject>, Error> {
    MAPS.get(map)
}

/// The generations that a command's completion reports: those it applied
/// at, or for a failure, those of the map's latest snapshot, which the
/// failure's events carry.
fn generations(map: &::mln_runtime::Map, result: &Result<Applied, mln_util::Error>) -> Generations {
    match result {
        Ok(applied) => Generations { snapshot: applied.generation, render_update: applied.render_update_generation },
        Err(_) => map.snapshot().map_or(Generations::default(), |snapshot| Generations {
            snapshot: snapshot.generation,
            render_update: snapshot.update_generation,
        }),
    }
}

/// Sends a map command, which takes an optional completion.
///
/// # Safety
///
/// `completion` is null or valid for the header's use of it.
unsafe fn command(
    map: mln_map,
    completion: *const mln_completion,
    send: impl FnOnce(&::mln_runtime::Map) -> Result<::mln_runtime::Completion<Applied>, mln_util::Error>,
) -> Result<(), Error> {
    let object = resolve(map)?;
    // SAFETY: the caller's contract.
    let pending = unsafe { PendingCompletion::optional(completion) }?;
    let sent = send(&object.map)?;
    let map = object.map.clone();
    completion::forward(sent, pending.map(PendingCompletion::accept), move |result| {
        let generations = generations(&map, &result);
        (result.map(drop).map_err(Error::from), generations)
    });
    Ok(())
}

/// Sends a map query, which needs a completion for its result.
///
/// # Safety
///
/// `completion` is null or valid for the header's use of it.
unsafe fn query<T: Send + 'static, V: CompletionValue>(
    map: mln_map,
    completion: *const mln_completion,
    send: impl FnOnce(&::mln_runtime::Map) -> Result<::mln_runtime::Completion<T>, mln_util::Error>,
    value: impl FnOnce(T) -> V + Send + 'static,
) -> Result<(), Error> {
    let object = resolve(map)?;
    // SAFETY: the caller's contract.
    let pending = unsafe { PendingCompletion::new(completion) }?;
    let sent = send(&object.map)?;
    let map = object.map.clone();
    completion::forward(sent, Some(pending.accept()), move |result| {
        let generations = map.snapshot().map_or(Generations::default(), |snapshot| Generations {
            snapshot: snapshot.generation,
            render_update: snapshot.update_generation,
        });
        (result.map(value).map_err(Error::from), generations)
    });
    Ok(())
}

/// The runtime layer's options for a caller's `mln_map_options`, and the
/// scale factor they give.
///
/// # Safety
///
/// `options` is null or points to a readable `mln_map_options` of the size it
/// declares, whose views are valid.
unsafe fn map_options(options: *const mln_map_options) -> Result<(MapOptions, f64), Error> {
    // SAFETY: the caller's contract.
    let options = unsafe { abi::read_sized(options) }?;
    let fields = abi::Fields::new(
        options.fields,
        MLN_MAP_OPTION_EVENT_MASK | MLN_MAP_OPTION_TILE_CACHE_ENABLED,
        "mln_map_options.fields",
    )?;
    let event_mask = match fields.get(MLN_MAP_OPTION_EVENT_MASK, options.event_mask) {
        None => EventMask::ALL,
        Some(bits) => EventMask::from_bits(bits)
            .ok_or_else(|| Error::invalid_argument("mln_map_options.event_mask contains unknown bits"))?,
    };
    let extent = options.initial_extent;
    if extent.width == 0 || extent.height == 0 || !extent.scale_factor.is_finite() || extent.scale_factor <= 0.0 {
        return Err(Error::invalid_argument("initial extent dimensions and scale factor must be positive"));
    }
    let mode = match abi::enum_value(options.map_mode, MLN_MAP_MODE_CONTINUOUS..=MLN_MAP_MODE_TILE, "map_mode")? {
        MLN_MAP_MODE_STATIC => MapMode::Static,
        MLN_MAP_MODE_TILE => MapMode::Tile,
        _ => MapMode::Continuous,
    };
    // SAFETY: the caller's contract.
    let faces = unsafe { buffer::text(options.fallback_font_faces, "mln_map_options.fallback_font_faces") }?;
    let fallback_font_faces = if faces.is_empty() {
        None
    } else {
        // Checked here, so that the call fails rather than the map's
        // creation, and on the parse thread, which has the stack for it.
        let json = faces.to_owned();
        let parsed = parse_thread::run(move || mln_style::parser::parse_fallback_font_faces(&json).map(drop))?;
        parsed.map_err(|error| {
            Error::invalid_argument(format!("mln_map_options.fallback_font_faces is invalid: {}", error.message))
        })?;
        Some(faces.to_owned())
    };
    let options = MapOptions {
        map: ::mln_map::MapOptions::new()
            .with_map_mode(mode)
            .with_size(Size::new(extent.width, extent.height))
            .with_pixel_ratio(extent.scale_factor as f32)
            .with_fast_pfor_enabled(options.fast_pfor_enabled),
        event_mask,
        fallback_font_faces,
        tile_cache_enabled: fields.get(MLN_MAP_OPTION_TILE_CACHE_ENABLED, options.tile_cache_enabled).unwrap_or(true),
    };
    Ok((options, extent.scale_factor))
}

/// `mln_runtime_create_map()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_create_map(
    runtime: mln_runtime,
    options: *const mln_map_options,
    out_map: *mut mln_map,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let (options, scale_factor) = unsafe { map_options(options) }?;
        // SAFETY: the caller's contract.
        let out_map = unsafe { handles::out_handle(out_map, "out_map") }?;
        let runtime = runtime::resolve(runtime)?;
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::new(completion) }?;
        let (handle, created) = {
            // The runtime's events find the map's handle once this lock goes,
            // including the events of its creation.
            let mut handles = lock(&runtime.maps);
            let (map, created) = runtime.runtime.create_map(options)?;
            let id = map.id();
            let handle = MAPS.insert(Arc::new(MapObject { map, handles: Arc::clone(&runtime.maps), scale_factor }))?;
            handles.insert(id, handle);
            (handle, created)
        };
        // The completion may run before this call returns, and the caller may
        // read the handle there.
        *out_map = handle;
        completion::forward(created, Some(pending.accept()), |result| {
            let generations = match &result {
                Ok(applied) => {
                    Generations { snapshot: applied.generation, render_update: applied.render_update_generation }
                }
                Err(_) => Generations::default(),
            };
            (result.map(drop).map_err(Error::from), generations)
        });
        Ok(())
    })
}

/// Lets go of a removed map: its handle leaves the runtime's table once the
/// map's last events are out.
fn retire(object: Arc<MapObject>, completion: Option<completion::Completion>) -> Result<(), Error> {
    let released = object.map.release()?;
    let id = object.map.id();
    let handles = Arc::clone(&object.handles);
    released.on_complete(move |result| {
        lock(&handles).remove(&id);
        if let Some(completion) = completion {
            completion.complete(result.map_err(Error::from), Generations::default());
        }
    });
    Ok(())
}

/// `mln_map_release()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_release(
    map: mln_map,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::new(completion) }?;
        let object = MAPS.remove(map)?;
        retire(object, Some(pending.accept()))
    })
}

/// `mln_map_dispose()`.
///
/// # Safety
///
/// `out_diagnostic` is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_dispose(map: mln_map, out_diagnostic: *mut mln_diagnostic) -> mln_status {
    export!(out_diagnostic, {
        let object = MAPS.remove(map)?;
        retire(object, None)
    })
}

fn from_snapshot(snapshot: &MapSnapshot, event_mask: mln_runtime_event_flags) -> mln_map_snapshot {
    let tile = &snapshot.tile;
    let bounds = &snapshot.bounds;
    let area = bounds.bounds.unwrap_or_else(LatLngBounds::world);
    let [x, y, z] = snapshot.free_camera.position;
    let [qx, qy, qz, qw] = snapshot.free_camera.orientation;
    mln_map_snapshot {
        size: size_of::<mln_map_snapshot>() as u32,
        debug_flags: snapshot.debug.0,
        generation: snapshot.generation,
        fully_loaded: snapshot.fully_loaded,
        rendering_stats_view_enabled: snapshot.rendering_stats_view_enabled,
        gesture_in_progress: snapshot.gesture_in_progress,
        transition_in_progress: snapshot.transition_in_progress,
        event_mask,
        latest_render_update_generation: snapshot.update_generation,
        tile: mln_map_tile_settings {
            prefetch_zoom_delta: u32::from(tile.prefetch_zoom_delta),
            lod_min_radius: tile.lod_min_radius,
            lod_scale: tile.lod_scale,
            lod_pitch_threshold: tile.lod_pitch_threshold,
            lod_zoom_shift: tile.lod_zoom_shift,
            lod_mode: match tile.lod_mode {
                ::mln_map::TileLodMode::Default => MLN_TILE_LOD_MODE_DEFAULT,
                ::mln_map::TileLodMode::Distance => MLN_TILE_LOD_MODE_DISTANCE,
            },
        },
        bounds: mln_camera_bounds {
            bounds: mln_lat_lng_bounds {
                southwest: geo::from_lat_lng(area.southwest()),
                northeast: geo::from_lat_lng(area.northeast()),
            },
            unbounded: bounds.bounds.is_none(),
            min_zoom: bounds.min_zoom,
            max_zoom: bounds.max_zoom,
            min_pitch: bounds.min_pitch,
            max_pitch: bounds.max_pitch,
        },
        free_camera: mln_free_camera {
            position: mln_vec3 { x, y, z },
            orientation: mln_quaternion { x: qx, y: qy, z: qz, w: qw },
        },
        view: projection::from_map_view(&snapshot.view),
    }
}

/// `mln_map_get_snapshot()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_get_snapshot(
    map: mln_map,
    out_snapshot: *mut mln_map_snapshot,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_snapshot), {
        let object = resolve(map)?;
        let snapshot = object.map.snapshot()?;
        let mut snapshot = from_snapshot(&snapshot, object.map.event_mask().bits());
        snapshot.view.extent.scale_factor = object.scale_factor;
        // SAFETY: the caller's contract.
        unsafe { abi::write_sized(out_snapshot, &snapshot) }
    })
}

/// `mln_map_resize()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_resize(
    map: mln_map,
    logical_size: mln_logical_size,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.resize(Size::new(logical_size.width, logical_size.height))) }
    })
}

/// `mln_map_request_repaint()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_request_repaint(
    map: mln_map,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, ::mln_runtime::Map::request_repaint) }
    })
}

/// `mln_map_begin_command_group()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_begin_command_group(
    map: mln_map,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, ::mln_runtime::Map::begin_command_group) }
    })
}

/// `mln_map_end_command_group()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_end_command_group(
    map: mln_map,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, ::mln_runtime::Map::end_command_group) }
    })
}

/// `mln_map_set_event_mask()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_event_mask(
    map: mln_map,
    mask: mln_runtime_event_flags,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let mask =
            EventMask::from_bits(mask).ok_or_else(|| Error::invalid_argument("event mask contains unknown bits"))?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_event_mask(mask)) }
    })
}

/// `mln_map_set_style_url()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_style_url(
    map: mln_map,
    url: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let url = unsafe { buffer::text(url, "style URL") }?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_style_url(url)) }
    })
}

/// `mln_map_set_style_json()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_style_json(
    map: mln_map,
    json: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let json = unsafe { buffer::text(json, "style JSON") }?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_style_json(json)) }
    })
}

/// `mln_map_get_loaded_style_json()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_get_loaded_style_json(
    map: mln_map,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { query(map, completion, ::mln_runtime::Map::loaded_style_json, |json| LentText::new(&json)) }
    })
}

/// `mln_map_get_style_url()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_get_style_url(
    map: mln_map,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { query(map, completion, ::mln_runtime::Map::style_url, |url| LentText::new(&url)) }
    })
}

// MARK: - Feature state

/// The text of a caller's view, which may be empty but never null with a
/// size.
///
/// # Safety
///
/// As for [`buffer::text`].
unsafe fn selector_text(view: mln_buffer_view, name: &str) -> Result<String, Error> {
    if view.data.is_null() && view.size != 0 {
        return Err(Error::invalid_argument(format!("{name} must not be null")));
    }
    // SAFETY: the caller's contract.
    unsafe { buffer::text(view, name) }.map(str::to_owned)
}

/// The runtime layer's selector for a caller's `mln_feature_state_selector`.
///
/// # Safety
///
/// `selector` is null or points to a readable selector of the size it
/// declares, whose views are valid.
unsafe fn selector(selector: *const mln_feature_state_selector) -> Result<FeatureStateSelector, Error> {
    if selector.is_null() {
        return Err(Error::invalid_argument("feature state selector must not be null"));
    }
    // SAFETY: the caller's contract.
    let selector = unsafe { abi::read_sized(selector) }?;
    if selector.fields & !MLN_FEATURE_STATE_SELECTOR_FEATURE_ID != 0 {
        return Err(Error::invalid_argument("feature state selector has unknown fields"));
    }
    // SAFETY: the caller's contract, for each view.
    let (source_id, source_layer_id, state_key) = unsafe {
        (
            selector_text(selector.source_id, "feature state source_id")?,
            selector_text(selector.source_layer_id, "feature state source_layer_id")?,
            selector_text(selector.state_key, "feature state state_key")?,
        )
    };
    let feature_id = if selector.fields & MLN_FEATURE_STATE_SELECTOR_FEATURE_ID == 0 {
        None
    } else {
        let id = selector.feature_id;
        // SAFETY: each arm reads the member that `type_` names.
        Some(unsafe {
            match id.type_ {
                MLN_FEATURE_ID_TYPE_STRING => {
                    Identifier::String(selector_text(id.data.string_value, "feature_id string_value")?)
                }
                MLN_FEATURE_ID_TYPE_UINT => Identifier::Uint(id.data.uint_value),
                MLN_FEATURE_ID_TYPE_INT => Identifier::Int(id.data.int_value),
                MLN_FEATURE_ID_TYPE_DOUBLE => Identifier::Double(id.data.double_value),
                _ => return Err(Error::invalid_argument("feature_id has an unknown type")),
            }
        })
    };
    Ok(FeatureStateSelector {
        source_id,
        source_layer_id: Some(source_layer_id).filter(|id| !id.is_empty()),
        feature_id,
        state_key: Some(state_key).filter(|key| !key.is_empty()),
    })
}

/// A value of the JSON value `json`.
fn value(json: serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(value),
        serde_json::Value::Number(number) => match (number.as_u64(), number.as_i64()) {
            (Some(value), _) => Value::Uint(value),
            (None, Some(value)) => Value::Int(value),
            (None, None) => Value::Double(number.as_f64().unwrap_or_default()),
        },
        serde_json::Value::String(value) => Value::String(value),
        serde_json::Value::Array(values) => Value::Array(values.into_iter().map(value).collect()),
        serde_json::Value::Object(members) => {
            Value::Object(members.into_iter().map(|(key, member)| (key, value(member))).collect())
        }
    }
}

/// The state that the JSON object `json` holds, parsed on the parse thread.
fn feature_state(json: &str) -> Result<FeatureState, Error> {
    if json.is_empty() {
        return Err(Error::invalid_argument("feature state must not be empty"));
    }
    let json = json.to_owned();
    parse_thread::run(move || match serde_json::from_str(&json) {
        Ok(serde_json::Value::Object(members)) => {
            Ok(members.into_iter().map(|(key, member)| (key, value(member))).collect())
        }
        Ok(_) => Err(Error::invalid_argument("feature state must be a JSON object")),
        Err(error) => Err(Error::invalid_argument(format!("feature state isn't valid JSON: {error}"))),
    })?
}

/// `mln_map_set_feature_state()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_feature_state(
    map: mln_map,
    selector: *const mln_feature_state_selector,
    state: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let selector = unsafe { self::selector(selector) }?;
        // SAFETY: the caller's contract.
        let state = feature_state(unsafe { buffer::text(state, "feature state") }?)?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_feature_state(selector, state)) }
    })
}

/// `mln_map_get_feature_state()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_get_feature_state(
    map: mln_map,
    selector: *const mln_feature_state_selector,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let selector = unsafe { self::selector(selector) }?;
        // SAFETY: the caller's contract.
        unsafe {
            query(
                map,
                completion,
                |map| map.feature_state(selector),
                |state| {
                    let json = mln_conversion::value_conversion::value_to_json(&Value::Object(state));
                    LentText::new(&json.to_string())
                },
            )
        }
    })
}

/// `mln_map_remove_feature_state()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_remove_feature_state(
    map: mln_map,
    selector: *const mln_feature_state_selector,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let selector = unsafe { self::selector(selector) }?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.remove_feature_state(selector)) }
    })
}

#[cfg(test)]
mod tests {
    use core::ffi::c_void;
    use std::sync::mpsc;

    use super::*;
    use crate::diagnostic::tests::{diagnostic, message};
    use crate::generated::types::{
        MLN_STATUS_INVALID_ARGUMENT, MLN_STATUS_INVALID_STATE, MLN_STATUS_OK, mln_completion_result, mln_feature_id,
        mln_feature_id_data, mln_logical_extent,
    };
    use crate::runtime::tests::{Waiter, create, release};

    const NO_DIAGNOSTIC: *mut mln_diagnostic = core::ptr::null_mut();

    fn options(scale_factor: f64) -> mln_map_options {
        // SAFETY: zeros are valid options, which the fields below complete.
        let mut options: mln_map_options = unsafe { core::mem::zeroed() };
        options.size = size_of::<mln_map_options>() as u32;
        options.initial_extent = mln_logical_extent { width: 64, height: 32, scale_factor };
        options
    }

    fn create_map(runtime: mln_runtime, options: &mln_map_options) -> mln_map {
        let waiter = Waiter::new();
        let mut map = 0;
        // SAFETY: local arguments.
        let status = unsafe { mln_runtime_create_map(runtime, options, &mut map, &waiter.descriptor, NO_DIAGNOSTIC) };
        assert_eq!(status, MLN_STATUS_OK);
        assert_eq!(waiter.status(), MLN_STATUS_OK);
        map
    }

    fn release_map(map: mln_map) {
        let waiter = Waiter::new();
        // SAFETY: a local completion.
        assert_eq!(unsafe { mln_map_release(map, &waiter.descriptor, NO_DIAGNOSTIC) }, MLN_STATUS_OK);
        assert_eq!(waiter.status(), MLN_STATUS_OK);
    }

    fn snapshot(map: mln_map) -> mln_map_snapshot {
        // SAFETY: zeros are a valid snapshot to overwrite.
        let mut snapshot: mln_map_snapshot = unsafe { core::mem::zeroed() };
        snapshot.size = size_of::<mln_map_snapshot>() as u32;
        // SAFETY: a local snapshot.
        assert_eq!(unsafe { mln_map_get_snapshot(map, &mut snapshot, NO_DIAGNOSTIC) }, MLN_STATUS_OK);
        snapshot
    }

    fn view(text: &str) -> mln_buffer_view {
        mln_buffer_view { data: text.as_ptr().cast(), size: text.len() }
    }

    fn selector(source: &'static str, id: Option<mln_feature_id>) -> mln_feature_state_selector {
        mln_feature_state_selector {
            size: size_of::<mln_feature_state_selector>() as u32,
            fields: if id.is_some() { MLN_FEATURE_STATE_SELECTOR_FEATURE_ID } else { 0 },
            source_id: view(source),
            source_layer_id: view(""),
            feature_id: id.unwrap_or(mln_feature_id { type_: 0, data: mln_feature_id_data { uint_value: 0 } }),
            state_key: view(""),
        }
    }

    unsafe extern "C" fn send_text(user_data: *mut c_void, result: *const mln_completion_result) {
        // SAFETY: the sender that `read_state` passed, and the library's
        // result with one lent text.
        unsafe {
            let sender = &*user_data.cast::<mpsc::Sender<String>>();
            let text = *(*result).value.cast::<mln_buffer_view>();
            let bytes = core::slice::from_raw_parts(text.data.cast::<u8>(), text.size);
            let _ = sender.send(String::from_utf8(bytes.to_vec()).unwrap());
        }
    }

    fn read_state(map: mln_map, selector: &mln_feature_state_selector) -> String {
        let (sender, receiver) = mpsc::channel::<String>();
        let completion = mln_completion {
            size: size_of::<mln_completion>() as u32,
            callback: Some(send_text),
            user_data: core::ptr::from_ref(&sender).cast_mut().cast(),
            release_user_data: None,
            queue: 0,
        };
        // SAFETY: local arguments, which outlive the completion's call.
        assert_eq!(unsafe { mln_map_get_feature_state(map, selector, &completion, NO_DIAGNOSTIC) }, MLN_STATUS_OK);
        receiver.recv_timeout(std::time::Duration::from_secs(30)).unwrap()
    }

    #[test]
    fn a_snapshot_keeps_the_callers_scale_factor() {
        let runtime = create();
        let map = create_map(runtime, &options(1.1));
        let snapshot = snapshot(map);
        assert_eq!((snapshot.view.extent.width, snapshot.view.extent.height), (64, 32));
        assert_eq!(snapshot.view.extent.scale_factor, 1.1);
        assert!(snapshot.bounds.unbounded);
        assert_eq!(snapshot.event_mask, EventMask::ALL.bits());
        release_map(map);
        let mut out = diagnostic();
        // SAFETY: a null snapshot.
        assert_eq!(unsafe { mln_map_get_snapshot(map, core::ptr::null_mut(), &mut *out) }, MLN_STATUS_INVALID_ARGUMENT);
        let mut stale = snapshot;
        // SAFETY: a local snapshot.
        assert_eq!(unsafe { mln_map_get_snapshot(map, &mut stale, &mut *out) }, MLN_STATUS_INVALID_STATE);
        release(runtime);
    }

    #[test]
    fn map_options_are_checked_before_the_map_is_made() {
        let runtime = create();
        let mut out = diagnostic();
        let mut map = 0;
        let waiter = Waiter::new();
        for (options, expected) in [
            (mln_map_options { map_mode: 9, ..options(1.0) }, "map_mode"),
            (options(f64::NAN), "initial extent"),
            (mln_map_options { fields: 1 << 9, ..options(1.0) }, "mln_map_options.fields"),
            (mln_map_options { fields: MLN_MAP_OPTION_EVENT_MASK, event_mask: 1, ..options(1.0) }, "event_mask"),
            (mln_map_options { fallback_font_faces: view("{}"), ..options(1.0) }, "fallback_font_faces is invalid"),
        ] {
            // SAFETY: local arguments.
            let status = unsafe { mln_runtime_create_map(runtime, &options, &mut map, &waiter.descriptor, &mut *out) };
            assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
            assert!(message(&out).contains(expected), "{}", message(&out));
            assert_eq!(map, 0);
        }
        // SAFETY: the rejected calls left the user data with the caller.
        unsafe { (waiter.descriptor.release_user_data.unwrap())(waiter.descriptor.user_data) };
        release(runtime);
    }

    #[test]
    fn feature_state_round_trips_as_json() {
        let runtime = create();
        let map = create_map(runtime, &options(1.0));
        let id = mln_feature_id { type_: MLN_FEATURE_ID_TYPE_UINT, data: mln_feature_id_data { uint_value: 7 } };
        let numbered = selector("points", Some(id));
        let named = selector(
            "points",
            Some(mln_feature_id {
                type_: MLN_FEATURE_ID_TYPE_STRING,
                data: mln_feature_id_data { string_value: view("7") },
            }),
        );
        // Keys come back sorted.
        let state = r#"{"depth":-2.5,"hover":true,"tags":["a",1]}"#;
        // SAFETY: local arguments.
        unsafe {
            assert_eq!(
                mln_map_set_feature_state(map, &numbered, view(state), core::ptr::null(), NO_DIAGNOSTIC),
                MLN_STATUS_OK
            );
        }
        assert_eq!(read_state(map, &named), state);

        let mut out = diagnostic();
        // SAFETY: local arguments.
        unsafe {
            let array = mln_map_set_feature_state(map, &numbered, view("[1]"), core::ptr::null(), &mut *out);
            assert_eq!(array, MLN_STATUS_INVALID_ARGUMENT);
            assert!(message(&out).contains("must be a JSON object"));
            let source = selector("points", None);
            let state = mln_map_set_feature_state(map, &source, view(state), core::ptr::null(), &mut *out);
            assert_eq!(state, MLN_STATUS_INVALID_ARGUMENT);
            assert!(message(&out).contains("requires feature_id"));
            assert_eq!(mln_map_remove_feature_state(map, &source, core::ptr::null(), NO_DIAGNOSTIC), MLN_STATUS_OK);
        }
        assert_eq!(read_state(map, &named), "{}");
        release_map(map);
        release(runtime);
    }
}
