// Ports test/gl/bucket.test.cpp (the Buckets.* tests; the OpenGL headless
// backend is the WebGPU one here) and test/renderer/fill_bucket.test.cpp.

use std::any::Any;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use feature::{Identifier, PropertyMap, Value};
use geometry::Point;
use mln_gfx::backend_scope::{BackendScope, ScopeType};
use mln_gfx::index_vector::IndexBufferBase;
use mln_gfx::renderer_backend::RendererBackend;
use mln_gfx::shaders::segment::SegmentBase;
use mln_gfx::types::BufferUsageType;
use mln_gfx::{AttributeDataType, DrawMode};
use mln_map_types::MapMode;
use mln_renderer::bucket::Bucket;
use mln_renderer::bucket_geometry::{BucketGeometry, paint_vertices};
use mln_renderer::buckets::raster_bucket::RasterLayoutVertex;
use mln_renderer::buckets::symbol_bucket::SymbolBucket;
use mln_renderer::buckets::{CircleBucket, FillBucket, LineBucket, RasterBucket};
use mln_renderer::layout::pattern_layout::PatternLayerMap;
use mln_renderer::tile_mask::TileMask;
use mln_style::CrossfadeParameters;
use mln_style::conversion::{Convertible, Error as ConversionError, convert_layer};
use mln_style::layer::static_immutable_cast;
use mln_style::layer_properties::LayerProperties;
use mln_style::layers::FillLayer;
use mln_style::layers::fill_layer_properties::{
    FillColor, FillLayerProperties, FillLayoutProperties, FillPaintProperties,
};
use mln_style::layers::line_layer_properties::LineLayoutProperties;
use mln_style::layers::symbol_layer_properties::SymbolLayoutProperties;
use mln_style::property_value::PropertyValue;
use mln_style::renderer::possibly_evaluated_property_value::PossiblyEvaluatedPropertyValue;
use mln_style_types::image::ImagePositions;
use mln_tile_id::CanonicalTileID;
use mln_tile_id::geometry_tile_data::{GeometryCollection, GeometryTileFeature};
use mln_util::geometry::FeatureType;
use mln_util::image::PremultipliedImage;
use mln_util::immutable::Immutable;
use mln_util::logging::{Event, EventSeverity, Log, Observer};
use mln_util::size::Size;
use mln_vec_map::VecMap;
use mln_webgpu::headless_backend::HeadlessBackend;
use mln_webgpu::upload_pass::UploadPass as WebGpuUploadPass;

/// `StubGeometryTileFeature(id, type, geometry, properties)`.
struct StubGeometryTileFeature {
    properties: PropertyMap,
    id: Identifier,
    type_: FeatureType,
    geometry: GeometryCollection,
}

impl GeometryTileFeature for StubGeometryTileFeature {
    fn get_type(&self) -> FeatureType {
        self.type_
    }
    fn get_value(&self, key: &str) -> Option<Value> {
        self.properties.get(key).cloned()
    }
    fn get_id(&self) -> Identifier {
        self.id.clone()
    }
    fn get_geometries(&self) -> &GeometryCollection {
        &self.geometry
    }
}

fn feature(type_: FeatureType, geometry: &GeometryCollection) -> StubGeometryTileFeature {
    StubGeometryTileFeature { properties: PropertyMap::new(), id: Identifier::Null, type_, geometry: geometry.clone() }
}

fn collection(rings: &[&[(i16, i16)]]) -> GeometryCollection {
    GeometryCollection::from(
        rings.iter().map(|ring| ring.iter().map(|(x, y)| Point::new(*x, *y)).collect::<Vec<_>>()).collect::<Vec<_>>(),
    )
}

/// Runs `f` with an upload pass of a headless backend (the C++ tests create
/// a `gl::HeadlessBackend`, a context and an upload pass).
fn with_upload_pass(f: impl FnOnce(&dyn mln_gfx::UploadPass)) {
    let backend = HeadlessBackend::create(Size { width: 512, height: 256 });
    let _scope = BackendScope::new(&backend, ScopeType::Explicit);
    let context = backend.get_context();
    let command_encoder = context.create_command_encoder();
    let upload_pass = command_encoder.create_upload_pass("upload", backend.get_default_renderable());
    f(&*upload_pass);
}

