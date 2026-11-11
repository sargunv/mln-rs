//! Ported gtests (test/style/**) plus the M1 render-test style check.
//! One module per C++ test file; `#[test]` names are the gtest names in
//! snake_case.

mod conversion_function;
mod conversion_geojson_options;
mod conversion_layer;
mod conversion_light;
mod conversion_padding;
mod conversion_property_value;
mod conversion_source_options;
mod conversion_stringify;
mod conversion_tileset;
mod expression_global_state;
mod filter;
mod layer_properties;
mod layer_round_trip;
mod m1_render_test_styles;
mod properties;
mod property_expression;
mod source;
mod style;
mod style_image;
mod style_json;
mod style_layer;
mod style_parser;
mod util;
mod variable_anchor_offset_collection;
