// Ports test/api/{api_misuse.test.cpp, custom_geometry_source.test.cpp,
// custom_layer.test.cpp, recycle_map.cpp}. test/api/annotations.test.cpp
// and test/api/query.test.cpp are the modules below; the fixture is in
// support/mod.rs.

mod gtest_annotations;
mod gtest_query;
mod support;

use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use geojson::{Feature, GeoJSON, Geometry, Point};
use mln_actor::RunLoop;
use mln_headless::HeadlessFrontend;
use mln_map::{CameraOptions, Map, MapMode, MapOptions, NullMapObserver};
use mln_storage::stub_file_source::{ResponseType, StubFileSource};
use mln_storage::{FileSourceManager, ResourceOptions};
use mln_style::PropertyValue;
use mln_style::layer::Layer;
use mln_style::layers::fill_layer::FillLayer;
use mln_style::layers::line_layer::LineLayer;
use mln_style::layers::symbol_layer::SymbolLayer;
use mln_style::sources::custom_geometry_source::{CustomGeometrySource, Options as CustomGeometrySourceOptions};
use mln_style::sources::geojson_source::GeoJSONSource;
use mln_util::Color;
use mln_util::geo::{LatLng, LatLngBounds};
use support::*;

/// `std::make_unique<style::Image>(id, image, pixelRatio)`.
pub fn style_image(
    id: &str,
    image: mln_util::image::PremultipliedImage,
    pixel_ratio: f32,
) -> Arc<mln_style::image::Impl> {
    Arc::new(
        mln_style::image::Impl::new(id.to_owned(), image, pixel_ratio, false, Vec::new(), Vec::new(), None, None, None)
            .expect("style::Image"),
    )
}

// gtest: API.RenderWithoutCallback
#[test]
#[ignore = "not applicable as written: the Rust StillImageCallback is a Box<dyn FnOnce>, so there is no null callback to pass (C++ renderStill(nullptr) logs \"StillImageCallback not set\"); see api_misuse.test.cpp:16"]
fn render_without_callback() {
    // C++: map->renderStill(nullptr); map.reset(); expects one Error/General
    // log "StillImageCallback not set". The Rust type system rules the
    // misuse out; nothing to run.
}

// gtest: CustomGeometrySource.Grid
#[test]
fn grid() {
    let _loop = RunLoop::new();

    let mut frontend = HeadlessFrontend::with_pixel_ratio(1.0);
    let mut map = Map::with_resource_options(
        &frontend,
        Rc::new(NullMapObserver),
        MapOptions::new().with_map_mode(MapMode::Static).with_size(frontend.get_size()),
        &FileSourceManager::new(),
        &ResourceOptions::default()
            .with_cache_path(":memory:")
            .with_asset_path(cpp_root().join("test/fixtures/api/assets").to_str().unwrap()),
    )
    .unwrap();
    map.get_style_mut().load_json(&read_file("test/fixtures/api/water.json"));
    map.jump_to(&CameraOptions {
        center: Some(LatLng::new(37.8, -122.5).unwrap()),
        zoom: Some(10.0),
        ..Default::default()
    });

    let handle = Arc::new(OnceLock::new());
    let options = CustomGeometrySourceOptions {
        fetch_tile_function: Some(answer_with(&handle, |tile_id| {
            let grid_spacing = 0.1;
            let mut features = Vec::new();
            let bounds = LatLngBounds::from_canonical_tile(tile_id.z, tile_id.x, tile_id.y);
            let mut y = (bounds.north() / grid_spacing).ceil() * grid_spacing;
            while y >= (bounds.south() / grid_spacing).floor() * grid_spacing {
                let grid_line = vec![Point::new(bounds.west(), y), Point::new(bounds.east(), y)];
                features.push(Feature::new(Geometry::LineString(grid_line)));
                y -= grid_spacing;
            }

            let mut x = (bounds.west() / grid_spacing).floor() * grid_spacing;
            while x <= (bounds.east() / grid_spacing).ceil() * grid_spacing {
                let grid_line = vec![Point::new(x, bounds.south()), Point::new(x, bounds.north())];
                features.push(Feature::new(Geometry::LineString(grid_line)));
                x += grid_spacing;
            }
            features
        })),
        ..Default::default()
    };

    let source = CustomGeometrySource::new("custom", options).unwrap();
    handle.set(source.handle()).unwrap();
    map.get_style_mut().add_source(Box::new(source)).unwrap();

    let mut fill_layer = FillLayer::new("landcover", "mapbox");
    fill_layer.set_source_layer("landcover");
    fill_layer.set_fill_color(&PropertyValue::Constant(Color::new(1.0, 1.0, 0.0, 1.0)));
    map.get_style_mut().add_layer(Box::new(fill_layer), None).unwrap();

    let mut layer = LineLayer::new("grid", "custom");
    layer.set_line_color(&PropertyValue::Constant(Color::new(1.0, 1.0, 1.0, 1.0)));
    map.get_style_mut().add_layer(Box::new(layer), None).unwrap();

    check_image(
        "test/fixtures/custom_geometry_source/grid",
        &frontend.render(&mut map).expect("render").image,
        0.0006,
        0.1,
    );
}

