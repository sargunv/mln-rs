//! `map.h`'s offline regions: the regions in a runtime's cache database,
//! their downloads, and the events that a download reports.

use ::mln_runtime::{EventMask, Offline};
use mln_storage::offline::{
    OfflineGeometryRegionDefinition, OfflineRegion, OfflineRegionDefinition, OfflineRegionDownloadState,
    OfflineRegionStatus, OfflineTilePyramidRegionDefinition,
};
use mln_util::geo::LatLngBounds;
use mln_util::geometry::{Geometry, for_each_point};

use crate::buffer::LentText;
use crate::completion::{self, CompletionValue, Generations, PendingCompletion};
use crate::error::Error;
use crate::events::offline_status;
use crate::generated::types::{
    MLN_OFFLINE_REGION_DEFINITION_GEOMETRY, MLN_OFFLINE_REGION_DEFINITION_TILE_PYRAMID,
    MLN_OFFLINE_REGION_DOWNLOAD_ACTIVE, MLN_OFFLINE_REGION_DOWNLOAD_INACTIVE, mln_buffer_view, mln_completion,
    mln_diagnostic, mln_lat_lng_bounds, mln_offline_geometry_region_definition, mln_offline_region_definition,
    mln_offline_region_definition_data, mln_offline_region_download_state, mln_offline_region_id,
    mln_offline_region_info, mln_offline_region_status, mln_offline_tile_pyramid_region_definition, mln_runtime,
    mln_runtime_event_flags, mln_status,
};
use crate::{abi, buffer, geo, parse_thread, runtime};

abi::versioned!(mln_offline_region_definition);

/// What a region's definition covers, as a completion lends it.
enum Shape {
    TilePyramid(mln_lat_lng_bounds),
    /// The geometry as GeoJSON text.
    Geometry(LentText),
}

/// A region that a completion lends as an `mln_offline_region_info`, with the
/// text and bytes that its views point into.
struct LentRegion {
    id: mln_offline_region_id,
    shape: Shape,
    style_url: LentText,
    min_zoom: f64,
    max_zoom: f64,
    pixel_ratio: f32,
    include_ideographs: bool,
    metadata: Vec<u8>,
}

impl LentRegion {
    fn new(region: &OfflineRegion) -> Self {
        Self::from_parts(region.get_id(), region.get_definition(), region.get_metadata())
    }

    fn from_parts(id: mln_offline_region_id, definition: &OfflineRegionDefinition, metadata: &[u8]) -> Self {
        let shape = match definition {
            OfflineRegionDefinition::TilePyramid(pyramid) => Shape::TilePyramid(mln_lat_lng_bounds {
                southwest: geo::from_lat_lng(pyramid.bounds.southwest()),
                northeast: geo::from_lat_lng(pyramid.bounds.northeast()),
            }),
            OfflineRegionDefinition::Geometry(shaped) => {
                Shape::Geometry(LentText::new(&geojson::stringify_geometry(&shaped.geometry)))
            }
        };
        LentRegion {
            id,
            shape,
            style_url: LentText::new(definition.style_url()),
            min_zoom: definition.min_zoom(),
            max_zoom: definition.max_zoom(),
            pixel_ratio: definition.pixel_ratio(),
            include_ideographs: definition.include_ideographs(),
            metadata: metadata.to_vec(),
        }
    }

    /// The region's C view, valid while `self` is.
    fn info(&self) -> mln_offline_region_info {
        let (type_, data) = match &self.shape {
            Shape::TilePyramid(bounds) => (
                MLN_OFFLINE_REGION_DEFINITION_TILE_PYRAMID,
                mln_offline_region_definition_data {
                    tile_pyramid: mln_offline_tile_pyramid_region_definition {
                        style_url: self.style_url.view(),
                        bounds: *bounds,
                        min_zoom: self.min_zoom,
                        max_zoom: self.max_zoom,
                        pixel_ratio: self.pixel_ratio,
                        include_ideographs: self.include_ideographs,
                    },
                },
            ),
            Shape::Geometry(geometry) => (
                MLN_OFFLINE_REGION_DEFINITION_GEOMETRY,
                mln_offline_region_definition_data {
                    geometry: mln_offline_geometry_region_definition {
                        style_url: self.style_url.view(),
                        geometry: geometry.view(),
                        min_zoom: self.min_zoom,
                        max_zoom: self.max_zoom,
                        pixel_ratio: self.pixel_ratio,
                        include_ideographs: self.include_ideographs,
                    },
                },
            ),
        };
        mln_offline_region_info {
            id: self.id,
            definition: mln_offline_region_definition {
                size: size_of::<mln_offline_region_definition>() as u32,
                type_,
                data,
            },
            metadata: if self.metadata.is_empty() { core::ptr::null() } else { self.metadata.as_ptr() },
            metadata_size: self.metadata.len(),
        }
    }
}