// gtest: Buckets.CircleBucket
#[test]
fn circle_bucket() {
    let mut bucket = CircleBucket::new(&VecMap::new(), MapMode::Static, 1.0);
    assert!(!bucket.has_data());
    assert!(!bucket.needs_upload());

    // CircleBucket::addFeature() is a no-op.
    let point = collection(&[&[(0, 0)]]);
    bucket
        .add_feature(
            &feature(FeatureType::Point, &point),
            &point,
            &ImagePositions::new(),
            &PatternLayerMap::new(),
            0,
            &CanonicalTileID::new(0, 0, 0),
        )
        .unwrap();
    assert!(!bucket.has_data());
    assert!(!bucket.needs_upload());

    bucket.segments.push(SegmentBase::new(0, 0));
    assert!(bucket.has_data());
    assert!(bucket.needs_upload());

    with_upload_pass(|upload_pass| bucket.upload(upload_pass));
    assert!(bucket.has_data());
    assert!(!bucket.needs_upload());
}

// gtest: Buckets.FillBucket
#[test]
fn fill_bucket() {
    let layout = FillLayoutProperties::PossiblyEvaluated::default();
    let mut bucket = FillBucket::new(&layout, &VecMap::new(), 5.0, 1);
    assert!(!bucket.has_data());
    assert!(!bucket.needs_upload());

    let polygon = collection(&[&[(0, 0), (0, 1), (1, 1)]]);
    bucket
        .add_feature(
            &feature(FeatureType::Polygon, &polygon),
            &polygon,
            &ImagePositions::new(),
            &PatternLayerMap::new(),
            0,
            &CanonicalTileID::new(0, 0, 0),
        )
        .unwrap();
    assert!(bucket.has_data());
    assert!(bucket.needs_upload());

    with_upload_pass(|upload_pass| bucket.upload(upload_pass));
    assert!(!bucket.needs_upload());
}

/// `StubIndexBuffer`: an index buffer that stands in for an upload.
struct StubIndexBuffer;