// gtest: CustomLayer.Basic
#[test]
#[ignore = "OpenGL-only in C++ (returns early on other backends; the CustomLayerHost draws with GL calls); also TODO(port): CustomLayer; see custom_layer.test.cpp:93"]
fn basic() {
    // test/api/custom_layer.test.cpp:93: a GL CustomLayer drawing a triangle
    // under a fill layer (test/fixtures/custom_layer/basic, 0.0006, 0.1).
}

// gtest: CustomLayer.RendersAfterLeavingZoomRange
#[test]
#[ignore = "OpenGL-only in C++, and needs the CustomLayer that CustomLayer.Basic waits on"]
fn renders_after_leaving_zoom_range() {
    // A GL CustomLayer with minzoom 10 renders at zoom 16, is blank at zoom
    // 5, and renders the same image at zoom 16 again.
}

// gtest: API.RecycleMapUpdateImages
#[test]
fn recycle_map_update_images() {
    let _loop = RunLoop::new();

    let mut frontend = HeadlessFrontend::with_pixel_ratio(1.0);
    let mut map = Map::with_file_source(
        &frontend,
        Rc::new(NullMapObserver),
        Arc::new(StubFileSource::new(ResponseType::Asynchronous)),
        MapOptions::new().with_map_mode(MapMode::Static).with_size(frontend.get_size()),
    )
    .unwrap();

    let load_style = |map: &mut Map, marker_name: &str, marker_path: &str| {
        let source = GeoJSONSource::with_default_options("geometry");
        source.set_geojson(&GeoJSON::Geometry(Geometry::Point(Point::new(0.0, 0.0)))).unwrap();

        let mut layer = SymbolLayer::new("geometry", "geometry");
        layer.set_icon_image(&PropertyValue::Constant(mln_style::expression::Image::new(marker_name)));

        map.get_style_mut().load_json(&read_file("test/fixtures/api/empty.json"));
        map.get_style_mut().add_source(Box::new(source)).unwrap();
        map.get_style_mut().add_layer(Box::new(layer), None).unwrap();
        map.get_style_mut().add_image(style_image(marker_name, decode_image(marker_path), 1.0));
    };

    // default marker

    load_style(&mut map, "default_marker", "test/fixtures/sprites/default_marker.png");
    check_image(
        "test/fixtures/recycle_map/default_marker",
        &frontend.render(&mut map).expect("render").image,
        0.0006,
        0.1,
    );

    // flipped marker

    load_style(&mut map, "flipped_marker", "test/fixtures/sprites/flipped_marker.png");
    check_image(
        "test/fixtures/recycle_map/flipped_marker",
        &frontend.render(&mut map).expect("render").image,
        0.0006,
        0.1,
    );
}