/// Regions: `value` holds one `mln_offline_region_info` for each, or is null
/// for none.
struct Regions(Vec<LentRegion>);

impl Regions {
    fn new<'a>(regions: impl IntoIterator<Item = &'a OfflineRegion>) -> Self {
        Regions(regions.into_iter().map(LentRegion::new).collect())
    }
}

impl CompletionValue for Regions {
    type Item = mln_offline_region_info;

    fn lend<R>(&self, f: impl FnOnce(&[mln_offline_region_info]) -> R) -> R {
        let infos: Vec<_> = self.0.iter().map(LentRegion::info).collect();
        f(&infos)
    }
}

/// A download's progress: `value` is one `mln_offline_region_status`.
impl CompletionValue for mln_offline_region_status {
    type Item = mln_offline_region_status;

    fn lend<R>(&self, f: impl FnOnce(&[mln_offline_region_status]) -> R) -> R {
        f(core::slice::from_ref(self))
    }
}

/// Sends an operation on the runtime's offline regions, whose completion
/// receives `value` of its result. The result converts on the thread that
/// resolves it, the runtime's own.
///
/// # Safety
///
/// `completion` is null or valid for the header's use of it.
unsafe fn operation<T: Send + 'static, V: CompletionValue>(
    runtime: mln_runtime,
    completion: *const mln_completion,
    send: impl FnOnce(Offline<'_>) -> Result<::mln_runtime::Completion<T>, mln_util::Error>,
    value: impl FnOnce(T) -> V + Send + 'static,
) -> Result<(), Error> {
    let object = runtime::resolve(runtime)?;
    // SAFETY: the caller's contract.
    let pending = unsafe { PendingCompletion::new(completion) }?;
    let sent = send(object.runtime.offline())?;
    completion::forward(sent, Some(pending.accept()), move |result| {
        (result.map(value).map_err(Error::from), Generations::default())
    });
    Ok(())
}

/// A caller's metadata bytes: `size` of them at `metadata`, which may be
/// null only when `size` is 0.
///
/// # Safety
///
/// `metadata` is null or points to `size` readable bytes.
unsafe fn metadata(metadata: *const u8, size: usize) -> Result<Vec<u8>, Error> {
    let view = mln_buffer_view { data: metadata.cast(), size };
    // SAFETY: the caller's contract.
    Ok(unsafe { buffer::bytes(view, "offline region metadata") }?.to_vec())
}

/// The style URL of a caller's definition, which must not be empty.
///
/// # Safety
///
/// As for [`buffer::text`].
unsafe fn style_url(view: mln_buffer_view) -> Result<String, Error> {
    // SAFETY: the caller's contract.
    let url = unsafe { buffer::text(view, "offline region style URL") }?;
    if url.is_empty() {
        return Err(Error::invalid_argument("offline region style URL must not be empty"));
    }
    Ok(url.to_owned())
}

/// The bounds of a tile pyramid, whose southwest corner must not be north or
/// east of its northeast corner.
fn bounds(bounds: mln_lat_lng_bounds) -> Result<LatLngBounds, Error> {
    let invalid = || Error::invalid_argument("offline region bounds are invalid");
    let southwest = geo::lat_lng(bounds.southwest).map_err(|_| invalid())?;
    let northeast = geo::lat_lng(bounds.northeast).map_err(|_| invalid())?;
    if southwest.latitude() > northeast.latitude() || southwest.longitude() > northeast.longitude() {
        return Err(invalid());
    }
    Ok(LatLngBounds::hull(southwest, northeast))
}

