//! `style.h`: the sources, layers, images, light, global state and
//! transitions of a map's style.

use core::ffi::c_void;
use std::sync::Arc;

use ::mln_runtime::{Applied, CustomSourceCallbacks, LayerInfo, SourceInfo, SourceKind};
use mln_style::TransitionOptions;
use mln_style::image::{ImageContent, Impl as StyleImage, TextFit};
use mln_style::sources::custom_geometry_source::TileOptions;
use mln_style::sources::{GeoJSONOptions, TileDataFormat};
use mln_tile_id::CanonicalTileID;
use mln_util::chrono::Duration;
use mln_util::constants;
use mln_util::geo::{LatLng, LatLngBounds};
use mln_util::image::PremultipliedImage;
use mln_util::immutable::Immutable;
use mln_util::range::Range;
use mln_util::size::Size;
use mln_util::tileset::{RasterEncoding, Scheme, VectorEncoding};

use crate::buffer::LentText;
use crate::callback::{Registration, UserData};
use crate::completion::{self, CompletionValue, PendingCompletion};
use crate::error::Error;
use crate::generated::types::{
    MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_BUFFER, MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_CLIP,
    MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_MAX_ZOOM, MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_MIN_ZOOM,
    MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_TILE_SIZE, MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_TOLERANCE,
    MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_WRAP, MLN_CUSTOM_VECTOR_SOURCE_OPTION_MAX_ZOOM,
    MLN_CUSTOM_VECTOR_SOURCE_OPTION_MIN_ZOOM, MLN_GEO_JSON_SOURCE_OPTION_BUFFER, MLN_GEO_JSON_SOURCE_OPTION_CLUSTER,
    MLN_GEO_JSON_SOURCE_OPTION_CLUSTER_MAX_ZOOM, MLN_GEO_JSON_SOURCE_OPTION_CLUSTER_MIN_POINTS,
    MLN_GEO_JSON_SOURCE_OPTION_CLUSTER_RADIUS, MLN_GEO_JSON_SOURCE_OPTION_LINE_METRICS,
    MLN_GEO_JSON_SOURCE_OPTION_MAX_ZOOM, MLN_GEO_JSON_SOURCE_OPTION_MIN_ZOOM, MLN_GEO_JSON_SOURCE_OPTION_TOLERANCE,
    MLN_STYLE_IMAGE_INFO_CONTENT, MLN_STYLE_IMAGE_INFO_TEXT_FIT_HEIGHT, MLN_STYLE_IMAGE_INFO_TEXT_FIT_WIDTH,
    MLN_STYLE_IMAGE_OPTION_CONTENT, MLN_STYLE_IMAGE_OPTION_KEEP_UNUSED, MLN_STYLE_IMAGE_OPTION_PIXEL_RATIO,
    MLN_STYLE_IMAGE_OPTION_SDF, MLN_STYLE_IMAGE_OPTION_STRETCH_X, MLN_STYLE_IMAGE_OPTION_STRETCH_Y,
    MLN_STYLE_IMAGE_OPTION_TEXT_FIT_HEIGHT, MLN_STYLE_IMAGE_OPTION_TEXT_FIT_WIDTH,
    MLN_STYLE_IMAGE_TEXT_FIT_PROPORTIONAL, MLN_STYLE_IMAGE_TEXT_FIT_STRETCH_ONLY,
    MLN_STYLE_IMAGE_TEXT_FIT_STRETCH_OR_SHRINK, MLN_STYLE_LAYER_VISIBILITY_NONE, MLN_STYLE_LAYER_VISIBILITY_VISIBLE,
    MLN_STYLE_RASTER_DEM_ENCODING_MAPBOX, MLN_STYLE_RASTER_DEM_ENCODING_TERRARIUM, MLN_STYLE_SOURCE_INFO_BOUNDS,
    MLN_STYLE_SOURCE_INFO_GEO_JSON, MLN_STYLE_SOURCE_INFO_RASTER_ENCODING, MLN_STYLE_SOURCE_INFO_TILE_JSON,
    MLN_STYLE_SOURCE_INFO_TILE_SIZE, MLN_STYLE_SOURCE_INFO_VECTOR_ENCODING, MLN_STYLE_SOURCE_TYPE_CUSTOM_GEOMETRY,
    MLN_STYLE_SOURCE_TYPE_CUSTOM_VECTOR, MLN_STYLE_SOURCE_TYPE_GEO_JSON, MLN_STYLE_SOURCE_TYPE_IMAGE,
    MLN_STYLE_SOURCE_TYPE_RASTER, MLN_STYLE_SOURCE_TYPE_RASTER_DEM, MLN_STYLE_SOURCE_TYPE_VECTOR,
    MLN_STYLE_TILE_SCHEME_TMS, MLN_STYLE_TILE_SCHEME_XYZ, MLN_STYLE_TRANSITION_OPTION_DELAY,
    MLN_STYLE_TRANSITION_OPTION_DURATION, MLN_STYLE_TRANSITION_OPTION_ENABLE_PLACEMENT_TRANSITIONS,
    MLN_STYLE_VECTOR_TILE_ENCODING_MLT, MLN_STYLE_VECTOR_TILE_ENCODING_MVT, mln_buffer_view, mln_canonical_tile_id,
    mln_completion, mln_custom_geometry_source_options, mln_custom_vector_source_options, mln_diagnostic,
    mln_geo_json_source_options, mln_image_content, mln_image_stretch, mln_lat_lng, mln_lat_lng_bounds, mln_map,
    mln_premultiplied_rgba8_image, mln_status, mln_style_image_info, mln_style_image_options, mln_style_image_text_fit,
    mln_style_layer_info, mln_style_source_info, mln_style_source_tile_info, mln_style_source_type,
    mln_style_transition_options, mln_style_vector_tile_encoding,
};
use crate::map::{self, command, query};
use crate::{abi, buffer, geo, parse_thread};

abi::versioned!(
    mln_geo_json_source_options,
    mln_custom_geometry_source_options,
    mln_custom_vector_source_options,
    mln_premultiplied_rgba8_image,
    mln_style_image_options,
    mln_style_transition_options,
);

// MARK: - Inputs

/// The text of a view that must hold at least one byte, as IDs, URLs and
/// JSON must.
///
/// # Safety
///
/// As for [`buffer::text`].
unsafe fn required_text(view: mln_buffer_view, name: &str) -> Result<String, Error> {
    if view.size == 0 {
        return Err(Error::invalid_argument(format!("{name} must not be empty")));
    }
    // SAFETY: the caller's contract.
    unsafe { buffer::text(view, name) }.map(str::to_owned)
}

/// The text of a view that may be empty, as `None` when it is.
///
/// # Safety
///
/// As for [`buffer::text`].
unsafe fn optional_text(view: mln_buffer_view, name: &str) -> Result<Option<String>, Error> {
    // SAFETY: the caller's contract.
    let text = unsafe { buffer::text(view, name) }?;
    Ok(Some(text).filter(|text| !text.is_empty()).map(str::to_owned))
}

/// Fails a command in its place among the map's commands: the call
/// succeeds, and the completion reports `error` once the commands sent
/// before it have run. For inputs that the headers say a command, rather
/// than its call, refuses.
///
/// # Safety
///
/// `completion` is null or valid for the header's use of it.
unsafe fn fail_in_order(map: mln_map, completion: *const mln_completion, error: Error) -> Result<(), Error> {
    let object = map::resolve(map)?;
    // SAFETY: the caller's contract.
    let pending = unsafe { PendingCompletion::optional(completion) }?;
    let sent = object.map.with_core(|_| ())?;
    let handle = object.map.clone();
    completion::forward(sent, pending.map(PendingCompletion::accept), move |_| {
        (Err::<(), _>(error), map::snapshot_generations(&handle))
    });
    Ok(())
}

/// Sends a command whose input `checked` holds, or fails it in order as
/// [`fail_in_order`] does.
///
/// # Safety
///
/// `completion` is null or valid for the header's use of it.
unsafe fn command_with<T>(
    map: mln_map,
    completion: *const mln_completion,
    checked: Result<T, Error>,
    send: impl FnOnce(&::mln_runtime::Map, T) -> Result<::mln_runtime::Completion<Applied>, mln_util::Error>,
) -> Result<(), Error> {
    match checked {
        // SAFETY: the caller's contract.
        Ok(value) => unsafe { command(map, completion, |map| send(map, value)) },
        // SAFETY: the caller's contract.
        Err(error) => unsafe { fail_in_order(map, completion, error) },
    }
}

