// Ports test/layout/sort_key.test.cpp.

use std::collections::HashMap;
use std::sync::Arc;

use feature::{Feature, PropertyMap, Value};
use geometry::{Geometry, Point};
use mln_map_types::MapMode;
use mln_renderer::bucket::BucketPtr;
use mln_renderer::bucket_parameters::BucketParameters;
use mln_renderer::buckets::symbol_bucket::SymbolBucket;
use mln_renderer::buckets::{CircleBucket, FillBucket, LineBucket};
use mln_renderer::geometry::feature_index::FeatureIndex;
use mln_renderer::layermanager;
use mln_renderer::layout::layout::{LayoutParameters, SourceLayer};
use mln_renderer::render_layer::LayerRenderData;
use mln_renderer::tile::geojson_tile_data::GeoJSONTileData;
use mln_style::conversion::{Convertible, Error as ConversionError, convert_layer};
use mln_style::layer::static_immutable_cast;
use mln_style::layer_properties::LayerProperties;
use mln_style::layers::{CircleLayerProperties, FillLayerProperties, LineLayerProperties, SymbolLayerProperties};
use mln_style_types::image::{ImageDependencies, ImageMap, ImagePosition, ImagePositions, Impl as ImageImpl, RectU16};
use mln_text::glyph::{GlyphDependencies, GlyphMap, GlyphPositions};
use mln_tile_id::OverscaledTileID;
use mln_tile_id::geometry_tile_data::GeometryTileData;
use mln_util::image::PremultipliedImage;
use mln_util::immutable::Immutable;
use mln_util::size::Size;
use mln_vec_map::VecSet;
use serde_json::json;

#[derive(Clone, Copy, Debug, PartialEq)]
enum SortKeyType {
    Undefined,
    Constant,
    Camera,
    DataDriven,
}

const SORT_KEY_TYPES: [SortKeyType; 4] =
    [SortKeyType::Undefined, SortKeyType::Constant, SortKeyType::Camera, SortKeyType::DataDriven];

fn sort_key(type_: SortKeyType) -> Option<serde_json::Value> {
    match type_ {
        SortKeyType::Undefined => None,
        SortKeyType::Constant => Some(json!(5)),
        SortKeyType::Camera => Some(json!(["step", ["zoom"], 5, 10, 10])),
        SortKeyType::DataDriven => Some(json!(["number", ["get", "rank"]])),
    }
}