/// The GeoJSON geometry of a geometry region, parsed on the parse thread. It
/// must have at least one coordinate.
fn geometry(json: &str) -> Result<Geometry<f64>, Error> {
    if json.is_empty() {
        return Err(Error::invalid_argument("geometry must not be empty"));
    }
    let json = json.to_owned();
    parse_thread::run(move || {
        let document: serde_json::Value = serde_json::from_str(&json)
            .map_err(|error| Error::invalid_argument(format!("geometry is not valid JSON: {error}")))?;
        let geometry = match geojson::convert(&document) {
            Ok(geojson::GeoJSON::Geometry(geometry)) => geometry,
            Ok(_) => return Err(Error::invalid_argument("geometry must contain one GeoJSON Geometry")),
            Err(error) => return Err(Error::invalid_argument(format!("geometry is invalid: {}", error.message()))),
        };
        let mut points = 0usize;
        for_each_point(&geometry, &mut |_| points += 1);
        if points == 0 {
            return Err(Error::invalid_argument("offline region geometry must contain at least one coordinate"));
        }
        Ok(geometry)
    })?
}

/// The definition that a caller's `mln_offline_region_definition` gives.
///
/// # Safety
///
/// `definition` is null or points to a readable definition of the size it
/// declares, whose member for its type is initialized and whose views are
/// valid.
unsafe fn definition(definition: *const mln_offline_region_definition) -> Result<OfflineRegionDefinition, Error> {
    // SAFETY: the caller's contract.
    let definition = unsafe { abi::read_sized(definition) }?;
    match definition.type_ {
        MLN_OFFLINE_REGION_DEFINITION_TILE_PYRAMID => {
            // SAFETY: the type names this member, which the caller's contract
            // initializes.
            let pyramid = unsafe { definition.data.tile_pyramid };
            Ok(OfflineTilePyramidRegionDefinition::new(
                // SAFETY: the caller's contract.
                unsafe { style_url(pyramid.style_url) }?,
                bounds(pyramid.bounds)?,
                pyramid.min_zoom,
                pyramid.max_zoom,
                pyramid.pixel_ratio,
                pyramid.include_ideographs,
            )?
            .into())
        }
        MLN_OFFLINE_REGION_DEFINITION_GEOMETRY => {
            // SAFETY: as above.
            let shaped = unsafe { definition.data.geometry };
            // SAFETY: the caller's contract.
            let style_url = unsafe { style_url(shaped.style_url) }?;
            // SAFETY: the caller's contract.
            let geometry = geometry(unsafe { buffer::text(shaped.geometry, "geometry") }?)?;
            Ok(OfflineGeometryRegionDefinition::new(
                style_url,
                geometry,
                shaped.min_zoom,
                shaped.max_zoom,
                shaped.pixel_ratio,
                shaped.include_ideographs,
            )?
            .into())
        }
        other => Err(Error::invalid_argument(format!("offline region definition type {other} names no type"))),
    }
}

/// `mln_runtime_create_offline_region()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_create_offline_region(
    runtime: mln_runtime,
    definition: *const mln_offline_region_definition,
    metadata: *const u8,
    metadata_size: usize,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let definition = unsafe { self::definition(definition) }?;
        // SAFETY: the caller's contract.
        let metadata = unsafe { self::metadata(metadata, metadata_size) }?;
        // SAFETY: the caller's contract.
        unsafe {
            operation(
                runtime,
                completion,
                |offline| offline.create_region(definition, metadata),
                |region| Regions::new([&region]),
            )
        }
    })
}

/// `mln_runtime_get_offline_region()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_get_offline_region(
    runtime: mln_runtime,
    region_id: mln_offline_region_id,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe {
            operation(runtime, completion, |offline| offline.region(region_id), |region| Regions::new(region.as_ref()))
        }
    })
}

/// `mln_runtime_list_offline_regions()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_list_offline_regions(
    runtime: mln_runtime,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { operation(runtime, completion, |offline| offline.regions(), |regions| Regions::new(&regions)) }
    })
}

/// `mln_runtime_merge_offline_regions()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_merge_offline_regions(
    runtime: mln_runtime,
    side_database_path: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let path = unsafe { buffer::text(side_database_path, "side_database_path") }?.to_owned();
        // SAFETY: the caller's contract.
        unsafe {
            operation(runtime, completion, |offline| offline.merge_regions(path), |regions| Regions::new(&regions))
        }
    })
}