/// A tile ID within its zoom level, checked in the command.
fn tile_id(tile: mln_canonical_tile_id) -> Result<CanonicalTileID, Error> {
    let z = u8::try_from(tile.z).ok().filter(|z| *z <= 32);
    match z {
        Some(z) if u64::from(tile.x) < 1u64 << z && u64::from(tile.y) < 1u64 << z => {
            Ok(CanonicalTileID::new(z, tile.x, tile.y))
        }
        _ => Err(Error::invalid_argument("tile_id must be a tile within zoom bounds, with z at most 32")),
    }
}

/// Bounds whose corners are on the globe, southwest of each other, checked
/// in the command.
fn bounds(bounds: mln_lat_lng_bounds) -> Result<LatLngBounds, Error> {
    let southwest = geo::lat_lng(bounds.southwest)?;
    let northeast = geo::lat_lng(bounds.northeast)?;
    if southwest.latitude() > northeast.latitude() || southwest.longitude() > northeast.longitude() {
        return Err(Error::invalid_argument("bounds southwest must be less than or equal to northeast"));
    }
    Ok(LatLngBounds::hull(southwest, northeast))
}

/// A copy of a caller's image, with its rows packed.
///
/// # Safety
///
/// `image` is null or points to a readable image of the size it declares,
/// whose `pixels` hold `byte_length` readable bytes.
unsafe fn image(image: *const mln_premultiplied_rgba8_image) -> Result<PremultipliedImage, Error> {
    if image.is_null() {
        return Err(Error::invalid_argument("image must not be null"));
    }
    // SAFETY: the caller's contract.
    let image = unsafe { abi::read_sized(image) }?;
    if image.width == 0 || image.height == 0 {
        return Err(Error::invalid_argument("image dimensions must be positive"));
    }
    let row = usize::try_from(image.width).ok().and_then(|width| width.checked_mul(4));
    let Some(row) = row.filter(|row| u32::try_from(*row).is_ok()) else {
        return Err(Error::invalid_argument("image row byte length overflows"));
    };
    let stride = image.stride as usize;
    if stride < row {
        return Err(Error::invalid_argument("image stride must be at least width * 4"));
    }
    if image.pixels.is_null() {
        return Err(Error::invalid_argument("image pixels must not be null"));
    }
    let required = (image.height as usize - 1).checked_mul(stride).and_then(|rows| rows.checked_add(row));
    if required.is_none_or(|required| image.byte_length < required) {
        return Err(Error::invalid_argument("image byte_length is too small"));
    }
    // SAFETY: non-null, and `byte_length` readable bytes by the caller's
    // contract.
    let pixels = unsafe { core::slice::from_raw_parts(image.pixels, image.byte_length) };
    let mut packed = Vec::with_capacity(row * image.height as usize);
    for line in pixels.chunks(stride).take(image.height as usize) {
        packed.extend_from_slice(&line[..row]);
    }
    Ok(PremultipliedImage::from_vec(Size::new(image.width, image.height), packed))
}

/// The four corners of an image source, clockwise from the top left.
///
/// # Safety
///
/// `coordinates` is null or points to `count` readable coordinates.
unsafe fn corners(coordinates: *const mln_lat_lng, count: usize) -> Result<[LatLng; 4], Error> {
    if count != 4 {
        return Err(Error::invalid_argument("image source coordinate_count must be 4"));
    }
    // SAFETY: the caller's contract.
    let corners = unsafe { geo::lat_lngs(coordinates, count) }?;
    let corners: [LatLng; 4] = corners.try_into().map_err(|_| Error::invalid_argument("coordinates must be 4"))?;
    if corners.iter().all(|corner| *corner == corners[0]) {
        return Err(Error::invalid_argument("image source coordinates must not all match"));
    }
    Ok(corners)
}

fn text_fit(value: mln_style_image_text_fit, name: &str) -> Result<TextFit, Error> {
    match value {
        MLN_STYLE_IMAGE_TEXT_FIT_STRETCH_OR_SHRINK => Ok(TextFit::StretchOrShrink),
        MLN_STYLE_IMAGE_TEXT_FIT_STRETCH_ONLY => Ok(TextFit::StretchOnly),
        MLN_STYLE_IMAGE_TEXT_FIT_PROPORTIONAL => Ok(TextFit::Proportional),
        _ => Err(Error::invalid_argument(format!("{name} is invalid"))),
    }
}

fn from_text_fit(value: Option<TextFit>) -> mln_style_image_text_fit {
    match value {
        None | Some(TextFit::StretchOrShrink) => MLN_STYLE_IMAGE_TEXT_FIT_STRETCH_OR_SHRINK,
        Some(TextFit::StretchOnly) => MLN_STYLE_IMAGE_TEXT_FIT_STRETCH_ONLY,
        Some(TextFit::Proportional) => MLN_STYLE_IMAGE_TEXT_FIT_PROPORTIONAL,
    }
}

/// The stretch intervals of one axis: finite, each wider than nothing, in
/// increasing order without overlap. The map divides by their summed width.
///
/// # Safety
///
/// `stretches` is null or points to `count` readable intervals.
unsafe fn stretches(stretches: *const mln_image_stretch, count: usize, name: &str) -> Result<Vec<(f32, f32)>, Error> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if stretches.is_null() {
        return Err(Error::invalid_argument(format!("{name} must not be null when its count is non-zero")));
    }
    // SAFETY: non-null, and the caller's contract.
    let stretches = unsafe { core::slice::from_raw_parts(stretches, count) };
    let mut last = f32::NEG_INFINITY;
    stretches
        .iter()
        .map(|stretch| {
            if !stretch.from.is_finite() || !stretch.to.is_finite() {
                return Err(Error::invalid_argument(format!("{name} intervals must be finite")));
            }
            if stretch.from >= stretch.to {
                return Err(Error::invalid_argument(format!("{name} intervals must have a positive width")));
            }
            if stretch.from < last {
                return Err(Error::invalid_argument(format!("{name} intervals must increase and must not overlap")));
            }
            last = stretch.to;
            Ok((stretch.from, stretch.to))
        })
        .collect()
}

/// A style image named `id` from a caller's image and options.
///
/// # Safety
///
/// `pixels` and `options` are null or valid for the header's use of them.
unsafe fn style_image(
    id: String,
    pixels: *const mln_premultiplied_rgba8_image,
    options: *const mln_style_image_options,
) -> Result<StyleImage, Error> {
    // SAFETY: the caller's contract.
    let pixels = unsafe { image(pixels) }?;
    let options = if options.is_null() {
        None
    } else {
        // SAFETY: the caller's contract.
        Some(unsafe { abi::read_sized(options) }?)
    };
    let known = MLN_STYLE_IMAGE_OPTION_PIXEL_RATIO
        | MLN_STYLE_IMAGE_OPTION_SDF
        | MLN_STYLE_IMAGE_OPTION_STRETCH_X
        | MLN_STYLE_IMAGE_OPTION_STRETCH_Y
        | MLN_STYLE_IMAGE_OPTION_CONTENT
        | MLN_STYLE_IMAGE_OPTION_TEXT_FIT_WIDTH
        | MLN_STYLE_IMAGE_OPTION_TEXT_FIT_HEIGHT
        | MLN_STYLE_IMAGE_OPTION_KEEP_UNUSED;
    let fields =
        abi::Fields::new(options.map_or(0, |options| options.fields), known, "mln_style_image_options.fields")?;
    let Some(options) = options else {
        return StyleImage::new(id, pixels, 1.0, false, Vec::new(), Vec::new(), None, None, None).map_err(Error::from);
    };
    let pixel_ratio = fields.get(MLN_STYLE_IMAGE_OPTION_PIXEL_RATIO, options.pixel_ratio).unwrap_or(1.0);
    if !pixel_ratio.is_finite() || pixel_ratio <= 0.0 {
        return Err(Error::invalid_argument("pixel_ratio must be finite and positive"));
    }
    // SAFETY: the caller's contract, for the intervals the fields name.
    let (stretch_x, stretch_y) = unsafe {
        (
            match fields.get(MLN_STYLE_IMAGE_OPTION_STRETCH_X, ()) {
                Some(()) => stretches(options.stretch_x, options.stretch_x_count, "stretch_x")?,
                None => Vec::new(),
            },
            match fields.get(MLN_STYLE_IMAGE_OPTION_STRETCH_Y, ()) {
                Some(()) => stretches(options.stretch_y, options.stretch_y_count, "stretch_y")?,
                None => Vec::new(),
            },
        )
    };
    let content = match fields.get(MLN_STYLE_IMAGE_OPTION_CONTENT, options.content) {
        None => None,
        Some(content) => {
            let insets = [content.left, content.top, content.right, content.bottom];
            if !insets.iter().all(|inset| inset.is_finite()) {
                return Err(Error::invalid_argument("content insets must be finite"));
            }
            if content.left > content.right || content.top > content.bottom {
                return Err(Error::invalid_argument("content insets must not run backwards"));
            }
            Some(ImageContent { left: content.left, top: content.top, right: content.right, bottom: content.bottom })
        }
    };
    let text_fit_width = fields
        .get(MLN_STYLE_IMAGE_OPTION_TEXT_FIT_WIDTH, options.text_fit_width)
        .map(|fit| text_fit(fit, "text_fit_width"))
        .transpose()?;
    let text_fit_height = fields
        .get(MLN_STYLE_IMAGE_OPTION_TEXT_FIT_HEIGHT, options.text_fit_height)
        .map(|fit| text_fit(fit, "text_fit_height"))
        .transpose()?;
    let sdf = fields.get(MLN_STYLE_IMAGE_OPTION_SDF, options.sdf).unwrap_or(false);
    let mut image =
        StyleImage::new(id, pixels, pixel_ratio, sdf, stretch_x, stretch_y, content, text_fit_width, text_fit_height)?;
    image.keep_unused = fields.get(MLN_STYLE_IMAGE_OPTION_KEEP_UNUSED, options.keep_unused).unwrap_or(false);
    Ok(image)
}

