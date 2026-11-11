//! `query.h`: the features that a render session drew or loaded, and the
//! clusters of its clustered GeoJSON sources.

use ::mln_runtime::{RenderSession, RenderedQueryGeometry, RenderedQueryOptions, SourceQueryOptions};
use mln_style::filter::Filter;
use mln_util::geo::ScreenCoordinate;

use crate::abi::{self, Fields};
use crate::buffer::LentText;
use crate::completion::CompletionValue;
use crate::error::Error;
use crate::generated::types::{
    MLN_RENDERED_FEATURE_QUERY_OPTION_LAYER_IDS, MLN_RENDERED_QUERY_GEOMETRY_TYPE_BOX,
    MLN_RENDERED_QUERY_GEOMETRY_TYPE_LINE_STRING, MLN_RENDERED_QUERY_GEOMETRY_TYPE_POINT,
    MLN_SOURCE_FEATURE_QUERY_OPTION_SOURCE_LAYER_IDS, mln_buffer_view, mln_completion, mln_diagnostic,
    mln_render_session, mln_rendered_feature, mln_rendered_feature_query_options, mln_rendered_query_geometry,
    mln_screen_point, mln_source_feature, mln_source_feature_query_options, mln_status,
};
use crate::{buffer, parse_thread, render_session};

abi::versioned!(mln_rendered_query_geometry, mln_rendered_feature_query_options, mln_source_feature_query_options);

fn point(point: mln_screen_point) -> ScreenCoordinate {
    ScreenCoordinate::new(point.x, point.y)
}

/// The runtime's form of a caller's query geometry. The runtime checks its
/// points.
///
/// # Safety
///
/// `geometry` is null or points to a readable struct of the size it
/// declares, whose line string points are valid.
unsafe fn geometry(geometry: *const mln_rendered_query_geometry) -> Result<RenderedQueryGeometry, Error> {
    // SAFETY: the caller's contract.
    let geometry = unsafe { abi::read_sized(geometry) }?;
    // SAFETY: each arm reads the member that `type_` names.
    Ok(unsafe {
        match geometry.type_ {
            MLN_RENDERED_QUERY_GEOMETRY_TYPE_POINT => RenderedQueryGeometry::Point(point(geometry.data.point)),
            MLN_RENDERED_QUERY_GEOMETRY_TYPE_BOX => {
                let screen_box = geometry.data.box_;
                RenderedQueryGeometry::Box { min: point(screen_box.min), max: point(screen_box.max) }
            }
            MLN_RENDERED_QUERY_GEOMETRY_TYPE_LINE_STRING => {
                let line = geometry.data.line_string;
                if line.point_count == 0 {
                    return Err(Error::invalid_argument("query line string must contain points"));
                }
                if line.points.is_null() {
                    return Err(Error::invalid_argument("query line string points must not be null"));
                }
                // SAFETY: non-null, and the caller's contract.
                let points = core::slice::from_raw_parts(line.points, line.point_count);
                RenderedQueryGeometry::LineString(points.iter().copied().map(point).collect())
            }
            _ => return Err(Error::invalid_argument("rendered query geometry type is invalid")),
        }
    })
}

/// The texts of `count` views at `views`, which `name` names.
///
/// # Safety
///
/// `views` is null or points to `count` readable views, each valid for
/// [`buffer::text`].
unsafe fn texts(views: *const mln_buffer_view, count: usize, name: &str) -> Result<Vec<String>, Error> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if views.is_null() {
        return Err(Error::invalid_argument(format!("{name} must not be null")));
    }
    // SAFETY: non-null, and the caller's contract.
    let views = unsafe { core::slice::from_raw_parts(views, count) };
    views
        .iter()
        .map(|view| {
            if view.data.is_null() && view.size != 0 {
                return Err(Error::invalid_argument(format!("{name} has a view without bytes")));
            }
            // SAFETY: the caller's contract.
            unsafe { buffer::text(*view, name) }.map(str::to_owned)
        })
        .collect()
}