/// `mln_runtime_update_offline_region_metadata()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_update_offline_region_metadata(
    runtime: mln_runtime,
    region_id: mln_offline_region_id,
    metadata: *const u8,
    metadata_size: usize,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let metadata = unsafe { self::metadata(metadata, metadata_size) }?;
        // SAFETY: the caller's contract.
        unsafe {
            operation(
                runtime,
                completion,
                |offline| offline.update_region_metadata(region_id, metadata),
                |region| Regions::new([&region]),
            )
        }
    })
}

/// `mln_runtime_get_offline_region_status()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_get_offline_region_status(
    runtime: mln_runtime,
    region_id: mln_offline_region_id,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe {
            operation(
                runtime,
                completion,
                |offline| offline.region_status(region_id),
                |status: OfflineRegionStatus| offline_status(&status),
            )
        }
    })
}

/// `mln_runtime_set_offline_region_observed()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_set_offline_region_observed(
    runtime: mln_runtime,
    region_id: mln_offline_region_id,
    events: mln_runtime_event_flags,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let events =
            EventMask::from_bits(events).ok_or_else(|| Error::invalid_argument("event mask contains unknown bits"))?;
        // SAFETY: the caller's contract.
        unsafe { operation(runtime, completion, |offline| offline.set_region_observed(region_id, events), |()| ()) }
    })
}

/// `mln_runtime_set_offline_region_download_state()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_set_offline_region_download_state(
    runtime: mln_runtime,
    region_id: mln_offline_region_id,
    state: mln_offline_region_download_state,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let state = match abi::enum_value(
            state,
            MLN_OFFLINE_REGION_DOWNLOAD_INACTIVE..=MLN_OFFLINE_REGION_DOWNLOAD_ACTIVE,
            "offline region download state",
        )? {
            MLN_OFFLINE_REGION_DOWNLOAD_ACTIVE => OfflineRegionDownloadState::Active,
            _ => OfflineRegionDownloadState::Inactive,
        };
        // SAFETY: the caller's contract.
        unsafe {
            operation(runtime, completion, |offline| offline.set_region_download_state(region_id, state), |()| ())
        }
    })
}

/// `mln_runtime_invalidate_offline_region()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_invalidate_offline_region(
    runtime: mln_runtime,
    region_id: mln_offline_region_id,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { operation(runtime, completion, |offline| offline.invalidate_region(region_id), |()| ()) }
    })
}