/// A whole zoom level from 0 to `maximum`.
fn whole_zoom(zoom: f64, maximum: u8, name: &str) -> Result<u8, Error> {
    if zoom.is_finite() && (0.0..=f64::from(maximum)).contains(&zoom) && zoom.fract() == 0.0 {
        Ok(zoom as u8)
    } else {
        Err(Error::invalid_argument(format!("{name} must be an integer within [0, {maximum}]")))
    }
}

fn zoom_range(min_zoom: Option<f64>, max_zoom: Option<f64>, maximum: u8) -> Result<Range<u8>, Error> {
    let max_zoom = max_zoom.map_or(Ok(18), |zoom| whole_zoom(zoom, maximum, "max_zoom"))?;
    let min_zoom = min_zoom.map_or(Ok(0), |zoom| whole_zoom(zoom, maximum, "min_zoom"))?;
    if min_zoom > max_zoom {
        return Err(Error::invalid_argument("min_zoom must be less than or equal to max_zoom"));
    }
    Ok(Range::new(min_zoom, max_zoom))
}

fn tolerance(tolerance: f64) -> Result<f64, Error> {
    if tolerance.is_finite() && tolerance >= 0.0 {
        Ok(tolerance)
    } else {
        Err(Error::invalid_argument("tolerance must be finite and non-negative"))
    }
}

/// The options of a GeoJSON source, with the defaults for the fields that
/// `options` leaves unset, or for all of them when it is null. The cluster
/// properties parse on the parse thread.
///
/// # Safety
///
/// `options` is null or valid for the header's use of it.
unsafe fn geojson_options(options: *const mln_geo_json_source_options) -> Result<GeoJSONOptions, Error> {
    if options.is_null() {
        return Ok(GeoJSONOptions::default());
    }
    // SAFETY: the caller's contract.
    let options = unsafe { abi::read_sized(options) }?;
    let known = MLN_GEO_JSON_SOURCE_OPTION_MIN_ZOOM
        | MLN_GEO_JSON_SOURCE_OPTION_MAX_ZOOM
        | MLN_GEO_JSON_SOURCE_OPTION_TOLERANCE
        | MLN_GEO_JSON_SOURCE_OPTION_CLUSTER_MAX_ZOOM
        | MLN_GEO_JSON_SOURCE_OPTION_BUFFER
        | MLN_GEO_JSON_SOURCE_OPTION_CLUSTER_RADIUS
        | MLN_GEO_JSON_SOURCE_OPTION_CLUSTER_MIN_POINTS
        | MLN_GEO_JSON_SOURCE_OPTION_LINE_METRICS
        | MLN_GEO_JSON_SOURCE_OPTION_CLUSTER;
    let fields = abi::Fields::new(options.fields, known, "mln_geo_json_source_options.fields")?;
    let mut result = GeoJSONOptions::default();
    // Tiling stops at zoom 24, and a cluster ID holds its zoom level plus one
    // in five bits.
    if let Some(zoom) = fields.get(MLN_GEO_JSON_SOURCE_OPTION_MAX_ZOOM, options.max_zoom) {
        result.maxzoom = whole_zoom(zoom, 24, "max_zoom")?;
    }
    if let Some(zoom) = fields.get(MLN_GEO_JSON_SOURCE_OPTION_MIN_ZOOM, options.min_zoom) {
        result.minzoom = whole_zoom(zoom, 24, "min_zoom")?;
    }
    if let Some(zoom) = fields.get(MLN_GEO_JSON_SOURCE_OPTION_CLUSTER_MAX_ZOOM, options.cluster_max_zoom) {
        result.cluster_max_zoom = whole_zoom(zoom, 30, "cluster_max_zoom")?;
    }
    if result.minzoom > result.maxzoom {
        return Err(Error::invalid_argument("min_zoom must be less than or equal to max_zoom"));
    }
    if let Some(value) = fields.get(MLN_GEO_JSON_SOURCE_OPTION_TOLERANCE, options.tolerance) {
        result.tolerance = tolerance(value)?;
    }
    if let Some(buffer) = fields.get(MLN_GEO_JSON_SOURCE_OPTION_BUFFER, options.buffer) {
        result.buffer = u16::try_from(buffer)
            .ok()
            .filter(|buffer| *buffer <= 512)
            .ok_or_else(|| Error::invalid_argument("buffer must be at most 512"))?;
    }
    if let Some(radius) = fields.get(MLN_GEO_JSON_SOURCE_OPTION_CLUSTER_RADIUS, options.cluster_radius) {
        result.cluster_radius = u16::try_from(radius)
            .ok()
            .filter(|radius| *radius <= 4095)
            .ok_or_else(|| Error::invalid_argument("cluster_radius must be at most 4095"))?;
    }
    if let Some(points) = fields.get(MLN_GEO_JSON_SOURCE_OPTION_CLUSTER_MIN_POINTS, options.cluster_min_points) {
        result.cluster_min_points = usize::try_from(points)
            .map_err(|_| Error::invalid_argument(format!("cluster_min_points must be at most {}", usize::MAX)))?;
    }
    result.line_metrics = fields.get(MLN_GEO_JSON_SOURCE_OPTION_LINE_METRICS, options.line_metrics).unwrap_or(false);
    result.cluster = fields.get(MLN_GEO_JSON_SOURCE_OPTION_CLUSTER, options.cluster).unwrap_or(false);
    // SAFETY: the caller's contract.
    let properties = unsafe { buffer::text(options.cluster_properties, "cluster_properties") }?;
    if !properties.is_empty() {
        let json = properties.to_owned();
        result.cluster_properties = parse_thread::run(move || parse_cluster_properties(&json))??;
    }
    Ok(result)
}

/// Cluster properties parse through the style's conversion of a GeoJSON
/// source, as its one member.
fn parse_cluster_properties(json: &str) -> Result<mln_style::sources::geojson_source::ClusterProperties, Error> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| Error::invalid_argument(format!("cluster_properties is not valid JSON: {error}")))?;
    if !value.is_object() {
        return Err(Error::invalid_argument("cluster_properties must contain a JSON object"));
    }
    let wrapper = serde_json::json!({ "clusterProperties": value });
    let mut error = mln_conversion::Error::default();
    let options = <GeoJSONOptions as mln_style::conversion::Converter>::convert(
        &mln_conversion::Convertible::from(&wrapper),
        &mut error,
    )
    .ok_or_else(|| Error::invalid_argument(format!("GeoJSON source options: {}", error.message)))?;
    Ok(options.cluster_properties)
}

// MARK: - Results

/// One item, or none.
struct Plain<T>(Vec<T>);

impl<T: Send + 'static> CompletionValue for Plain<T> {
    type Item = T;

    fn lend<R>(&self, f: impl FnOnce(&[T]) -> R) -> R {
        f(&self.0)
    }
}