impl IndexBufferBase for StubIndexBuffer {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Runs `f` with the WebGPU upload pass of a headless backend.
fn with_webgpu_upload_pass(f: impl FnOnce(&WebGpuUploadPass)) {
    with_upload_pass(|upload_pass| {
        f(upload_pass.as_any().downcast_ref::<WebGpuUploadPass>().expect("WebGPU upload pass"));
    });
}

/// A fill bucket whose "fill-color" is the given data-driven expression, with
/// one polygon.
fn data_driven_fill_bucket(fill_color: serde_json::Value) -> FillBucket {
    let json = serde_json::json!({"type": "fill", "id": "fill", "source": "s", "paint": {"fill-color": fill_color}});
    let mut error = ConversionError::default();
    let layer = convert_layer(&Convertible::from(&json), &mut error).unwrap_or_else(|| panic!("{}", error.message));
    let mut evaluated = FillPaintProperties::PossiblyEvaluated::default();
    *evaluated.get_mut::<FillColor>() = PossiblyEvaluatedPropertyValue::Expression(
        layer.downcast_ref::<FillLayer>().expect("fill layer").get_fill_color().as_expression().clone(),
    );
    let properties: Immutable<dyn LayerProperties> = Arc::new(FillLayerProperties::with_evaluated(
        static_immutable_cast(&layer.base_impl()),
        CrossfadeParameters::default(),
        evaluated,
    ));
    let mut bucket = FillBucket::new(
        &FillLayoutProperties::PossiblyEvaluated::default(),
        &VecMap::from_iter([("fill".to_owned(), properties)]),
        5.0,
        1,
    );

    let polygon = collection(&[&[(0, 0), (0, 1), (1, 1)]]);
    let mut feature = feature(FeatureType::Polygon, &polygon);
    feature.properties.insert("c".to_owned(), Value::String("red".to_owned()));
    bucket
        .add_feature(
            &feature,
            &polygon,
            &ImagePositions::new(),
            &PatternLayerMap::new(),
            0,
            &CanonicalTileID::new(0, 0, 0),
        )
        .unwrap();
    bucket
}

// gtest: Buckets.FillBucketFreesCpuGeometryAfterUpload
#[test]
fn fill_bucket_frees_cpu_geometry_after_upload() {
    let bucket = data_driven_fill_bucket(serde_json::json!(["to-color", ["get", "c"]]));
    let vertex_count = bucket.vertices.len();
    let index_count = bucket.triangles.len();
    assert!(vertex_count > 0);
    assert!(index_count > 0);

    with_webgpu_upload_pass(|upload_pass| {
        bucket.upload(upload_pass);
        let geometry = BucketGeometry::default();

        // A layer draws the bucket: its data moves to the render thread's
        // vectors, which hold it until they are uploaded.
        let vertices = geometry.vertices(&bucket, &bucket.vertices);
        assert_eq!(bucket.vertices.data().capacity(), 0);
        assert_eq!(bucket.vertices.len(), vertex_count);
        assert_eq!(vertices.get_raw_data().len(), vertex_count * vertices.get_raw_size());

        // A drawable upload turns the shared vectors into buffers.
        let buffer = upload_pass.get_buffer(Some(&vertices), BufferUsageType::StaticDraw, false).expect("buffer");
        let triangles = geometry.indexes(&bucket, &bucket.triangles);
        triangles.borrow_mut().set_buffer(Some(Box::new(StubIndexBuffer)));
        triangles.borrow_mut().set_dirty(false);

        // The CPU copies are gone, the counts that drawables are built from
        // are not.
        assert!(!vertices.has_raw_data());
        assert!(vertices.get_raw_data().is_empty());
        assert_eq!(vertices.get_raw_count(), vertex_count);
        assert_eq!(triangles.borrow().vector().capacity(), 0);
        assert_eq!(triangles.borrow().elements(), index_count);
        assert_eq!(bucket.triangles.len(), index_count);

        // A second drawable over the same bucket reuses the vectors and the
        // buffer without reading the data.
        let again = geometry.vertices(&bucket, &bucket.vertices);
        assert!(Rc::ptr_eq(&vertices, &again));
        assert!(Rc::ptr_eq(&geometry.indexes(&bucket, &bucket.triangles), &triangles));
        let reused = upload_pass.get_buffer(Some(&again), BufferUsageType::StaticDraw, true).expect("buffer");
        assert!(Rc::ptr_eq(&buffer, &reused));

        // Data-driven paint values are freed too when nothing can rewrite them.
        let binders = bucket.paint_property_binders.get("fill").unwrap().read().unwrap();
        assert!(!binders.depends_on_feature_state());
        let buffer = &binders.interleaved_vertex_buffer;
        let paint = paint_vertices(Some(&geometry), &bucket, buffer, false);
        assert!(paint.get_raw_count() > 0);
        assert!(upload_pass.get_buffer(Some(&paint), BufferUsageType::StaticDraw, false).is_some());
        assert!(!paint.has_raw_data());
        assert_eq!(buffer.shared_vertex_vector.data().capacity(), 0);
    });
}

// gtest: Buckets.FillBucketKeepsPaintDataThatFeatureStateRewrites
#[test]
fn fill_bucket_keeps_paint_data_that_feature_state_rewrites() {
    let bucket = data_driven_fill_bucket(serde_json::json!([
        "to-color",
        ["case", ["boolean", ["feature-state", "hover"], false], "red", "blue"]
    ]));

    with_webgpu_upload_pass(|upload_pass| {
        bucket.upload(upload_pass);
        let geometry = BucketGeometry::default();
        let binders = bucket.paint_property_binders.get("fill").unwrap().read().unwrap();
        assert!(binders.depends_on_feature_state());
        let buffer = &binders.interleaved_vertex_buffer;
        let paint = paint_vertices(Some(&geometry), &bucket, buffer, binders.depends_on_feature_state());
        assert!(paint.get_raw_count() > 0);
        assert!(upload_pass.get_buffer(Some(&paint), BufferUsageType::StaticDraw, false).is_some());
        assert!(!buffer.shared_vertex_vector.is_moved());
        assert_eq!(buffer.shared_vertex_vector.data().len(), paint.get_raw_count());
    });
}

// gtest: Buckets.FillBucketKeepsCpuGeometryWhenRenderedFeaturesAreRetained
#[test]
fn fill_bucket_keeps_cpu_geometry_when_rendered_features_are_retained() {
    let mut bucket = data_driven_fill_bucket(serde_json::json!(["to-color", ["get", "c"]]));
    bucket.set_retain_features_by_id(true);
    let vertex_count = bucket.vertices.len();

    with_webgpu_upload_pass(|upload_pass| {
        bucket.upload(upload_pass);
        let geometry = BucketGeometry::default();
        let vertices = geometry.vertices(&bucket, &bucket.vertices);
        assert!(upload_pass.get_buffer(Some(&vertices), BufferUsageType::StaticDraw, false).is_some());

        // Reading rendered features back needs the vertices.
        assert_eq!(bucket.vertices.data().len(), vertex_count);
    });
}

// gtest: Buckets.LineBucket
#[test]
fn line_bucket() {
    let layout = LineLayoutProperties::PossiblyEvaluated::default();
    let mut bucket = LineBucket::new(layout, &VecMap::new(), 10.0, 1);
    assert!(!bucket.has_data());
    assert!(!bucket.needs_upload());

    // Ignore invalid feature type.
    let point = collection(&[&[(0, 0)]]);
    bucket
        .add_feature(
            &feature(FeatureType::Point, &point),
            &point,
            &ImagePositions::new(),
            &PatternLayerMap::new(),
            0,
            &CanonicalTileID::new(0, 0, 0),
        )
        .unwrap();
    assert!(!bucket.has_data());

    let line = collection(&[&[(0, 0), (1, 1)]]);
    bucket
        .add_feature(
            &feature(FeatureType::LineString, &line),
            &line,
            &ImagePositions::new(),
            &PatternLayerMap::new(),
            1,
            &CanonicalTileID::new(0, 0, 0),
        )
        .unwrap();
    assert!(bucket.has_data());
    assert!(bucket.needs_upload());

    with_upload_pass(|upload_pass| bucket.upload(upload_pass));
    assert!(!bucket.needs_upload());
}

// gtest: Buckets.LineBucketDrawablesBuildFromFreedIndexes
#[test]
fn line_bucket_drawables_build_from_freed_indexes() {
    let mut bucket = LineBucket::new(LineLayoutProperties::PossiblyEvaluated::default(), &VecMap::new(), 10.0, 1);
    let line = collection(&[&[(0, 0), (100, 100), (200, 0)]]);
    bucket
        .add_feature(
            &feature(FeatureType::LineString, &line),
            &line,
            &ImagePositions::new(),
            &PatternLayerMap::new(),
            0,
            &CanonicalTileID::new(0, 0, 0),
        )
        .unwrap();
    assert!(bucket.has_data());

    let backend = HeadlessBackend::create(Size { width: 512, height: 256 });
    let _scope = BackendScope::new(&backend, ScopeType::Explicit);
    let context = backend.get_context();
    let command_encoder = context.create_command_encoder();
    let upload_pass = command_encoder.create_upload_pass("upload", backend.get_default_renderable());
    bucket.upload(&*upload_pass);

    // An earlier drawable put the indexes in a buffer, which frees their CPU
    // copy.
    let geometry = BucketGeometry::default();
    let vertex_count = bucket.vertices.len();
    let index_count = bucket.triangles.len();
    let triangles = geometry.indexes(&bucket, &bucket.triangles);
    triangles.borrow_mut().set_buffer(Some(Box::new(StubIndexBuffer)));
    triangles.borrow_mut().set_dirty(false);
    assert!(triangles.borrow().vector().is_empty());

    // A layer that rebuilds its drawables for the tile builds the segments
    // from the counts.
    let mut builder = context.create_drawable_builder("line".to_owned());
    builder.set_raw_vertices(vec![0; vertex_count * 4], vertex_count, AttributeDataType::Short2);
    builder.set_segments_shared(
        DrawMode::Triangles(),
        Some(geometry.indexes(&bucket, &bucket.triangles)),
        &bucket.segments,
    );
    assert_eq!(builder.cur_vertex_count(), vertex_count);
    assert_eq!(triangles.borrow().elements(), index_count);
}

// gtest: Buckets.SymbolBucket
#[test]
fn symbol_bucket() {
    let layout = Arc::new(SymbolLayoutProperties::PossiblyEvaluated::default());
    let icons_need_linear = false;
    let sort_features_by_y = false;
    let bucket_leader_id = "test".to_owned();
    let symbol_instances = Vec::new();
    let symbol_ranges = Vec::new();

    let mut bucket = SymbolBucket::new(
        layout,
        &VecMap::new(),
        &PropertyValue::Constant(16.0),
        &PropertyValue::Constant(1.0),
        0.0,
        icons_need_linear,
        sort_features_by_y,
        bucket_leader_id,
        symbol_instances,
        symbol_ranges,
        1.0,
        false,
        Vec::new(),
        false, /*iconsInText*/
    );
    assert!(!bucket.has_icon_data());
    assert!(!bucket.has_sdf_icon_data());
    assert!(!bucket.has_text_data());
    assert!(!bucket.has_icon_collision_box_data());
    assert!(!bucket.has_text_collision_box_data());
    assert!(!bucket.has_icon_collision_circle_data());
    assert!(!bucket.has_text_collision_circle_data());
    assert!(!bucket.has_data());
    assert!(!bucket.needs_upload());

    // SymbolBucket::addFeature() is a no-op.
    let point = collection(&[&[(0, 0)]]);
    bucket
        .add_feature(
            &feature(FeatureType::Point, &point),
            &point,
            &ImagePositions::new(),
            &PatternLayerMap::new(),
            0,
            &CanonicalTileID::new(0, 0, 0),
        )
        .unwrap();
    assert!(!bucket.has_data());
    assert!(!bucket.needs_upload());

    bucket.state().text.segments.push(SegmentBase::new(0, 0));
    assert!(bucket.has_text_data());
    assert!(bucket.has_data());
    assert!(bucket.needs_upload());

    with_upload_pass(|upload_pass| bucket.upload(upload_pass));
    assert!(!bucket.needs_upload());
}

// gtest: Buckets.RasterBucket
#[test]
fn raster_bucket() {
    let rgba = PremultipliedImage::new(Size::new(1, 1));

    // RasterBucket::hasData() is always true.
    let bucket = RasterBucket::new(rgba);
    assert!(bucket.has_data());
    assert!(bucket.needs_upload());

    with_upload_pass(|upload_pass| bucket.upload(upload_pass));
    assert!(!bucket.needs_upload());

    bucket.clear();
    assert!(bucket.needs_upload());
}

/// The C++ test's `operator==(SegmentBase, SegmentBase)` compares only these.
fn segment_tuples(segments: &[SegmentBase]) -> Vec<(usize, usize, usize, usize)> {
    segments.iter().map(|s| (s.vertex_offset, s.index_offset, s.vertex_length, s.index_length)).collect()
}

fn mask(ids: &[(u8, u32, u32)]) -> TileMask {
    let mut mask = TileMask::new();
    for &(z, x, y) in ids {
        mask.insert(CanonicalTileID::new(z, x, y));
    }
    mask
}

fn raster_vertex(p: (i16, i16), t: (u16, u16)) -> RasterLayoutVertex {
    RasterBucket::layout_vertex(Point::new(p.0, p.1), Point::new(t.0, t.1))
}

// gtest: Buckets.RasterBucketMaskEmpty
#[test]
fn raster_bucket_mask_empty() {
    let bucket = RasterBucket::with_shared_image(None);
    bucket.set_mask(mask(&[]));
    let geometry = bucket.geometry();
    assert_eq!(Vec::<RasterLayoutVertex>::new(), geometry.vertices);
    assert_eq!(Vec::<u16>::new(), geometry.indices);
    assert_eq!(vec![(0, 0, 0, 0)], segment_tuples(&geometry.segments));
}

// gtest: Buckets.RasterBucketMaskNoChildren
#[test]
fn raster_bucket_mask_no_children() {
    let bucket = RasterBucket::with_shared_image(None);
    bucket.set_mask(mask(&[(0, 0, 0)]));

    // A mask of 0/0/0 doesn't produce buffers since we're instead using the
    // global shared buffers.
    let geometry = bucket.geometry();
    assert_eq!(Vec::<RasterLayoutVertex>::new(), geometry.vertices);
    assert_eq!(Vec::<u16>::new(), geometry.indices);
    assert_eq!(Vec::<(usize, usize, usize, usize)>::new(), segment_tuples(&geometry.segments));
}

// gtest: Buckets.RasterBucketMaskTwoChildren
#[test]
fn raster_bucket_mask_two_children() {
    let bucket = RasterBucket::with_shared_image(None);
    bucket.set_mask(mask(&[(1, 0, 0), (1, 1, 1)]));

    let geometry = bucket.geometry();
    assert_eq!(
        vec![
            // 1/0/1
            raster_vertex((0, 0), (0, 0)),
            raster_vertex((4096, 0), (4096, 0)),
            raster_vertex((0, 4096), (0, 4096)),
            raster_vertex((4096, 4096), (4096, 4096)),
            // 1/1/1
            raster_vertex((4096, 4096), (4096, 4096)),
            raster_vertex((8192, 4096), (8192, 4096)),
            raster_vertex((4096, 8192), (4096, 8192)),
            raster_vertex((8192, 8192), (8192, 8192)),
        ],
        geometry.vertices
    );

    assert_eq!(
        vec![
            // 1/0/1
            0u16, 1, 2, 1, 2, 3, //
            // 1/1/1
            4, 5, 6, 5, 6, 7,
        ],
        geometry.indices
    );

    assert_eq!(vec![(0, 0, 8, 12)], segment_tuples(&geometry.segments));
}

// gtest: Buckets.RasterBucketMaskComplex
#[test]
fn raster_bucket_mask_complex() {
    let bucket = RasterBucket::with_shared_image(None);
    bucket.set_mask(mask(&[(1, 0, 1), (1, 1, 0), (2, 2, 3), (2, 3, 2), (3, 6, 7), (3, 7, 6)]));

    let geometry = bucket.geometry();
    assert_eq!(
        vec![
            // 1/0/1
            raster_vertex((0, 4096), (0, 4096)),
            raster_vertex((4096, 4096), (4096, 4096)),
            raster_vertex((0, 8192), (0, 8192)),
            raster_vertex((4096, 8192), (4096, 8192)),
            // 1/1/0
            raster_vertex((4096, 0), (4096, 0)),
            raster_vertex((8192, 0), (8192, 0)),
            raster_vertex((4096, 4096), (4096, 4096)),
            raster_vertex((8192, 4096), (8192, 4096)),
            // 2/2/3
            raster_vertex((4096, 6144), (4096, 6144)),
            raster_vertex((6144, 6144), (6144, 6144)),
            raster_vertex((4096, 8192), (4096, 8192)),
            raster_vertex((6144, 8192), (6144, 8192)),
            // 2/3/2
            raster_vertex((6144, 4096), (6144, 4096)),
            raster_vertex((8192, 4096), (8192, 4096)),
            raster_vertex((6144, 6144), (6144, 6144)),
            raster_vertex((8192, 6144), (8192, 6144)),
            // 3/6/7
            raster_vertex((6144, 7168), (6144, 7168)),
            raster_vertex((7168, 7168), (7168, 7168)),
            raster_vertex((6144, 8192), (6144, 8192)),
            raster_vertex((7168, 8192), (7168, 8192)),
            // 3/7/6
            raster_vertex((7168, 6144), (7168, 6144)),
            raster_vertex((8192, 6144), (8192, 6144)),
            raster_vertex((7168, 7168), (7168, 7168)),
            raster_vertex((8192, 7168), (8192, 7168)),
        ],
        geometry.vertices
    );

    assert_eq!(
        vec![
            // 1/0/1
            0u16, 1, 2, 1, 2, 3, //
            // 1/1/0
            4, 5, 6, 5, 6, 7, //
            // 2/2/3
            8, 9, 10, 9, 10, 11, //
            // 2/3/2
            12, 13, 14, 13, 14, 15, //
            // 3/6/7
            16, 17, 18, 17, 18, 19, //
            // 3/7/6
            20, 21, 22, 21, 22, 23,
        ],
        geometry.indices
    );

    assert_eq!(vec![(0, 0, 24, 36)], segment_tuples(&geometry.segments));
}

type Records = Arc<Mutex<Vec<(EventSeverity, Event, i64, String)>>>;

struct Recorder(Records);

impl Observer for Recorder {
    fn on_record(&self, severity: EventSeverity, event: Event, code: i64, msg: &str) -> bool {
        self.0.lock().unwrap().push((severity, event, code, msg.to_owned()));
        true
    }
}

/// `FixtureLog` for the calling thread.
struct FixtureLog(Records);

impl FixtureLog {
    fn new() -> Self {
        let records: Records = Arc::new(Mutex::new(Vec::new()));
        Log::set_thread_observer(Some(Box::new(Recorder(records.clone()))));
        FixtureLog(records)
    }
    fn count(&self, severity: EventSeverity, event: Event, msg: &str) -> usize {
        self.0.lock().unwrap().iter().filter(|m| m.0 == severity && m.1 == event && m.2 == -1 && m.3 == msg).count()
    }
    fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_empty()
    }
}