/// `mln_runtime_delete_offline_region()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_delete_offline_region(
    runtime: mln_runtime,
    region_id: mln_offline_region_id,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { operation(runtime, completion, |offline| offline.delete_region(region_id), |()| ()) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::types::mln_lat_lng;

    fn view(text: &'static str) -> mln_buffer_view {
        mln_buffer_view { data: text.as_ptr().cast(), size: text.len() }
    }

    fn pyramid() -> mln_offline_region_definition {
        mln_offline_region_definition {
            size: size_of::<mln_offline_region_definition>() as u32,
            type_: MLN_OFFLINE_REGION_DEFINITION_TILE_PYRAMID,
            data: mln_offline_region_definition_data {
                tile_pyramid: mln_offline_tile_pyramid_region_definition {
                    style_url: view("custom://style.json"),
                    bounds: mln_lat_lng_bounds {
                        southwest: mln_lat_lng { latitude: 1.0, longitude: 2.0 },
                        northeast: mln_lat_lng { latitude: 3.0, longitude: 4.0 },
                    },
                    min_zoom: 0.0,
                    max_zoom: f64::INFINITY,
                    pixel_ratio: 2.0,
                    include_ideographs: true,
                },
            },
        }
    }

    fn shaped(geometry: &'static str) -> mln_offline_region_definition {
        mln_offline_region_definition {
            type_: MLN_OFFLINE_REGION_DEFINITION_GEOMETRY,
            data: mln_offline_region_definition_data {
                geometry: mln_offline_geometry_region_definition {
                    style_url: view("custom://style.json"),
                    geometry: view(geometry),
                    min_zoom: 5.0,
                    max_zoom: 6.0,
                    pixel_ratio: 1.0,
                    include_ideographs: false,
                },
            },
            ..pyramid()
        }
    }

    fn convert(descriptor: &mln_offline_region_definition) -> Result<OfflineRegionDefinition, Error> {
        // SAFETY: a local definition whose views name static text.
        unsafe { definition(descriptor) }
    }

    fn message(descriptor: &mln_offline_region_definition) -> String {
        convert(descriptor).unwrap_err().message().to_owned()
    }

    #[test]
    fn a_region_lends_the_definition_it_was_created_with() {
        let line = r#"{"type":"LineString","coordinates":[[2,1],[4,3]]}"#;
        for descriptor in [pyramid(), shaped(line)] {
            let lent = LentRegion::from_parts(7, &convert(&descriptor).unwrap(), &[1, 2]);
            let info = lent.info();
            assert_eq!((info.id, info.metadata_size), (7, 2));
            assert_eq!(info.definition.type_, descriptor.type_);
            // SAFETY: the info's views point into `lent`, which is live, and
            // each union read is of the member that the type names.
            unsafe {
                assert_eq!(*info.metadata.add(1), 2);
                if descriptor.type_ == MLN_OFFLINE_REGION_DEFINITION_TILE_PYRAMID {
                    let (given, lent) = (descriptor.data.tile_pyramid, info.definition.data.tile_pyramid);
                    assert_eq!(buffer::text(lent.style_url, "url").unwrap(), "custom://style.json");
                    assert_eq!(lent.bounds.southwest.longitude, given.bounds.southwest.longitude);
                    assert_eq!(lent.bounds.northeast.latitude, given.bounds.northeast.latitude);
                    assert_eq!((lent.max_zoom, lent.pixel_ratio, lent.include_ideographs), (f64::INFINITY, 2.0, true));
                } else {
                    let lent = info.definition.data.geometry;
                    assert!(buffer::text(lent.geometry, "geometry").unwrap().contains("LineString"));
                    assert_eq!((lent.min_zoom, lent.max_zoom), (5.0, 6.0));
                }
            }
        }
        let empty = LentRegion::from_parts(8, &convert(&pyramid()).unwrap(), &[]);
        assert!(empty.info().metadata.is_null());
    }

    #[test]
    fn a_definition_needs_ordered_bounds_and_zooms() {
        let mut inverted = pyramid();
        let mut off_globe = pyramid();
        let mut zooms = pyramid();
        let mut ratio = pyramid();
        let mut no_style = pyramid();
        inverted.data.tile_pyramid.bounds.southwest.longitude = 5.0;
        off_globe.data.tile_pyramid.bounds.northeast.latitude = 91.0;
        zooms.data.tile_pyramid.min_zoom = 4.0;
        zooms.data.tile_pyramid.max_zoom = 2.0;
        ratio.data.tile_pyramid.pixel_ratio = f32::NAN;
        no_style.data.tile_pyramid.style_url = mln_buffer_view { data: core::ptr::null(), size: 0 };
        assert_eq!(message(&inverted), "offline region bounds are invalid");
        assert_eq!(message(&off_globe), "offline region bounds are invalid");
        assert_eq!(message(&zooms), "offline region zoom range is invalid");
        assert_eq!(message(&ratio), "offline region pixel ratio is invalid");
        assert_eq!(message(&no_style), "offline region style URL must not be empty");
        assert!(message(&mln_offline_region_definition { type_: 3, ..pyramid() }).contains("names no type"));
    }

    #[test]
    fn a_geometry_is_one_geojson_geometry_with_a_coordinate() {
        assert!(message(&shaped("not json")).starts_with("geometry is not valid JSON"));
        assert!(message(&shaped(r#"{"type":"Feature"}"#)).starts_with("geometry is invalid"));
        let feature = r#"{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[1,2]}}"#;
        assert_eq!(message(&shaped(feature)), "geometry must contain one GeoJSON Geometry");
        assert_eq!(
            message(&shaped(r#"{"type":"GeometryCollection","geometries":[]}"#)),
            "offline region geometry must contain at least one coordinate"
        );
        assert_eq!(message(&shaped("")), "geometry must not be empty");
    }
}