const fn source_type(kind: SourceKind) -> mln_style_source_type {
    match kind {
        SourceKind::Vector => MLN_STYLE_SOURCE_TYPE_VECTOR,
        SourceKind::Raster => MLN_STYLE_SOURCE_TYPE_RASTER,
        SourceKind::RasterDem => MLN_STYLE_SOURCE_TYPE_RASTER_DEM,
        SourceKind::GeoJson => MLN_STYLE_SOURCE_TYPE_GEO_JSON,
        SourceKind::Image => MLN_STYLE_SOURCE_TYPE_IMAGE,
        SourceKind::CustomGeometry => MLN_STYLE_SOURCE_TYPE_CUSTOM_GEOMETRY,
        SourceKind::CustomVector => MLN_STYLE_SOURCE_TYPE_CUSTOM_VECTOR,
    }
}

const fn vector_encoding(encoding: VectorEncoding) -> mln_style_vector_tile_encoding {
    match encoding {
        VectorEncoding::Mapbox => MLN_STYLE_VECTOR_TILE_ENCODING_MVT,
        VectorEncoding::MLT => MLN_STYLE_VECTOR_TILE_ENCODING_MLT,
    }
}

/// A GeoJSON source's options with every field set.
fn from_geojson_options(options: &GeoJSONOptions, cluster_properties: mln_buffer_view) -> mln_geo_json_source_options {
    mln_geo_json_source_options {
        size: size_of::<mln_geo_json_source_options>() as u32,
        fields: MLN_GEO_JSON_SOURCE_OPTION_MIN_ZOOM
            | MLN_GEO_JSON_SOURCE_OPTION_MAX_ZOOM
            | MLN_GEO_JSON_SOURCE_OPTION_TOLERANCE
            | MLN_GEO_JSON_SOURCE_OPTION_CLUSTER_MAX_ZOOM
            | MLN_GEO_JSON_SOURCE_OPTION_BUFFER
            | MLN_GEO_JSON_SOURCE_OPTION_CLUSTER_RADIUS
            | MLN_GEO_JSON_SOURCE_OPTION_CLUSTER_MIN_POINTS
            | MLN_GEO_JSON_SOURCE_OPTION_LINE_METRICS
            | MLN_GEO_JSON_SOURCE_OPTION_CLUSTER,
        min_zoom: options.minzoom.into(),
        max_zoom: options.maxzoom.into(),
        tolerance: options.tolerance,
        cluster_max_zoom: options.cluster_max_zoom.into(),
        cluster_properties,
        buffer: options.buffer.into(),
        cluster_radius: options.cluster_radius.into(),
        cluster_min_points: options.cluster_min_points as u64,
        line_metrics: options.line_metrics,
        cluster: options.cluster,
    }
}

/// A source and the text its `mln_style_source_info` lends.
struct LentSource {
    info: SourceInfo,
    id: LentText,
    attribution: LentText,
    url: LentText,
    tile_urls: Vec<LentText>,
    cluster_properties: LentText,
}

impl LentSource {
    fn new(info: SourceInfo) -> Self {
        Self {
            id: LentText::new(&info.id),
            attribution: LentText::new(info.attribution.as_deref().unwrap_or_default()),
            url: LentText::new(info.url.as_deref().unwrap_or_default()),
            tile_urls: info.tiles.iter().flat_map(|tiles| &tiles.urls).map(|url| LentText::new(url)).collect(),
            cluster_properties: LentText::new(
                info.geojson.as_ref().map(|geojson| geojson.cluster_properties.as_str()).unwrap_or_default(),
            ),
            info,
        }
    }

    /// The C view, valid while `self` and `tile_urls`, the views of its
    /// tile URLs, are.
    fn item(&self, tile_urls: &[mln_buffer_view]) -> mln_style_source_info {
        let info = &self.info;
        let mut fields = 0;
        let mut tile_json = mln_style_source_tile_info {
            tile_urls: core::ptr::null(),
            tile_url_count: 0,
            min_zoom: 0.0,
            max_zoom: 0.0,
            scheme: MLN_STYLE_TILE_SCHEME_XYZ,
        };
        let mut area = mln_lat_lng_bounds {
            southwest: mln_lat_lng { latitude: 0.0, longitude: 0.0 },
            northeast: mln_lat_lng { latitude: 0.0, longitude: 0.0 },
        };
        let mut raster_encoding = MLN_STYLE_RASTER_DEM_ENCODING_MAPBOX;
        if let Some(tiles) = &info.tiles {
            fields |= MLN_STYLE_SOURCE_INFO_TILE_JSON;
            tile_json = mln_style_source_tile_info {
                tile_urls: if tile_urls.is_empty() { core::ptr::null() } else { tile_urls.as_ptr() },
                tile_url_count: tile_urls.len(),
                min_zoom: tiles.zoom_range.min.into(),
                max_zoom: tiles.zoom_range.max.into(),
                scheme: match tiles.scheme {
                    Scheme::XYZ => MLN_STYLE_TILE_SCHEME_XYZ,
                    Scheme::TMS => MLN_STYLE_TILE_SCHEME_TMS,
                },
            };
            if let Some(bounds) = tiles.bounds {
                fields |= MLN_STYLE_SOURCE_INFO_BOUNDS;
                area = mln_lat_lng_bounds {
                    southwest: geo::from_lat_lng(bounds.southwest()),
                    northeast: geo::from_lat_lng(bounds.northeast()),
                };
            }
            if let Some(encoding) = tiles.raster_encoding {
                fields |= MLN_STYLE_SOURCE_INFO_RASTER_ENCODING;
                raster_encoding = match encoding {
                    RasterEncoding::Mapbox => MLN_STYLE_RASTER_DEM_ENCODING_MAPBOX,
                    RasterEncoding::Terrarium => MLN_STYLE_RASTER_DEM_ENCODING_TERRARIUM,
                };
            }
        }
        if info.tile_size.is_some() {
            fields |= MLN_STYLE_SOURCE_INFO_TILE_SIZE;
        }
        if info.vector_encoding.is_some() {
            fields |= MLN_STYLE_SOURCE_INFO_VECTOR_ENCODING;
        }
        let geo_json = match &info.geojson {
            Some(geojson) => {
                fields |= MLN_STYLE_SOURCE_INFO_GEO_JSON;
                from_geojson_options(&geojson.options, self.cluster_properties.view())
            }
            // SAFETY: zeros are valid options, which report no fields.
            None => unsafe { core::mem::zeroed() },
        };
        mln_style_source_info {
            id: self.id.view(),
            type_: source_type(info.kind),
            fields,
            is_volatile: info.is_volatile,
            attribution: self.attribution.view(),
            url: self.url.view(),
            tile_json,
            bounds: area,
            tile_size: info.tile_size.map_or(0, u32::from),
            vector_encoding: info.vector_encoding.map_or(MLN_STYLE_VECTOR_TILE_ENCODING_MVT, vector_encoding),
            raster_encoding,
            geo_json,
        }
    }
}

struct LentSources(Vec<LentSource>);

impl CompletionValue for LentSources {
    type Item = mln_style_source_info;

    fn lend<R>(&self, f: impl FnOnce(&[mln_style_source_info]) -> R) -> R {
        let urls: Vec<Vec<mln_buffer_view>> =
            self.0.iter().map(|source| source.tile_urls.iter().map(LentText::view).collect()).collect();
        let items: Vec<_> = self.0.iter().zip(&urls).map(|(source, urls)| source.item(urls)).collect();
        f(&items)
    }
}

/// A layer and the text its `mln_style_layer_info` lends.
struct LentLayer {
    info: LayerInfo,
    id: LentText,
    layer_type: LentText,
    source_id: LentText,
    source_layer: LentText,
}

impl LentLayer {
    fn new(info: LayerInfo) -> Self {
        Self {
            id: LentText::new(&info.id),
            layer_type: LentText::new(info.layer_type),
            source_id: LentText::new(&info.source_id),
            source_layer: LentText::new(&info.source_layer),
            info,
        }
    }

    fn item(&self) -> mln_style_layer_info {
        mln_style_layer_info {
            id: self.id.view(),
            type_: self.layer_type.view(),
            source_id: self.source_id.view(),
            source_layer: self.source_layer.view(),
            min_zoom: self.info.min_zoom,
            max_zoom: self.info.max_zoom,
            visibility: if self.info.visible {
                MLN_STYLE_LAYER_VISIBILITY_VISIBLE
            } else {
                MLN_STYLE_LAYER_VISIBILITY_NONE
            },
        }
    }
}

struct LentLayers(Vec<LentLayer>);