/// The filter that a caller's JSON names, parsed on the parse thread, or
/// none for an empty view.
///
/// # Safety
///
/// As for [`buffer::text`].
unsafe fn filter(view: mln_buffer_view) -> Result<Option<Filter>, Error> {
    // SAFETY: the caller's contract.
    let json = unsafe { buffer::text(view, "query filter") }?;
    if json.is_empty() {
        return Ok(None);
    }
    let json = json.to_owned();
    parse_thread::run(move || {
        let document: serde_json::Value = serde_json::from_str(&json)
            .map_err(|error| Error::invalid_argument(format!("query filter is not valid JSON: {error}")))?;
        let mut error = mln_style::conversion::Error::default();
        mln_style::conversion::convert::<Filter>(&mln_conversion::Convertible::from(&document), &mut error)
            .map(Some)
            .ok_or_else(|| Error::invalid_argument(format!("query filter is invalid: {}", error.message)))
    })?
}

/// # Safety
///
/// `options` is null or points to a readable struct of the size it
/// declares, whose views are valid.
unsafe fn rendered_options(options: *const mln_rendered_feature_query_options) -> Result<RenderedQueryOptions, Error> {
    if options.is_null() {
        return Ok(RenderedQueryOptions::default());
    }
    // SAFETY: non-null, and the caller's contract.
    let options = unsafe { abi::read_sized(options) }?;
    let fields = Fields::new(
        options.fields,
        MLN_RENDERED_FEATURE_QUERY_OPTION_LAYER_IDS,
        "mln_rendered_feature_query_options.fields",
    )?;
    let layer_ids = fields
        .get(MLN_RENDERED_FEATURE_QUERY_OPTION_LAYER_IDS, ())
        // SAFETY: the caller's contract.
        .map(|()| unsafe { texts(options.layer_ids, options.layer_id_count, "query layer IDs") })
        .transpose()?;
    // SAFETY: the caller's contract.
    let filter = unsafe { filter(options.filter) }?;
    Ok(RenderedQueryOptions::new(layer_ids, filter))
}

/// # Safety
///
/// `options` is null or points to a readable struct of the size it
/// declares, whose views are valid.
unsafe fn source_options(options: *const mln_source_feature_query_options) -> Result<SourceQueryOptions, Error> {
    if options.is_null() {
        return Ok(SourceQueryOptions::default());
    }
    // SAFETY: non-null, and the caller's contract.
    let options = unsafe { abi::read_sized(options) }?;
    let fields = Fields::new(
        options.fields,
        MLN_SOURCE_FEATURE_QUERY_OPTION_SOURCE_LAYER_IDS,
        "mln_source_feature_query_options.fields",
    )?;
    let source_layers = fields
        .get(MLN_SOURCE_FEATURE_QUERY_OPTION_SOURCE_LAYER_IDS, ())
        // SAFETY: the caller's contract.
        .map(|()| unsafe { texts(options.source_layer_ids, options.source_layer_id_count, "query source layer IDs") })
        .transpose()?;
    // SAFETY: the caller's contract.
    let filter = unsafe { filter(options.filter) }?;
    Ok(SourceQueryOptions::new(source_layers, filter))
}

/// A source ID that a query names, which must not be empty.
///
/// # Safety
///
/// As for [`buffer::text`].
unsafe fn source_id(view: mln_buffer_view) -> Result<String, Error> {
    // SAFETY: the caller's contract.
    let source_id = unsafe { buffer::text(view, "source_id") }?;
    if source_id.is_empty() {
        return Err(Error::invalid_argument("source_id must not be empty"));
    }
    Ok(source_id.to_owned())
}

fn feature_json(feature: &mln_util::feature::Feature) -> LentText {
    LentText::new(&geojson::stringify_feature(&feature.to_geojson_feature()))
}

/// The texts of rendered features, which their result lends.
struct RenderedFeatures(Vec<[LentText; 5]>);