impl Drop for FixtureLog {
    fn drop(&mut self) {
        Log::set_thread_observer(None);
    }
}

// FillBucket.TracksSDFPatternsPerLayer
// gtest: FillBucket.TracksSDFPatternsPerLayer
#[test]
fn tracks_sdf_patterns_per_layer() {
    let log = FixtureLog::new();
    let layout = FillLayoutProperties::PossiblyEvaluated::default();
    let mut bucket = FillBucket::new(&layout, &VecMap::new(), 5.0, 1);

    bucket.record_sdf_pattern("sdf-layer", true);
    bucket.record_sdf_pattern("rgba-layer", false);

    assert!(bucket.is_sdf_pattern("sdf-layer"));
    assert!(!bucket.is_sdf_pattern("rgba-layer"));
    assert!(!bucket.is_sdf_pattern("missing-layer"));
    assert!(log.is_empty());
}

// FillBucket.WarnsOnceForMixedPatternTypesPerBucket
// gtest: FillBucket.WarnsOnceForMixedPatternTypesPerBucket
#[test]
fn warns_once_for_mixed_pattern_types_per_bucket() {
    let log = FixtureLog::new();
    let layout = FillLayoutProperties::PossiblyEvaluated::default();
    let mut first_bucket = FillBucket::new(&layout, &VecMap::new(), 5.0, 1);
    let mut second_bucket = FillBucket::new(&layout, &VecMap::new(), 5.0, 1);

    first_bucket.record_sdf_pattern("mixed-layer", true);
    first_bucket.record_sdf_pattern("mixed-layer", false);
    first_bucket.record_sdf_pattern("mixed-layer", false);
    second_bucket.record_sdf_pattern("mixed-layer", true);
    second_bucket.record_sdf_pattern("mixed-layer", false);

    assert_eq!(
        2,
        log.count(
            EventSeverity::Warning,
            Event::Style,
            "Style sheet warning: Cannot mix SDF and non-SDF fill patterns in layer \"mixed-layer\""
        )
    );
}