impl CompletionValue for LentLayers {
    type Item = mln_style_layer_info;

    fn lend<R>(&self, f: impl FnOnce(&[mln_style_layer_info]) -> R) -> R {
        let items: Vec<_> = self.0.iter().map(LentLayer::item).collect();
        f(&items)
    }
}

/// A style image, whose pixels and intervals its `mln_style_image_info`
/// lends.
struct LentImage {
    image: Immutable<StyleImage>,
    stretch_x: Vec<mln_image_stretch>,
    stretch_y: Vec<mln_image_stretch>,
}

fn lend_stretches(stretches: &[mln_image_stretch]) -> *const mln_image_stretch {
    if stretches.is_empty() { core::ptr::null() } else { stretches.as_ptr() }
}

impl LentImage {
    fn new(image: Immutable<StyleImage>) -> Self {
        let intervals = |stretches: &[(f32, f32)]| {
            stretches.iter().map(|&(from, to)| mln_image_stretch { from, to }).collect::<Vec<_>>()
        };
        Self { stretch_x: intervals(&image.stretch_x), stretch_y: intervals(&image.stretch_y), image }
    }

    fn item(&self) -> mln_style_image_info {
        let image = &self.image;
        let fields = image.content.map_or(0, |_| MLN_STYLE_IMAGE_INFO_CONTENT)
            | image.text_fit_width.map_or(0, |_| MLN_STYLE_IMAGE_INFO_TEXT_FIT_WIDTH)
            | image.text_fit_height.map_or(0, |_| MLN_STYLE_IMAGE_INFO_TEXT_FIT_HEIGHT);
        let content = image.content.unwrap_or_default();
        mln_style_image_info {
            width: image.image.size.width,
            height: image.image.size.height,
            fields,
            pixels: mln_buffer_view { data: image.image.data.as_ptr().cast(), size: image.image.data.len() },
            stretch_x: lend_stretches(&self.stretch_x),
            stretch_x_count: self.stretch_x.len(),
            stretch_y: lend_stretches(&self.stretch_y),
            stretch_y_count: self.stretch_y.len(),
            content: mln_image_content {
                left: content.left,
                top: content.top,
                right: content.right,
                bottom: content.bottom,
            },
            text_fit_width: from_text_fit(image.text_fit_width),
            text_fit_height: from_text_fit(image.text_fit_height),
            pixel_ratio: image.pixel_ratio,
            sdf: image.sdf,
            keep_unused: image.keep_unused,
        }
    }
}

struct LentImages(Option<LentImage>);

impl CompletionValue for LentImages {
    type Item = mln_style_image_info;

    fn lend<R>(&self, f: impl FnOnce(&[mln_style_image_info]) -> R) -> R {
        let items: Vec<_> = self.0.iter().map(LentImage::item).collect();
        f(&items)
    }
}

/// Text that a query may have none of.
struct OptionalText(Option<LentText>);

impl CompletionValue for OptionalText {
    type Item = mln_buffer_view;

    fn lend<R>(&self, f: impl FnOnce(&[mln_buffer_view]) -> R) -> R {
        match &self.0 {
            Some(text) => f(&[text.view()]),
            None => f(&[]),
        }
    }
}

fn optional_text_value(text: Option<String>) -> OptionalText {
    OptionalText(text.as_deref().map(LentText::new))
}

// MARK: - Global state

/// `mln_map_set_global_state_property()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_global_state_property(
    map: mln_map,
    property_name: mln_buffer_view,
    value: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each view.
        let (name, value) =
            unsafe { (buffer::text(property_name, "property_name")?.to_owned(), required_text(value, "value")?) };
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_global_state_property(name, value)) }
    })
}

/// `mln_map_get_global_state()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_get_global_state(
    map: mln_map,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe { query(map, completion, ::mln_runtime::Map::global_state, |json| LentText::new(&json)) }
    })
}

// MARK: - Sources

/// `mln_map_add_style_source_json()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_add_style_source_json(
    map: mln_map,
    source_id: mln_buffer_view,
    source_json: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each view.
        let (id, json) =
            unsafe { (required_text(source_id, "source_id")?, required_text(source_json, "source_json")?) };
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.add_style_source_json(id, json)) }
    })
}

/// `mln_map_remove_style_source()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_remove_style_source(
    map: mln_map,
    source_id: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(source_id, "source_id") }?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.remove_style_source(id)) }
    })
}

/// `mln_map_get_style_source()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_get_style_source(
    map: mln_map,
    source_id: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(source_id, "source_id") }?;
        // SAFETY: the caller's contract.
        unsafe {
            query(
                map,
                completion,
                |map| map.style_source(id),
                |info| LentSources(info.into_iter().map(LentSource::new).collect()),
            )
        }
    })
}

/// `mln_map_set_style_source_volatile()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_style_source_volatile(
    map: mln_map,
    source_id: mln_buffer_view,
    is_volatile: bool,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(source_id, "source_id") }?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_style_source_volatile(id, is_volatile)) }
    })
}

/// `mln_map_list_style_sources()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_list_style_sources(
    map: mln_map,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe {
            query(map, completion, ::mln_runtime::Map::style_sources, |sources| {
                LentSources(sources.into_iter().map(LentSource::new).collect())
            })
        }
    })
}

// MARK: - GeoJSON sources

/// `mln_map_add_geo_json_source()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_add_geo_json_source(
    map: mln_map,
    source_id: mln_buffer_view,
    options: *const mln_geo_json_source_options,
    attribution: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each input.
        let (id, attribution, mut options) = unsafe {
            (
                required_text(source_id, "source_id")?,
                optional_text(attribution, "attribution")?,
                geojson_options(options)?,
            )
        };
        options.attribution = attribution;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.add_geojson_source(id, options)) }
    })
}

/// `mln_map_set_geo_json_source_url()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_geo_json_source_url(
    map: mln_map,
    source_id: mln_buffer_view,
    url: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each view.
        let (id, url) = unsafe { (required_text(source_id, "source_id")?, required_text(url, "url")?) };
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_geojson_source_url(id, url)) }
    })
}

/// `mln_map_set_geo_json_source_data()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_geo_json_source_data(
    map: mln_map,
    source_id: mln_buffer_view,
    data: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each view.
        let id = unsafe { required_text(source_id, "source_id") }?;
        if data.size == 0 {
            return Err(Error::invalid_argument("data must not be empty"));
        }
        // SAFETY: the caller's contract.
        let bytes = unsafe { buffer::bytes(data, "data") }?;
        // Text that isn't UTF-8 isn't JSON, which the command reports.
        let checked = core::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|error| Error::invalid_argument(format!("GeoJSON data is not valid JSON: {error}")));
        // SAFETY: the caller's contract.
        unsafe { command_with(map, completion, checked, |map, json| map.set_geojson_source_data(id, json)) }
    })
}

/// `mln_map_set_geo_json_source_synchronous_tiling()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_geo_json_source_synchronous_tiling(
    map: mln_map,
    source_id: mln_buffer_view,
    enabled: bool,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(source_id, "source_id") }?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_geojson_source_synchronous_tiling(id, enabled)) }
    })
}

// MARK: - Custom sources

type TileCallback = unsafe extern "C" fn(*mut c_void, mln_map, mln_buffer_view, mln_canonical_tile_id);

/// The host's tile callbacks of one custom source, and the map and source
/// they name. Its registration releases the user data once the map lets go
/// of the source.
struct TileCallbacks {
    registration: Registration,
    fetch_tile: TileCallback,
    cancel_tile: Option<TileCallback>,
    map: mln_map,
    source_id: Arc<LentText>,
}

impl TileCallbacks {
    fn call(&self, callback: TileCallback, tile: CanonicalTileID) {
        let (map, source_id) = (self.map, Arc::clone(&self.source_id));
        let tile = mln_canonical_tile_id { z: tile.z.into(), x: tile.x, y: tile.y };
        // A released queue refuses the call, which then never happens.
        let _ = self.registration.deliver(move |user_data: UserData| {
            // SAFETY: the host's tile callback with its user data, and a
            // source ID that stays valid until it returns.
            unsafe { callback(user_data.get(), map, source_id.view(), tile) };
        });
    }

