//! The style's JSON-text entry points, with the cases of maplibre-native-ffi's
//! style ABI tests (`tests/native/abi/style`), which reach them through the C
//! layer.

use mln_util::{Error, ErrorKind};

use crate::style::Style;

const EMPTY_GEOJSON_SOURCE: &str = r#"{"type":"geojson","data":{"type":"FeatureCollection","features":[]}}"#;
const DEM_SOURCE: &str = r#"{"type":"raster-dem","tiles":["https://example.com/dem/{z}/{x}/{y}.png"],"tileSize":256}"#;
const DOTS: &str = r#"{"id":"dots","type":"circle","source":"points"}"#;

fn loaded_style() -> Style {
    let mut style = Style::new(1.0);
    style.load_json(r#"{"version":8,"sources":{},"layers":[]}"#);
    assert!(style.is_loaded());
    style
}

/// A loaded style with the GeoJSON source `points` and the circle layer
/// `dots` on it.
fn style_with_dots() -> Style {
    let mut style = loaded_style();
    style.add_source_json("points", EMPTY_GEOJSON_SOURCE).unwrap();
    style.add_layer_json(DOTS, None).unwrap();
    style
}

#[track_caller]
fn assert_fails(result: Result<impl std::fmt::Debug, Error>, kind: ErrorKind, message: &str) {
    let error = result.expect_err("the call should fail");
    assert_eq!(error.kind, kind, "{}", error.message);
    assert!(error.message.contains(message), "{:?} doesn't contain {message:?}", error.message);
}

fn property(style: &Style, layer_id: &str, name: &str) -> Option<String> {
    style.layer_property_json(layer_id, name).unwrap()
}

fn set_property(style: &mut Style, layer_id: &str, name: &str, json: &str) {
    style.set_layer_property_json(layer_id, name, json).unwrap();
}

#[test]
fn edits_fail_while_the_style_is_not_loaded() {
    let mut style = Style::new(1.0);
    assert!(!style.is_loaded());
    let not_loaded = "style is not loaded";
    assert_fails(style.add_source_json("points", EMPTY_GEOJSON_SOURCE), ErrorKind::InvalidState, not_loaded);
    assert_fails(style.add_layer_json(DOTS, None), ErrorKind::InvalidState, not_loaded);
    assert_fails(style.set_layer_property_json("dots", "circle-radius", "4"), ErrorKind::InvalidState, not_loaded);
    assert_fails(style.set_light_json(r#"{"intensity":0.5}"#), ErrorKind::InvalidState, not_loaded);
    assert_fails(style.set_light_property_json("intensity", "0.5"), ErrorKind::InvalidState, not_loaded);
    assert!(style.get_sources().is_empty() && style.get_layers().is_empty());
}

#[test]
fn a_source_id_is_taken_once() {
    let mut style = loaded_style();
    style.add_source_json("owned-source", EMPTY_GEOJSON_SOURCE).unwrap();
    assert_eq!(style.get_source("owned-source").unwrap().get_id(), "owned-source");
    assert_fails(
        style.add_source_json("owned-source", EMPTY_GEOJSON_SOURCE),
        ErrorKind::AlreadyExists,
        r#"source "owned-source" already exists"#,
    );
}

#[test]
fn malformed_sources_are_invalid_arguments() {
    let mut style = loaded_style();
    assert_fails(
        style.add_source_json("unknown-type", r#"{"type":"no-such-source-type"}"#),
        ErrorKind::InvalidArgument,
        r#"style source "unknown-type": "#,
    );
    assert_fails(
        style.add_source_json("truncated", r#"{"type":"#),
        ErrorKind::InvalidArgument,
        r#"style source "truncated" is not valid JSON"#,
    );
    assert!(style.get_sources().is_empty());
}

#[test]
fn missing_style_ids_report_not_found() {
    let mut style = loaded_style();
    assert_fails(
        style.set_layer_property_json("absent-layer", "circle-radius", "4"),
        ErrorKind::NotFound,
        r#"layer "absent-layer" does not exist"#,
    );
    assert_fails(
        style.layer_property_json("absent-layer", "circle-radius"),
        ErrorKind::NotFound,
        r#"layer "absent-layer" does not exist"#,
    );
    assert_fails(
        style.add_layer_json(r#"{"id":"paper","type":"background"}"#, Some("absent-layer")),
        ErrorKind::NotFound,
        r#"before_layer_id "absent-layer" does not exist"#,
    );
    assert_fails(
        style.add_layer_json(r#"{"id":"hillshade","type":"hillshade","source":"absent-source"}"#, None),
        ErrorKind::NotFound,
        r#"layer "hillshade" uses source "absent-source", which does not exist"#,
    );
    assert!(style.get_layers().is_empty());
    assert_eq!(style.layer_json("absent-layer"), None);
}

#[test]
fn layers_go_below_the_before_layer() {
    let mut style = style_with_dots();
    style.add_layer_json(r#"{"id":"paper","type":"background"}"#, Some("dots")).unwrap();
    style.add_layer_json(r#"{"id":"top","type":"background"}"#, None).unwrap();
    let ids: Vec<String> = style.get_layers().iter().map(|layer| layer.get_id()).collect();
    assert_eq!(ids, ["paper", "dots", "top"]);
}

#[test]
fn malformed_layers_are_invalid_arguments() {
    let mut style = style_with_dots();
    assert_fails(style.add_layer_json(r#"{"id":"#, None), ErrorKind::InvalidArgument, "style layer is not valid JSON");
    assert_fails(
        style.add_layer_json(r#"{"id":"odd","type":"no-such-layer-type"}"#, None),
        ErrorKind::InvalidArgument,
        "style layer: ",
    );
    assert_eq!(style.get_layers().len(), 1);
}

// A hillshade layer requires a raster DEM source, and a circle layer can't
// draw one. A layer ID is taken once.
#[test]
fn a_layer_requires_a_source_that_supports_its_type() {
    let mut style = loaded_style();
    style.add_source_json("dem", DEM_SOURCE).unwrap();
    style.add_source_json("points", EMPTY_GEOJSON_SOURCE).unwrap();

    let hillshade = r#"{"id":"hillshade","type":"hillshade","source":"dem"}"#;
    style.add_layer_json(hillshade, None).unwrap();
    let layer = style.get_layer("hillshade").unwrap();
    assert_eq!(layer.get_type_info().type_, "hillshade");
    assert_eq!(layer.get_source_id(), "dem");
    assert_fails(
        style.add_layer_json(hillshade, None),
        ErrorKind::AlreadyExists,
        r#"layer "hillshade" already exists"#,
    );

    assert_fails(
        style.add_layer_json(r#"{"id":"on-points","type":"hillshade","source":"points"}"#, None),
        ErrorKind::InvalidArgument,
        r#"source "points" does not support hillshade layer "on-points""#,
    );
    assert_fails(
        style.add_layer_json(r#"{"id":"dots","type":"circle","source":"dem"}"#, None),
        ErrorKind::InvalidArgument,
        r#"source "dem" does not support circle layer "dots""#,
    );
    assert_eq!(style.get_layers().len(), 1);
}

// A bad property name or value on a live layer is an invalid argument that
// leaves the layer as it was.
#[test]
fn layer_properties_and_filters_round_trip_through_json() {
    let mut style = style_with_dots();

    assert_eq!(property(&style, "dots", "circle-radius"), None);
    set_property(&mut style, "dots", "circle-radius", "4");
    assert_eq!(property(&style, "dots", "circle-radius").as_deref(), Some("4.0"));
    set_property(&mut style, "dots", "circle-color", r##""#ff0000""##);
    assert_eq!(property(&style, "dots", "circle-color").as_deref(), Some("[\"rgba\",255.0,0.0,0.0,1.0]"));

    assert_fails(
        style.set_layer_property_json("dots", "fill-color", r##""#00ff00""##),
        ErrorKind::InvalidArgument,
        r#"property "fill-color" of layer "dots""#,
    );
    assert_fails(
        style.set_layer_property_json("dots", "circle-radius", r#"{"radius":"#),
        ErrorKind::InvalidArgument,
        r#"property "circle-radius" of layer "dots" is not valid JSON"#,
    );
    assert_fails(
        style.set_layer_property_json("dots", "circle-radius", r#""big""#),
        ErrorKind::InvalidArgument,
        r#"property "circle-radius" of layer "dots": "#,
    );
    assert_eq!(property(&style, "dots", "circle-radius").as_deref(), Some("4.0"));
    // A name the layer type lacks reads as unset rather than failing.
    assert_eq!(property(&style, "dots", "fill-color"), None);

    assert_eq!(property(&style, "dots", "filter"), None);
    let filter = r#"["==",["get","kind"],"park"]"#;
    set_property(&mut style, "dots", "filter", filter);
    assert_eq!(property(&style, "dots", "filter").as_deref(), Some(filter));
    set_property(&mut style, "dots", "filter", "null");
    assert_eq!(property(&style, "dots", "filter"), None);

    set_property(&mut style, "dots", "circle-radius", "null");
    assert_eq!(property(&style, "dots", "circle-radius"), None);
}

#[test]
fn root_properties_match_serialization_and_null_resets_zoom() {
    let mut style = style_with_dots();
    assert_eq!(property(&style, "dots", "source").as_deref(), Some("\"points\""));
    assert_eq!(property(&style, "dots", "source-layer"), None);

    for name in ["minzoom", "maxzoom"] {
        assert_eq!(property(&style, "dots", name), None);
        set_property(&mut style, "dots", name, "4.5");
        assert_eq!(property(&style, "dots", name).as_deref(), Some("4.5"));
        set_property(&mut style, "dots", name, "null");
        assert_eq!(property(&style, "dots", name), None);
    }
    let layer = style.get_layer("dots").unwrap();
    assert_eq!(layer.get_min_zoom(), f32::NEG_INFINITY);
    assert_eq!(layer.get_max_zoom(), f32::INFINITY);
    let json = style.layer_json("dots").unwrap();
    assert!(!json.contains("minzoom") && !json.contains("maxzoom"), "{json}");
}

// The layer's JSON is the whole layer as it stands after later edits, and adds
// back as the same layer.
#[test]
fn layer_json_serializes_the_current_layer() {
    let mut style = loaded_style();
    style
        .add_layer_json(r##"{"id":"paper","type":"background","paint":{"background-color":"#ff0000"}}"##, None)
        .unwrap();
    set_property(&mut style, "paper", "minzoom", "2.0");

    let json = style.layer_json("paper").unwrap();
    for part in [
        r#""id":"paper""#,
        r#""type":"background""#,
        r#""background-color":["rgba",255.0,0.0,0.0,1.0]"#,
        r#""minzoom":2.0"#,
    ] {
        assert!(json.contains(part), "{json} doesn't contain {part}");
    }

    style.remove_layer("paper").unwrap();
    style.add_layer_json(&json, None).unwrap();
    assert_eq!(style.layer_json("paper").unwrap(), json);
}

// A layer's source and source layer change only on layer types that take a
// source. The source need not exist yet, and an empty source layer clears it.
#[test]
fn source_bindings_change_only_on_layers_that_take_a_source() {
    let mut style = style_with_dots();
    style.add_layer_json(r#"{"id":"paper","type":"background"}"#, None).unwrap();

    set_property(&mut style, "dots", "source", r#""not-yet-added""#);
    assert_eq!(style.get_layer("dots").unwrap().get_source_id(), "not-yet-added");
    assert_eq!(property(&style, "dots", "source").as_deref(), Some("\"not-yet-added\""));
    set_property(&mut style, "dots", "source-layer", r#""pois""#);
    assert_eq!(property(&style, "dots", "source-layer").as_deref(), Some("\"pois\""));
    set_property(&mut style, "dots", "source-layer", r#""""#);
    assert_eq!(property(&style, "dots", "source-layer"), None);

    assert_fails(
        style.set_layer_property_json("paper", "source", r#""points""#),
        ErrorKind::InvalidArgument,
        r#"background layer "paper" does not take a source"#,
    );
    assert_fails(
        style.set_layer_property_json("paper", "source-layer", r#""pois""#),
        ErrorKind::InvalidArgument,
        r#"background layer "paper" does not take a source-layer"#,
    );
    assert_eq!(property(&style, "paper", "source"), None);
    assert_eq!(property(&style, "paper", "source-layer"), None);
}

// The light has no ID to miss, so every bad input is an invalid argument.
#[test]
fn the_style_light_round_trips_through_json() {
    let mut style = loaded_style();
    style.set_light_json(r#"{"anchor":"viewport","intensity":0.5}"#).unwrap();
    assert_eq!(style.light_property_json("anchor").as_deref(), Some("\"viewport\""));
    assert_eq!(style.light_property_json("intensity").as_deref(), Some("0.5"));

    style.set_light_property_json("intensity", "0.75").unwrap();
    assert_eq!(style.light_property_json("intensity").as_deref(), Some("0.75"));

    assert_fails(
        style.set_light_property_json("intensity", r#""bright""#),
        ErrorKind::InvalidArgument,
        r#"style light property "intensity""#,
    );
    assert_fails(
        style.set_light_property_json("intensity", "{"),
        ErrorKind::InvalidArgument,
        r#"style light property "intensity" is not valid JSON"#,
    );
    assert_fails(style.set_light_json(r#"{"anchor":7}"#), ErrorKind::InvalidArgument, "style light");
    assert_eq!(style.light_property_json("intensity").as_deref(), Some("0.75"));
    assert_eq!(style.light_property_json("anchor").as_deref(), Some("\"viewport\""));
    assert_eq!(style.light_property_json("color"), None);

    style.set_light_property_json("intensity", "null").unwrap();
    assert_eq!(style.light_property_json("intensity"), None);
}