impl RenderedFeatures {
    fn new(features: Vec<mln_util::feature::Feature>) -> Self {
        Self(
            features
                .iter()
                .map(|feature| {
                    let state = if feature.state.is_empty() {
                        LentText::default()
                    } else {
                        let state = feature::Value::Object(feature.state.clone());
                        LentText::new(&mln_conversion::value_conversion::value_to_json(&state).to_string())
                    };
                    [
                        feature_json(feature),
                        LentText::new(&feature.layer),
                        LentText::new(&feature.source),
                        LentText::new(&feature.source_layer),
                        state,
                    ]
                })
                .collect(),
        )
    }
}

impl CompletionValue for RenderedFeatures {
    type Item = mln_rendered_feature;

    fn lend<R>(&self, f: impl FnOnce(&[mln_rendered_feature]) -> R) -> R {
        let items: Vec<_> = self
            .0
            .iter()
            .map(|[feature, layer_id, source_id, source_layer_id, state]| mln_rendered_feature {
                feature: feature.view(),
                layer_id: layer_id.view(),
                source_id: source_id.view(),
                source_layer_id: source_layer_id.view(),
                state: state.view(),
            })
            .collect();
        f(&items)
    }
}

/// The texts of source features, which their result lends.
struct SourceFeatures(Vec<[LentText; 2]>);

impl CompletionValue for SourceFeatures {
    type Item = mln_source_feature;

    fn lend<R>(&self, f: impl FnOnce(&[mln_source_feature]) -> R) -> R {
        let items: Vec<_> = self
            .0
            .iter()
            .map(|[feature, source_layer_id]| mln_source_feature {
                feature: feature.view(),
                source_layer_id: source_layer_id.view(),
            })
            .collect();
        f(&items)
    }
}

fn feature_collection(features: Vec<feature::Feature<f64>>) -> LentText {
    LentText::new(&geojson::stringify_feature_collection(&features))
}

/// `mln_render_session_query_rendered_features()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_query_rendered_features(
    session: mln_render_session,
    geometry: *const mln_rendered_query_geometry,
    options: *const mln_rendered_feature_query_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let (geometry, options) = unsafe { (self::geometry(geometry)?, rendered_options(options)?) };
        // SAFETY: the caller's contract.
        unsafe {
            render_session::operation(
                session,
                completion,
                |session| session.query_rendered_features(geometry, options),
                RenderedFeatures::new,
            )
        }
    })
}

/// `mln_render_session_query_source_features()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_query_source_features(
    session: mln_render_session,
    source_id: mln_buffer_view,
    options: *const mln_source_feature_query_options,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let (source_id, options) = unsafe { (self::source_id(source_id)?, source_options(options)?) };
        // SAFETY: the caller's contract.
        unsafe {
            render_session::operation(
                session,
                completion,
                |session| session.query_source_features(source_id, options),
                |features| {
                    SourceFeatures(
                        features
                            .iter()
                            .map(|feature| [feature_json(feature), LentText::new(&feature.source_layer)])
                            .collect(),
                    )
                },
            )
        }
    })
}

/// `mln_render_session_get_cluster_children()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_get_cluster_children(
    session: mln_render_session,
    source_id: mln_buffer_view,
    cluster_id: u32,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let source_id = unsafe { self::source_id(source_id) }?;
        // SAFETY: the caller's contract.
        unsafe {
            render_session::operation(
                session,
                completion,
                |session| session.cluster_children(source_id, cluster_id),
                feature_collection,
            )
        }
    })
}

/// `mln_render_session_get_cluster_leaves()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_get_cluster_leaves(
    session: mln_render_session,
    source_id: mln_buffer_view,
    cluster_id: u32,
    limit: u32,
    offset: u32,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let source_id = unsafe { self::source_id(source_id) }?;
        // SAFETY: the caller's contract.
        unsafe {
            render_session::operation(
                session,
                completion,
                |session| session.cluster_leaves(source_id, cluster_id, limit, offset),
                feature_collection,
            )
        }
    })
}