    /// The runtime's callbacks over `callbacks`, which hold it until the map
    /// drops them.
    fn into_runtime(callbacks: &Arc<Self>) -> CustomSourceCallbacks {
        let fetch = Arc::clone(callbacks);
        let cancel = callbacks.cancel_tile.map(|cancel_tile| {
            let callbacks = Arc::clone(callbacks);
            Box::new(move |tile| callbacks.call(cancel_tile, tile)) as Box<dyn Fn(CanonicalTileID) + Send + Sync>
        });
        CustomSourceCallbacks {
            fetch_tile: Box::new(move |tile| fetch.call(fetch.fetch_tile, tile)),
            cancel_tile: cancel,
        }
    }
}

/// Sends the command that adds a custom source with `callbacks`. The user
/// data belongs to the library from the moment the command could run, and
/// returns to the caller when it can't be sent.
fn add_callback_source(
    callbacks: TileCallbacks,
    send: impl FnOnce(CustomSourceCallbacks) -> Result<(), Error>,
) -> Result<(), Error> {
    let callbacks = Arc::new(callbacks);
    callbacks.registration.accept();
    let sent = send(TileCallbacks::into_runtime(&callbacks));
    if sent.is_err() {
        // This handle outlives the copies that the failed call dropped, so
        // the registration drops unaccepted.
        callbacks.registration.revoke();
    }
    sent
}

/// `mln_map_add_custom_geometry_source()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_add_custom_geometry_source(
    map: mln_map,
    source_id: mln_buffer_view,
    options: *const mln_custom_geometry_source_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(source_id, "source_id") }?;
        if options.is_null() {
            return Err(Error::invalid_argument("options must not be null"));
        }
        // SAFETY: the caller's contract.
        let options = unsafe { abi::read_sized(options) }?;
        let known = MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_MIN_ZOOM
            | MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_MAX_ZOOM
            | MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_TOLERANCE
            | MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_TILE_SIZE
            | MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_BUFFER
            | MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_CLIP
            | MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_WRAP;
        let fields = abi::Fields::new(options.fields, known, "mln_custom_geometry_source_options.fields")?;
        let zooms = zoom_range(
            fields.get(MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_MIN_ZOOM, options.min_zoom),
            fields.get(MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_MAX_ZOOM, options.max_zoom),
            32,
        )?;
        let defaults = TileOptions::default();
        let tolerance = tolerance(
            fields.get(MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_TOLERANCE, options.tolerance).unwrap_or(defaults.tolerance),
        )?;
        // The map scales a tile's geometry to the tile extent by the whole
        // number extent / tile size, and keeps the scaled buffer in 16 bits.
        let extent = constants::EXTENT as u32;
        let tile_size = fields
            .get(MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_TILE_SIZE, options.tile_size)
            .unwrap_or(defaults.tile_size.into());
        if !tile_size.is_power_of_two() || tile_size > extent {
            return Err(Error::invalid_argument("tile_size must be a power of two within [1, 8192]"));
        }
        let max_buffer = u32::from(u16::MAX) / (extent / tile_size);
        let buffer =
            fields.get(MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_BUFFER, options.buffer).unwrap_or(defaults.buffer.into());
        if buffer > max_buffer {
            return Err(Error::invalid_argument(format!(
                "buffer must be at most {max_buffer} for a tile_size of {tile_size}"
            )));
        }
        let tile_options = TileOptions {
            tolerance,
            tile_size: tile_size as u16,
            buffer: buffer as u16,
            clip: fields.get(MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_CLIP, options.clip).unwrap_or(defaults.clip),
            wrap: fields.get(MLN_CUSTOM_GEOMETRY_SOURCE_OPTION_WRAP, options.wrap).unwrap_or(defaults.wrap),
        };
        let Some(fetch_tile) = options.fetch_tile else {
            return Err(Error::invalid_argument("custom source requires a callback"));
        };
        let registration =
            Registration::new("custom source", true, options.user_data, options.release_user_data, options.queue)?;
        let callbacks = TileCallbacks {
            registration,
            fetch_tile,
            cancel_tile: options.cancel_tile,
            map,
            source_id: Arc::new(LentText::new(&id)),
        };
        add_callback_source(callbacks, |callbacks| {
            // SAFETY: the caller's contract.
            unsafe {
                command(map, completion, |map| map.add_custom_geometry_source(id, callbacks, zooms, tile_options))
            }
        })
    })
}

/// `mln_map_set_custom_geometry_source_tile_data()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_custom_geometry_source_tile_data(
    map: mln_map,
    source_id: mln_buffer_view,
    tile_id: mln_canonical_tile_id,
    data: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each view.
        let (id, json) = unsafe { (required_text(source_id, "source_id")?, required_text(data, "data")?) };
        // SAFETY: the caller's contract.
        unsafe {
            command_with(map, completion, self::tile_id(tile_id), |map, tile| {
                map.set_custom_geometry_source_tile_data(id, tile, json)
            })
        }
    })
}

/// `mln_map_set_custom_geometry_source_tile_error()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_custom_geometry_source_tile_error(
    map: mln_map,
    source_id: mln_buffer_view,
    tile_id: mln_canonical_tile_id,
    message: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each view.
        let (id, message) =
            unsafe { (required_text(source_id, "source_id")?, buffer::text(message, "message")?.to_owned()) };
        // SAFETY: the caller's contract.
        unsafe {
            command_with(map, completion, self::tile_id(tile_id), |map, tile| {
                map.set_custom_geometry_source_tile_error(id, tile, message)
            })
        }
    })
}

/// `mln_map_invalidate_custom_geometry_source_tile()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_invalidate_custom_geometry_source_tile(
    map: mln_map,
    source_id: mln_buffer_view,
    tile_id: mln_canonical_tile_id,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(source_id, "source_id") }?;
        // SAFETY: the caller's contract.
        unsafe {
            command_with(map, completion, self::tile_id(tile_id), |map, tile| {
                map.invalidate_custom_geometry_source_tile(id, tile)
            })
        }
    })
}

/// `mln_map_invalidate_custom_geometry_source_region()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_invalidate_custom_geometry_source_region(
    map: mln_map,
    source_id: mln_buffer_view,
    bounds: mln_lat_lng_bounds,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(source_id, "source_id") }?;
        // SAFETY: the caller's contract.
        unsafe {
            command_with(map, completion, self::bounds(bounds), |map, bounds| {
                map.invalidate_custom_geometry_source_region(id, bounds)
            })
        }
    })
}

/// `mln_map_add_custom_vector_source()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_add_custom_vector_source(
    map: mln_map,
    source_id: mln_buffer_view,
    options: *const mln_custom_vector_source_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(source_id, "source_id") }?;
        if options.is_null() {
            return Err(Error::invalid_argument("options must not be null"));
        }
        // SAFETY: the caller's contract.
        let options = unsafe { abi::read_sized(options) }?;
        let known = MLN_CUSTOM_VECTOR_SOURCE_OPTION_MIN_ZOOM | MLN_CUSTOM_VECTOR_SOURCE_OPTION_MAX_ZOOM;
        let fields = abi::Fields::new(options.fields, known, "mln_custom_vector_source_options.fields")?;
        let zooms = zoom_range(
            fields.get(MLN_CUSTOM_VECTOR_SOURCE_OPTION_MIN_ZOOM, options.min_zoom),
            fields.get(MLN_CUSTOM_VECTOR_SOURCE_OPTION_MAX_ZOOM, options.max_zoom),
            32,
        )?;
        let Some(fetch_tile) = options.fetch_tile else {
            return Err(Error::invalid_argument("custom source requires a callback"));
        };
        let registration =
            Registration::new("custom source", true, options.user_data, options.release_user_data, options.queue)?;
        let callbacks = TileCallbacks {
            registration,
            fetch_tile,
            cancel_tile: options.cancel_tile,
            map,
            source_id: Arc::new(LentText::new(&id)),
        };
        add_callback_source(callbacks, |callbacks| {
            // SAFETY: the caller's contract.
            unsafe { command(map, completion, |map| map.add_custom_vector_source(id, callbacks, zooms)) }
        })
    })
}

/// `mln_map_set_custom_vector_source_tile_data()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_custom_vector_source_tile_data(
    map: mln_map,
    source_id: mln_buffer_view,
    tile_id: mln_canonical_tile_id,
    encoding: mln_style_vector_tile_encoding,
    data: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(source_id, "source_id") }?;
        let format = match encoding {
            MLN_STYLE_VECTOR_TILE_ENCODING_MVT => TileDataFormat::Mvt,
            MLN_STYLE_VECTOR_TILE_ENCODING_MLT => TileDataFormat::Mlt,
            _ => return Err(Error::invalid_argument("encoding is invalid")),
        };
        // SAFETY: the caller's contract.
        let bytes = unsafe { buffer::bytes(data, "data") }?;
        let data = Some(bytes.to_vec()).filter(|bytes| !bytes.is_empty());
        // SAFETY: the caller's contract.
        unsafe {
            command_with(map, completion, self::tile_id(tile_id), |map, tile| {
                map.set_custom_vector_source_tile_data(id, tile, data, format)
            })
        }
    })
}