fn expected_order(type_: SortKeyType) -> Vec<usize> {
    match type_ {
        SortKeyType::Undefined => vec![0, 1, 2],
        // Preserve the existing tie order of explicitly specified sort keys.
        SortKeyType::Constant | SortKeyType::Camera => vec![2, 1, 0],
        SortKeyType::DataDriven => vec![1, 2, 0],
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Polygon,
    LineString,
    Point,
}

/// The properties of a layer of the given style JSON, with `sort_key_name`
/// set to the sort key of `type_`.
fn layer(mut json: serde_json::Value, sort_key_name: &str, type_: SortKeyType) -> Immutable<dyn LayerProperties> {
    if let Some(sort_key) = sort_key(type_) {
        json["layout"][sort_key_name] = sort_key;
    }
    let mut error = ConversionError::default();
    let layer = convert_layer(&Convertible::from(&json), &mut error).unwrap_or_else(|| panic!("{}", error.message));
    let base_impl = layer.base_impl();
    match json["type"].as_str().unwrap() {
        "fill" => Arc::new(FillLayerProperties::new(static_immutable_cast(&base_impl))),
        "line" => Arc::new(LineLayerProperties::new(static_immutable_cast(&base_impl))),
        "circle" => Arc::new(CircleLayerProperties::new(static_immutable_cast(&base_impl))),
        "symbol" => Arc::new(SymbolLayerProperties::new(static_immutable_cast(&base_impl))),
        other => panic!("no properties for {other}"),
    }
}

/// Lays out three features with ranks 2, 0, 1 at x = 100, 200, 300 and
/// returns the layer's bucket.
fn create_bucket(properties: Immutable<dyn LayerProperties>, shape: Shape) -> BucketPtr {
    let ranks = [2.0, 0.0, 1.0];
    let features = ranks
        .iter()
        .enumerate()
        .map(|(i, &rank)| {
            let x = ((i + 1) * 100) as i16;
            let geometry = match shape {
                Shape::Polygon => {
                    Geometry::Polygon(vec![vec![Point::new(x, 10), Point::new(x, 20), Point::new(x + 10, 20), Point::new(x, 10)]])
                }
                Shape::LineString => Geometry::LineString(vec![Point::new(x, 10), Point::new(x, 20)]),
                Shape::Point => Geometry::Point(Point::new(x, 300 - (i as i16) * 100)),
            };
            let mut feature = Feature::new(geometry);
            feature.properties = PropertyMap::from_iter([("rank".to_owned(), Value::Double(rank))]);
            feature
        })
        .collect();
    let data: Arc<dyn GeometryTileData + Send + Sync> = Arc::new(GeoJSONTileData::new(features));
    let bucket_parameters = BucketParameters {
        tile_id: OverscaledTileID::from_zxy(0, 0, 0),
        mode: MapMode::Continuous,
        pixel_ratio: 1.0,
        layer_type: properties.base_impl.get_type_info(),
        global_state: None,
        retain_features_by_id: false,
    };
    let mut glyph_dependencies = GlyphDependencies::default();
    let mut image_dependencies = ImageDependencies::new();
    let available_images = VecSet::from_iter(["marker".to_owned()]);
    let layer_id = properties.base_impl.id.clone();
    let mut layout = layermanager::create_layout(
        &mut LayoutParameters {
            bucket_parameters: &bucket_parameters,
            font_faces: None,
            glyph_dependencies: &mut glyph_dependencies,
            image_dependencies: &mut image_dependencies,
            available_images: &available_images,
        },
        SourceLayer::new(data.clone(), ""),
        &[properties],
    )
    .unwrap()
    .unwrap();

    let marker = Arc::new(
        ImageImpl::new(
            "marker".to_owned(),
            PremultipliedImage::new(Size::new(16, 16)),
            1.0,
            false,
            Vec::new(),
            Vec::new(),
            None,
            None,
            None,
        )
        .unwrap(),
    );
    let image_positions = ImagePositions::from_iter([(
        "marker".to_owned(),
        ImagePosition::new(RectU16 { x: 0, y: 0, w: 18, h: 18 }, &marker, 0),
    )]);
    let images = ImageMap::from_iter([("marker".to_owned(), marker)]);
    layout.prepare_symbols(&GlyphMap::new(), &GlyphPositions::new(), &images, &image_positions);
    let mut feature_index = FeatureIndex::new(Some(data));
    let mut render_data: HashMap<String, LayerRenderData> = HashMap::new();
    layout
        .create_bucket(&image_positions, &mut feature_index, &mut render_data, true, false, &bucket_parameters.tile_id.canonical)
        .unwrap();
    render_data.remove(&layer_id).unwrap().bucket
}

/// The features in the order their vertices are, from the x coordinate the
/// vertices store `coordinate_scale` times.
fn feature_order(xs: impl Iterator<Item = i16>, coordinate_scale: i16) -> Vec<usize> {
    let mut order: Vec<usize> = Vec::new();
    for x in xs {
        let index = (x / coordinate_scale / 100 - 1) as usize;
        if order.last() != Some(&index) {
            order.push(index);
        }
    }
    order
}

#[test]
fn fill_order() {
    for type_ in SORT_KEY_TYPES {
        let properties = layer(json!({ "id": "fill", "type": "fill", "source": "source" }), "fill-sort-key", type_);
        let bucket = create_bucket(properties, Shape::Polygon);
        let bucket = bucket.downcast_ref::<FillBucket>().unwrap();
        assert_eq!(expected_order(type_), feature_order(bucket.vertices.iter().map(|v| v.a1[0]), 1), "{type_:?}");
    }
}

#[test]
fn line_order() {
    for type_ in SORT_KEY_TYPES {
        let properties = layer(json!({ "id": "line", "type": "line", "source": "source" }), "line-sort-key", type_);
        let bucket = create_bucket(properties, Shape::LineString);
        let bucket = bucket.downcast_ref::<LineBucket>().unwrap();
        assert_eq!(expected_order(type_), feature_order(bucket.vertices.iter().map(|v| v.a1[0]), 2), "{type_:?}");
    }
}

#[test]
fn circle_order() {
    for type_ in SORT_KEY_TYPES {
        let properties =
            layer(json!({ "id": "circle", "type": "circle", "source": "source" }), "circle-sort-key", type_);
        let bucket = create_bucket(properties, Shape::Point);
        let bucket = bucket.downcast_ref::<CircleBucket>().unwrap();
        assert_eq!(expected_order(type_), feature_order(bucket.vertices.iter().map(|v| v.a1[0]), 2), "{type_:?}");
        let expected_sort_key = match type_ {
            SortKeyType::Constant | SortKeyType::Camera => 5.0,
            _ => 0.0,
        };
        assert_eq!(expected_sort_key, bucket.segments.first().unwrap().sort_key, "{type_:?}");
    }
}

fn symbol_layer(type_: SortKeyType, z_order: Option<&str>) -> Immutable<dyn LayerProperties> {
    let mut json = json!({
        "id": "symbol", "type": "symbol", "source": "source",
        "layout": { "icon-image": "marker", "icon-allow-overlap": true }
    });
    if let Some(z_order) = z_order {
        json["layout"]["symbol-z-order"] = json!(z_order);
    }
    layer(json, "symbol-sort-key", type_)
}

#[test]
fn symbol_order() {
    for type_ in SORT_KEY_TYPES {
        let bucket = create_bucket(symbol_layer(type_, None), Shape::Point);
        let bucket = bucket.downcast_ref::<SymbolBucket>().unwrap();
        let order: Vec<usize> =
            bucket.state().symbol_instances.iter().map(|symbol| symbol.get_data_feature_index()).collect();
        assert_eq!(expected_order(type_), order, "{type_:?}");
        // An explicit constant key still disables viewport-Y sorting for symbol-z-order:auto.
        assert_eq!(type_ == SortKeyType::Undefined, bucket.sort_features_by_y, "{type_:?}");
        match type_ {
            SortKeyType::Undefined => assert!(bucket.sort_key_ranges.is_empty()),
            SortKeyType::Constant | SortKeyType::Camera => {
                assert_eq!(bucket.sort_key_ranges.len(), 1, "{type_:?}");
                let range = bucket.sort_key_ranges[0];
                assert_eq!((range.sort_key, range.start, range.end), (5.0, 0, 3), "{type_:?}");
            }
            SortKeyType::DataDriven => assert_eq!(bucket.sort_key_ranges.len(), 3),
        }
    }
}

#[test]
fn symbol_viewport_y() {
    for type_ in SORT_KEY_TYPES {
        let bucket = create_bucket(symbol_layer(type_, Some("viewport-y")), Shape::Point);
        let bucket = bucket.downcast_ref::<SymbolBucket>().unwrap();
        assert!(bucket.sort_features_by_y, "{type_:?}");
        assert!(bucket.sort_key_ranges.is_empty(), "{type_:?}");
        let order: Vec<usize> =
            bucket.state().symbol_instances.iter().map(|symbol| symbol.get_data_feature_index()).collect();
        assert_eq!(vec![0, 1, 2], order, "{type_:?}");
    }
}