/// `mln_render_session_get_cluster_expansion_zoom()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_render_session_get_cluster_expansion_zoom(
    session: mln_render_session,
    source_id: mln_buffer_view,
    cluster_id: u32,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let source_id = unsafe { self::source_id(source_id) }?;
        // SAFETY: the caller's contract.
        unsafe {
            render_session::operation(
                session,
                completion,
                |session: &RenderSession| session.cluster_expansion_zoom(source_id, cluster_id),
                u32::from,
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::types::{
        MLN_STATUS_INVALID_ARGUMENT, mln_rendered_query_geometry_data, mln_screen_box, mln_screen_line_string,
    };

    fn view(text: &'static str) -> mln_buffer_view {
        mln_buffer_view { data: text.as_ptr().cast(), size: text.len() }
    }

    fn query_geometry(type_: u32, data: mln_rendered_query_geometry_data) -> mln_rendered_query_geometry {
        mln_rendered_query_geometry { size: size_of::<mln_rendered_query_geometry>() as u32, type_, data }
    }

    fn message(result: Result<impl Sized, Error>) -> String {
        let error = result.err().unwrap();
        assert_eq!(error.status(), MLN_STATUS_INVALID_ARGUMENT);
        error.message().to_owned()
    }

    #[test]
    fn geometries_become_the_runtimes() {
        let min = mln_screen_point { x: 1.0, y: 2.0 };
        let max = mln_screen_point { x: 3.0, y: 4.0 };
        let points = [min, max];
        // SAFETY: local geometries, whose points outlive the calls.
        unsafe {
            let screen_box = query_geometry(
                MLN_RENDERED_QUERY_GEOMETRY_TYPE_BOX,
                mln_rendered_query_geometry_data { box_: mln_screen_box { min, max } },
            );
            assert_eq!(geometry(&screen_box).unwrap(), RenderedQueryGeometry::Box { min: point(min), max: point(max) });
            let line = query_geometry(
                MLN_RENDERED_QUERY_GEOMETRY_TYPE_LINE_STRING,
                mln_rendered_query_geometry_data {
                    line_string: mln_screen_line_string { points: points.as_ptr(), point_count: 2 },
                },
            );
            assert_eq!(geometry(&line).unwrap(), RenderedQueryGeometry::LineString(vec![point(min), point(max)]));
            let empty = query_geometry(
                MLN_RENDERED_QUERY_GEOMETRY_TYPE_LINE_STRING,
                mln_rendered_query_geometry_data {
                    line_string: mln_screen_line_string { points: points.as_ptr(), point_count: 0 },
                },
            );
            assert!(message(geometry(&empty)).contains("must contain points"));
            let unknown = query_geometry(9, mln_rendered_query_geometry_data { point: min });
            assert!(message(geometry(&unknown)).contains("geometry type is invalid"));
        }
    }

    #[test]
    fn options_name_layers_and_parse_filters() {
        let layers = [view("circles")];
        let mut options = mln_rendered_feature_query_options {
            size: size_of::<mln_rendered_feature_query_options>() as u32,
            fields: MLN_RENDERED_FEATURE_QUERY_OPTION_LAYER_IDS,
            layer_ids: layers.as_ptr(),
            layer_id_count: 1,
            filter: view(r#"["==",["get","kind"],"a"]"#),
        };
        // SAFETY: local options, whose views outlive the calls.
        unsafe {
            let parsed = rendered_options(&options).unwrap();
            assert_eq!(parsed.layer_ids, Some(vec!["circles".to_owned()]));
            assert!(parsed.filter.is_some());
            options.filter = view("");
            assert!(rendered_options(&options).unwrap().filter.is_none());
            options.filter = view(r#"["=="#);
            assert!(message(rendered_options(&options)).contains("query filter"));
            options.filter = view("");
            options.layer_ids = core::ptr::null();
            message(rendered_options(&options));
            let no_bytes = [mln_buffer_view { data: core::ptr::null(), size: 1 }];
            options.layer_ids = no_bytes.as_ptr();
            message(rendered_options(&options));
            assert!(rendered_options(core::ptr::null()).unwrap().layer_ids.is_none());
            assert_eq!(message(source_id(view(""))), "source_id must not be empty");
        }
    }
}