/// `mln_map_set_custom_vector_source_tile_error()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_custom_vector_source_tile_error(
    map: mln_map,
    source_id: mln_buffer_view,
    tile_id: mln_canonical_tile_id,
    message: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each view.
        let (id, message) =
            unsafe { (required_text(source_id, "source_id")?, buffer::text(message, "message")?.to_owned()) };
        // SAFETY: the caller's contract.
        unsafe {
            command_with(map, completion, self::tile_id(tile_id), |map, tile| {
                map.set_custom_vector_source_tile_error(id, tile, message)
            })
        }
    })
}

/// `mln_map_invalidate_custom_vector_source_tile()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_invalidate_custom_vector_source_tile(
    map: mln_map,
    source_id: mln_buffer_view,
    tile_id: mln_canonical_tile_id,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(source_id, "source_id") }?;
        // SAFETY: the caller's contract.
        unsafe {
            command_with(map, completion, self::tile_id(tile_id), |map, tile| {
                map.invalidate_custom_vector_source_tile(id, tile)
            })
        }
    })
}

/// `mln_map_invalidate_custom_vector_source_region()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_invalidate_custom_vector_source_region(
    map: mln_map,
    source_id: mln_buffer_view,
    bounds: mln_lat_lng_bounds,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(source_id, "source_id") }?;
        // SAFETY: the caller's contract.
        unsafe {
            command_with(map, completion, self::bounds(bounds), |map, bounds| {
                map.invalidate_custom_vector_source_region(id, bounds)
            })
        }
    })
}

// MARK: - Images

/// `mln_map_set_style_image()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_style_image(
    map: mln_map,
    image_id: mln_buffer_view,
    image: *const mln_premultiplied_rgba8_image,
    options: *const mln_style_image_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each input.
        let image = unsafe { style_image(required_text(image_id, "image_id")?, image, options) }?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_style_image(image)) }
    })
}

/// `mln_map_remove_style_image()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_remove_style_image(
    map: mln_map,
    image_id: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(image_id, "image_id") }?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.remove_style_image(id)) }
    })
}

/// `mln_map_get_style_image()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_get_style_image(
    map: mln_map,
    image_id: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(image_id, "image_id") }?;
        // SAFETY: the caller's contract.
        unsafe { query(map, completion, |map| map.style_image(id), |image| LentImages(image.map(LentImage::new))) }
    })
}

// MARK: - Image sources

/// `mln_map_add_image_source_image()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_add_image_source_image(
    map: mln_map,
    source_id: mln_buffer_view,
    coordinates: *const mln_lat_lng,
    coordinate_count: usize,
    image: *const mln_premultiplied_rgba8_image,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each input.
        let (id, corners, image) = unsafe {
            (required_text(source_id, "source_id")?, corners(coordinates, coordinate_count)?, self::image(image)?)
        };
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.add_image_source(id, corners, image)) }
    })
}

/// `mln_map_set_image_source_url()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_image_source_url(
    map: mln_map,
    source_id: mln_buffer_view,
    url: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each view.
        let (id, url) = unsafe { (required_text(source_id, "source_id")?, required_text(url, "url")?) };
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_image_source_url(id, url)) }
    })
}

/// `mln_map_set_image_source_image()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_image_source_image(
    map: mln_map,
    source_id: mln_buffer_view,
    image: *const mln_premultiplied_rgba8_image,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each input.
        let (id, image) = unsafe { (required_text(source_id, "source_id")?, self::image(image)?) };
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_image_source_image(id, image)) }
    })
}

/// `mln_map_set_image_source_coordinates()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_image_source_coordinates(
    map: mln_map,
    source_id: mln_buffer_view,
    coordinates: *const mln_lat_lng,
    coordinate_count: usize,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each input.
        let (id, corners) =
            unsafe { (required_text(source_id, "source_id")?, corners(coordinates, coordinate_count)?) };
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_image_source_coordinates(id, corners)) }
    })
}

/// `mln_map_get_image_source_coordinates()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_get_image_source_coordinates(
    map: mln_map,
    source_id: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(source_id, "source_id") }?;
        // SAFETY: the caller's contract.
        unsafe {
            query(
                map,
                completion,
                |map| map.image_source_coordinates(id),
                |corners| Plain(corners.into_iter().map(geo::from_lat_lng).collect()),
            )
        }
    })
}

// MARK: - Layers

/// `mln_map_add_style_layer_json()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_add_style_layer_json(
    map: mln_map,
    layer_json: mln_buffer_view,
    before_layer_id: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each view.
        let (json, before) =
            unsafe { (required_text(layer_json, "layer_json")?, optional_text(before_layer_id, "before_layer_id")?) };
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.add_style_layer_json(json, before)) }
    })
}

/// `mln_map_remove_style_layer()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_remove_style_layer(
    map: mln_map,
    layer_id: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(layer_id, "layer_id") }?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.remove_style_layer(id)) }
    })
}

/// `mln_map_get_style_layer()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_get_style_layer(
    map: mln_map,
    layer_id: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(layer_id, "layer_id") }?;
        // SAFETY: the caller's contract.
        unsafe {
            query(
                map,
                completion,
                |map| map.style_layer(id),
                |layer| LentLayers(layer.into_iter().map(LentLayer::new).collect()),
            )
        }
    })
}

/// `mln_map_list_style_layers()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_list_style_layers(
    map: mln_map,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe {
            query(map, completion, ::mln_runtime::Map::style_layers, |layers| {
                LentLayers(layers.into_iter().map(LentLayer::new).collect())
            })
        }
    })
}

/// `mln_map_move_style_layer()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_move_style_layer(
    map: mln_map,
    layer_id: mln_buffer_view,
    before_layer_id: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each view.
        let (id, before) =
            unsafe { (required_text(layer_id, "layer_id")?, optional_text(before_layer_id, "before_layer_id")?) };
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.move_style_layer(id, before)) }
    })
}

/// `mln_map_get_style_layer_json()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_get_style_layer_json(
    map: mln_map,
    layer_id: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let id = unsafe { required_text(layer_id, "layer_id") }?;
        // SAFETY: the caller's contract.
        unsafe { query(map, completion, |map| map.style_layer_json(id), optional_text_value) }
    })
}

/// `mln_map_set_style_layer_property()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_style_layer_property(
    map: mln_map,
    layer_id: mln_buffer_view,
    property_name: mln_buffer_view,
    value: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each view.
        let (id, name, value) = unsafe {
            (
                required_text(layer_id, "layer_id")?,
                required_text(property_name, "property_name")?,
                required_text(value, "value")?,
            )
        };
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_style_layer_property(id, name, value)) }
    })
}

/// `mln_map_get_style_layer_property()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_get_style_layer_property(
    map: mln_map,
    layer_id: mln_buffer_view,
    property_name: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each view.
        let (id, name) =
            unsafe { (required_text(layer_id, "layer_id")?, required_text(property_name, "property_name")?) };
        // SAFETY: the caller's contract.
        unsafe { query(map, completion, |map| map.style_layer_property(id, name), optional_text_value) }
    })
}

// MARK: - Light

/// `mln_map_set_style_light_json()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_style_light_json(
    map: mln_map,
    light_json: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let json = unsafe { required_text(light_json, "light_json") }?;
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_style_light_json(json)) }
    })
}

/// `mln_map_set_style_light_property()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_style_light_property(
    map: mln_map,
    property_name: mln_buffer_view,
    value: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract, for each view.
        let (name, value) = unsafe { (required_text(property_name, "property_name")?, required_text(value, "value")?) };
        // SAFETY: the caller's contract.
        unsafe { command(map, completion, |map| map.set_style_light_property(name, value)) }
    })
}

/// `mln_map_get_style_light_property()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_get_style_light_property(
    map: mln_map,
    property_name: mln_buffer_view,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let name = unsafe { required_text(property_name, "property_name") }?;
        // SAFETY: the caller's contract.
        unsafe { query(map, completion, |map| map.style_light_property(name), optional_text_value) }
    })
}

// MARK: - Transitions

/// A transition's milliseconds as a duration: finite, not negative, and
/// within the duration's range.
fn transition_duration(milliseconds: f64, name: &str) -> Result<Duration, Error> {
    let nanos = milliseconds * 1e6;
    if milliseconds.is_finite() && milliseconds >= 0.0 && nanos < i64::MAX as f64 {
        Ok(Duration::from_nanos(nanos.round() as i64))
    } else {
        Err(Error::invalid_argument(format!(
            "transition {name} must be finite, not negative, and fit the native duration range"
        )))
    }
}

fn transition_options(options: &mln_style_transition_options) -> Result<TransitionOptions, Error> {
    let known = MLN_STYLE_TRANSITION_OPTION_DURATION
        | MLN_STYLE_TRANSITION_OPTION_DELAY
        | MLN_STYLE_TRANSITION_OPTION_ENABLE_PLACEMENT_TRANSITIONS;
    let fields = abi::Fields::new(options.fields, known, "mln_style_transition_options.fields")?;
    let duration = fields
        .get(MLN_STYLE_TRANSITION_OPTION_DURATION, options.duration_ms)
        .map(|duration| transition_duration(duration, "duration_ms"))
        .transpose()?;
    let delay = fields
        .get(MLN_STYLE_TRANSITION_OPTION_DELAY, options.delay_ms)
        .map(|delay| transition_duration(delay, "delay_ms"))
        .transpose()?;
    let placement = fields
        .get(MLN_STYLE_TRANSITION_OPTION_ENABLE_PLACEMENT_TRANSITIONS, options.enable_placement_transitions)
        .unwrap_or(true);
    Ok(TransitionOptions::new(duration, delay, placement))
}

fn from_transition_options(options: TransitionOptions) -> mln_style_transition_options {
    let milliseconds = |duration: Duration| duration.count() as f64 / 1e6;
    mln_style_transition_options {
        size: size_of::<mln_style_transition_options>() as u32,
        // The style always holds whether placements fade.
        fields: MLN_STYLE_TRANSITION_OPTION_ENABLE_PLACEMENT_TRANSITIONS
            | options.duration.map_or(0, |_| MLN_STYLE_TRANSITION_OPTION_DURATION)
            | options.delay.map_or(0, |_| MLN_STYLE_TRANSITION_OPTION_DELAY),
        duration_ms: options.duration.map_or(0.0, milliseconds),
        delay_ms: options.delay.map_or(0.0, milliseconds),
        enable_placement_transitions: options.enable_placement_transitions,
    }
}

/// `mln_map_set_style_transition_options()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_set_style_transition_options(
    map: mln_map,
    options: *const mln_style_transition_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        if options.is_null() {
            return Err(Error::invalid_argument("options must not be null"));
        }
        // SAFETY: the caller's contract.
        let options = unsafe { abi::read_sized(options) }?;
        // The header has the command, not the call, refuse what the fields
        // hold.
        // SAFETY: the caller's contract.
        unsafe {
            command_with(map, completion, transition_options(&options), |map, options| {
                map.set_style_transition_options(options)
            })
        }
    })
}

/// `mln_map_get_style_transition_options()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_map_get_style_transition_options(
    map: mln_map,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        unsafe {
            query(map, completion, ::mln_runtime::Map::style_transition_options, |options| {
                Plain(vec![from_transition_options(options)])
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels(width: u32, height: u32, stride: u32, bytes: &[u8]) -> mln_premultiplied_rgba8_image {
        mln_premultiplied_rgba8_image {
            size: size_of::<mln_premultiplied_rgba8_image>() as u32,
            width,
            height,
            stride,
            pixels: bytes.as_ptr(),
            byte_length: bytes.len(),
        }
    }

    #[test]
    fn padded_rows_pack_and_short_buffers_fail() {
        let padded = [1, 2, 3, 4, 9, 9, 5, 6, 7, 8, 9, 9];
        // SAFETY: local images over `padded`.
        unsafe {
            let packed = image(&pixels(1, 2, 6, &padded)).unwrap();
            assert_eq!(packed.data, [1, 2, 3, 4, 5, 6, 7, 8]);
            // The last row needs no padding.
            assert!(image(&pixels(1, 2, 6, &padded[..10])).is_ok());
            assert_eq!(image(&pixels(1, 2, 6, &padded[..9])).unwrap_err().message(), "image byte_length is too small");
            assert!(image(&pixels(1, 2, 3, &padded)).unwrap_err().message().contains("stride"));
            assert!(image(&pixels(0, 2, 6, &padded)).unwrap_err().message().contains("dimensions"));
            let no_pixels = mln_premultiplied_rgba8_image { pixels: core::ptr::null(), ..pixels(1, 1, 4, &padded) };
            assert_eq!(image(&no_pixels).unwrap_err().message(), "image pixels must not be null");
        }
    }

    #[test]
    fn stretches_widen_and_increase() {
        let check = |intervals: &[(f32, f32)]| {
            let intervals: Vec<_> = intervals.iter().map(|&(from, to)| mln_image_stretch { from, to }).collect();
            // SAFETY: a local array of the count passed.
            unsafe { stretches(intervals.as_ptr(), intervals.len(), "stretch_x") }
                .map_err(|error| error.message().to_owned())
        };
        assert_eq!(check(&[(0.0, 1.0), (1.0, 2.0)]).unwrap(), [(0.0, 1.0), (1.0, 2.0)]);
        assert_eq!(check(&[(2.0, 1.0)]).unwrap_err(), "stretch_x intervals must have a positive width");
        assert_eq!(
            check(&[(0.0, 2.0), (1.0, 3.0)]).unwrap_err(),
            "stretch_x intervals must increase and must not overlap"
        );
        assert_eq!(check(&[(0.0, f32::NAN)]).unwrap_err(), "stretch_x intervals must be finite");
    }

    #[test]
    fn tile_ids_and_bounds_stay_on_the_globe() {
        assert!(tile_id(mln_canonical_tile_id { z: 1, x: 1, y: 1 }).is_ok());
        let outside = tile_id(mln_canonical_tile_id { z: 1, x: 2, y: 0 }).unwrap_err();
        assert!(outside.message().contains("within zoom bounds"));
        let point = |latitude, longitude| mln_lat_lng { latitude, longitude };
        assert!(bounds(mln_lat_lng_bounds { southwest: point(-1.0, -1.0), northeast: point(1.0, 1.0) }).is_ok());
        let inverted = bounds(mln_lat_lng_bounds { southwest: point(1.0, 1.0), northeast: point(-1.0, -1.0) });
        assert!(inverted.unwrap_err().message().contains("southwest must be less than or equal"));
    }

    #[test]
    fn transitions_round_trip_in_milliseconds() {
        let options = mln_style_transition_options {
            size: size_of::<mln_style_transition_options>() as u32,
            fields: MLN_STYLE_TRANSITION_OPTION_DURATION | MLN_STYLE_TRANSITION_OPTION_DELAY,
            duration_ms: 250.0,
            delay_ms: 0.5,
            enable_placement_transitions: false,
        };
        let read = from_transition_options(transition_options(&options).unwrap());
        assert_eq!((read.duration_ms, read.delay_ms, read.enable_placement_transitions), (250.0, 0.5, true));
        assert_eq!(read.fields, options.fields | MLN_STYLE_TRANSITION_OPTION_ENABLE_PLACEMENT_TRANSITIONS);
        for (duration_ms, delay_ms, name) in
            [(-1.0, 0.0, "duration_ms"), (0.0, f64::INFINITY, "delay_ms"), (1e20, 0.0, "duration_ms")]
        {
            let error =
                transition_options(&mln_style_transition_options { duration_ms, delay_ms, ..options }).unwrap_err();
            assert!(error.message().contains(name), "{error}");
        }
        let unknown = mln_style_transition_options { fields: 1 << 31, ..options };
        assert!(transition_options(&unknown).unwrap_err().message().contains("unknown bits"));
    }

    #[test]
    fn cluster_properties_parse_as_expressions() {
        let parsed = parse_cluster_properties(r#"{"total":["+",["get","weight"]]}"#).unwrap();
        assert_eq!(parsed.keys().collect::<Vec<_>>(), ["total"]);
        assert!(parse_cluster_properties("[]").unwrap_err().message().contains("must contain a JSON object"));
        assert!(parse_cluster_properties(r#"{"total":5}"#).unwrap_err().message().contains("GeoJSON source options"));
        assert!(parse_cluster_properties(r#"{"total":NaN}"#).unwrap_err().message().contains("cluster_properties"));
    }
}
