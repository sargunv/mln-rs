// The map handle's style commands. They follow maplibre-native-ffi's ABI
// tests (tests/native/abi/style/{sources,custom_sources,geojson,layers}.c at
// b20632a) where those test the same rules.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mln_runtime::{CustomSourceCallbacks, ErrorKind, Map, ResourceRequest, Runtime, SourceKind};
use mln_storage::response::Reason;
use mln_storage::{ResourceError, Response};
use mln_style::sources::GeoJSONOptions;
use mln_style::sources::custom_geometry_source::TileOptions;
use mln_util::geo::LatLng;
use mln_util::image::PremultipliedImage;
use mln_util::range::Range;
use mln_util::size::Size;
use support::{EMPTY_STYLE, map, runtime, wait, wait_err};

const EMPTY_GEOJSON_SOURCE: &str = r#"{"type":"geojson","data":{"type":"FeatureCollection","features":[]}}"#;

fn point_collection(name: &str) -> String {
    format!(
        r#"{{"type":"FeatureCollection","features":[{{"type":"Feature","geometry":{{"type":"Point","coordinates":[0,0]}},"properties":{{"name":"{name}"}}}}]}}"#
    )
}

/// A runtime whose provider fails every request, so that URL sources never
/// reach the network, and one of its maps with an empty style.
fn styled_map() -> (Runtime, Map) {
    let runtime = runtime();
    wait(runtime.set_resource_provider(|request: ResourceRequest| {
        let error = ResourceError::new(Reason::NotFound, "the style tests serve nothing");
        drop(request.respond(Response { error: Some(Box::new(error)), ..Response::default() }));
    }));
    let map = map(&runtime);
    wait(map.set_style_json(EMPTY_STYLE));
    (runtime, map)
}

#[test]
fn a_source_reports_its_url_attribution_and_tiles() {
    let (_runtime, map) = styled_map();
    wait(map.add_style_source_json("remote", r#"{"type":"raster","url":"fixture://tiles.json"}"#));
    let inline = r#"{"type":"vector","attribution":"Fixture tiles","tiles":["fixture://a/{z}/{x}/{y}.mvt","fixture://b/{z}/{x}/{y}.mvt"]}"#;
    wait(map.add_style_source_json("inline", inline));
    wait(map.add_style_source_json("geojson", r#"{"type":"geojson","data":"fixture://first.geojson"}"#));
    wait(map.set_geojson_source_url("geojson", "fixture://second.geojson"));

    let remote = wait(map.style_source("remote")).unwrap();
    assert_eq!((remote.kind, remote.url.as_deref()), (SourceKind::Raster, Some("fixture://tiles.json")));
    assert!(remote.tiles.is_none() && remote.attribution.is_none());
    assert_eq!(remote.tile_size, Some(512));

    let inline = wait(map.style_source("inline")).unwrap();
    assert_eq!((inline.url.as_deref(), inline.attribution.as_deref()), (None, Some("Fixture tiles")));
    assert_eq!(inline.tiles.unwrap().urls, ["fixture://a/{z}/{x}/{y}.mvt", "fixture://b/{z}/{x}/{y}.mvt"]);
    assert!(inline.vector_encoding.is_some());

    assert_eq!(wait(map.style_source("geojson")).unwrap().url.as_deref(), Some("fixture://second.geojson"));
    assert!(wait(map.style_source("missing")).is_none());
}

#[test]
fn sources_list_in_style_order() {
    let (_runtime, map) = styled_map();
    let style = format!(
        r#"{{"version":8,"sources":{{"first":{EMPTY_GEOJSON_SOURCE},"second":{EMPTY_GEOJSON_SOURCE}}},"layers":[]}}"#
    );
    wait(map.set_style_json(style));
    wait(map.add_style_source_json("third", r#"{"type":"vector","tiles":["fixture://tiles/{z}/{x}/{y}"]}"#));
    wait(map.remove_style_source("first"));
    let sources = wait(map.style_sources());
    let listed: Vec<_> = sources.iter().map(|source| (source.id.as_str(), source.kind)).collect();
    assert_eq!(listed, [("second", SourceKind::GeoJson), ("third", SourceKind::Vector)]);
}

#[test]
fn a_geojson_source_reports_the_options_its_data_needs() {
    let (_runtime, map) = styled_map();
    let clustered = r#"{"type":"geojson","data":{"type":"FeatureCollection","features":[]},"minzoom":3,"maxzoom":12,"buffer":64,"tolerance":0.5,"cluster":true,"clusterRadius":40,"clusterMaxZoom":10,"clusterMinPoints":3,"clusterProperties":{"total":["+",["get","weight"]]}}"#;
    wait(map.add_style_source_json("clustered", clustered));
    let info = wait(map.style_source("clustered")).unwrap().geojson.unwrap();
    let options = &info.options;
    assert_eq!((options.minzoom, options.maxzoom, options.buffer, options.cluster_radius), (3, 12, 64, 40));
    assert_eq!((options.cluster_max_zoom, options.cluster_min_points, options.cluster), (10, 3, true));
    assert!(info.cluster_properties.contains("\"total\""), "{}", info.cluster_properties);

    // The reported properties parse back into the same expressions.
    let wrapper = format!(
        r#"{{"type":"geojson","data":"fixture://copy.geojson","clusterProperties":{}}}"#,
        info.cluster_properties
    );
    wait(map.add_style_source_json("copy", wrapper));
    let copy = wait(map.style_source("copy")).unwrap().geojson.unwrap();
    assert_eq!(copy.cluster_properties, info.cluster_properties);
    assert!(wait(map.style_source("clustered")).unwrap().vector_encoding.is_none());
}

#[test]
fn a_geojson_source_keeps_its_attribution_through_new_data() {
    let (_runtime, map) = styled_map();
    let options = GeoJSONOptions { attribution: Some("Fixture data".to_owned()), ..GeoJSONOptions::default() };
    wait(map.add_geojson_source("attributed", options));
    wait(map.set_geojson_source_data("attributed", r#"{"type":"Point","coordinates":[0,0]}"#));
    assert_eq!(wait(map.style_source("attributed")).unwrap().attribution.as_deref(), Some("Fixture data"));
    let error = wait_err(map.add_geojson_source("attributed", GeoJSONOptions::default()));
    assert_eq!(
        (error.kind, error.message.as_str()),
        (ErrorKind::AlreadyExists, "source \"attributed\" already exists")
    );
}

#[test]
fn geojson_data_checks_its_source_and_its_options() {
    let (_runtime, map) = styled_map();
    wait(map.add_geojson_source("plain", GeoJSONOptions::default()));
    wait(map.add_geojson_source("clustered", GeoJSONOptions { cluster: true, ..GeoJSONOptions::default() }));
    wait(map.set_geojson_source_data("clustered", point_collection("kept")));

    let refused = |source: &str, data: &str| {
        let error = wait_err(map.set_geojson_source_data(source, data));
        assert_eq!(error.kind, ErrorKind::InvalidArgument, "{}", error.message);
        error.message
    };
    assert!(refused("plain", r#"{"type":"FeatureCollection","features":[]}garbage"#).contains("not valid JSON"));
    let line = r#"{"type":"FeatureCollection","features":[{"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},"properties":{}}]}"#;
    assert!(refused("clustered", line).contains("feature 0 has line string geometry"));
    assert!(refused("clustered", r#"{"type":"Point","coordinates":[0,0]}"#).contains("the data is a bare geometry"));
    // Clustering's rules belong to clustering alone.
    wait(map.set_geojson_source_data("plain", line));
    wait(map.set_geojson_source_data("clustered", r#"{"type":"FeatureCollection","features":[]}"#));

    let missing = wait_err(map.set_geojson_source_data("missing", point_collection("lost")));
    assert_eq!((missing.kind, missing.message.as_str()), (ErrorKind::NotFound, "source \"missing\" does not exist"));
    wait(map.add_style_source_json("raster", r#"{"type":"raster","tiles":["fixture://{z}/{x}/{y}.png"]}"#));
    let raster = wait_err(map.set_geojson_source_data("raster", point_collection("lost")));
    assert_eq!(raster.message, "source \"raster\" is not a GeoJSON source");
    let tiling = wait_err(map.set_geojson_source_synchronous_tiling("raster", true));
    assert_eq!(tiling.kind, ErrorKind::InvalidArgument);
}

#[test]
fn only_the_newest_waiting_geojson_data_follows_the_data_being_built() {
    let (_runtime, map) = styled_map();
    wait(map.add_geojson_source("points", GeoJSONOptions::default()));
    let first = map.set_geojson_source_data("points", point_collection("first")).unwrap();
    let middle = map.set_geojson_source_data("points", point_collection("middle")).unwrap();
    let last = map.set_geojson_source_data("points", point_collection("last")).unwrap();
    assert!(first.wait().is_ok());
    let middle = middle.wait();
    assert!(middle.as_ref().map_or_else(|error| error.kind == ErrorKind::Cancelled, |_| true), "{middle:?}");
    assert!(last.wait().is_ok());

    // A URL replaces what still waits, and the removal of the source what is
    // being built.
    let building = map.set_geojson_source_data("points", point_collection("building")).unwrap();
    let waiting = map.set_geojson_source_data("points", point_collection("waiting")).unwrap();
    wait(map.set_geojson_source_url("points", "fixture://points.geojson"));
    let removed = map.remove_style_source("points").unwrap();
    for result in [building.wait(), waiting.wait()] {
        assert!(result.as_ref().map_or_else(|error| error.kind == ErrorKind::Cancelled, |_| true), "{result:?}");
    }
    removed.wait().unwrap();
}

#[test]
fn synchronous_tiling_installs_data_before_the_next_command() {
    let (_runtime, map) = styled_map();
    wait(map.add_geojson_source("points", GeoJSONOptions::default()));
    wait(map.set_geojson_source_synchronous_tiling("points", true));
    let installed = Arc::new(AtomicUsize::new(0));
    let seen = installed.clone();
    map.set_geojson_source_data("points", point_collection("now")).unwrap().on_complete(move |result| {
        result.unwrap();
        seen.fetch_add(1, Ordering::SeqCst);
    });
    // A query after the data sees it completed, since nothing waited.
    wait(map.style_sources());
    assert_eq!(installed.load(Ordering::SeqCst), 1);
}

fn image(width: u32, height: u32) -> PremultipliedImage {
    PremultipliedImage::new(Size::new(width, height))
}

fn corners(offset: f64) -> [LatLng; 4] {
    let corner = |latitude, longitude| LatLng::new(latitude, longitude + offset).unwrap();
    [corner(1.0, 2.0), corner(1.0, 3.0), corner(0.0, 3.0), corner(0.0, 2.0)]
}

#[test]
fn image_sources_take_corners_images_and_urls() {
    let (_runtime, map) = styled_map();
    wait(map.add_image_source("inline-image", corners(0.0), image(2, 2)));
    assert_eq!(wait(map.style_source("inline-image")).unwrap().kind, SourceKind::Image);
    assert_eq!(wait(map.image_source_coordinates("inline-image")), corners(0.0));
    wait(map.set_image_source_coordinates("inline-image", corners(4.0)));
    assert_eq!(wait(map.image_source_coordinates("inline-image")), corners(4.0));
    wait(map.set_image_source_image("inline-image", image(1, 1)));
    wait(map.set_image_source_url("inline-image", "fixture://image.png"));
    assert_eq!(wait(map.style_source("inline-image")).unwrap().url.as_deref(), Some("fixture://image.png"));

    wait(map.add_style_source_json("geojson", EMPTY_GEOJSON_SOURCE));
    let wrong = wait_err(map.set_image_source_coordinates("geojson", corners(0.0)));
    assert_eq!(
        (wrong.kind, wrong.message.as_str()),
        (ErrorKind::InvalidArgument, "source \"geojson\" is not an image source")
    );
    assert_eq!(wait_err(map.image_source_coordinates("missing")).kind, ErrorKind::NotFound);
}

/// Counts a custom source's calls and its callbacks' drop.
#[derive(Clone, Default)]
struct Probe {
    fetches: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

struct DropCounter(Arc<AtomicUsize>);

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Probe {
    fn callbacks(&self) -> CustomSourceCallbacks {
        let (fetches, counter) = (self.fetches.clone(), DropCounter(self.drops.clone()));
        CustomSourceCallbacks {
            fetch_tile: Box::new(move |_| {
                let _ = &counter;
                fetches.fetch_add(1, Ordering::SeqCst);
            }),
            cancel_tile: None,
        }
    }

    fn drops(&self) -> usize {
        self.drops.load(Ordering::SeqCst)
    }
}

fn add_geometry(map: &Map, id: &str, probe: &Probe) -> Result<mln_runtime::Applied, mln_runtime::Error> {
    map.add_custom_geometry_source(id, probe.callbacks(), Range::new(0, 18), TileOptions::default()).unwrap().wait()
}

#[test]
fn a_custom_source_drops_its_callbacks_once_it_leaves_the_style() {
    let (runtime, map) = styled_map();
    let removed = Probe::default();
    add_geometry(&map, "removed", &removed).unwrap();
    assert_eq!(wait(map.style_source("removed")).unwrap().kind, SourceKind::CustomGeometry);
    assert_eq!(removed.drops(), 0);
    wait(map.remove_style_source("removed"));
    assert_eq!(removed.drops(), 1);

    // A failed add drops the callbacks it carried before it completes.
    let duplicate = Probe::default();
    add_geometry(&map, "kept", &Probe::default()).unwrap();
    assert_eq!(add_geometry(&map, "kept", &duplicate).unwrap_err().kind, ErrorKind::AlreadyExists);
    assert_eq!(duplicate.drops(), 1);

    let replaced = Probe::default();
    let vector = Probe::default();
    add_geometry(&map, "replaced", &replaced).unwrap();
    wait(map.add_custom_vector_source("vector", vector.callbacks(), Range::new(0, 18)));
    assert_eq!(wait(map.style_source("vector")).unwrap().kind, SourceKind::CustomVector);
    wait(map.set_style_json(r#"{"version":8,"name":"next","sources":{},"layers":[]}"#));
    assert_eq!((replaced.drops(), vector.drops()), (1, 1));

    let released = Probe::default();
    add_geometry(&map, "released", &released).unwrap();
    wait(map.release());
    assert_eq!(released.drops(), 1);
    drop(runtime);
}

#[test]
fn tile_commands_check_the_kind_of_their_source() {
    let (_runtime, map) = styled_map();
    add_geometry(&map, "geometry", &Probe::default()).unwrap();
    let tile = mln_tile_id::CanonicalTileID::new(0, 0, 0);
    wait(map.set_custom_geometry_source_tile_data("geometry", tile, r#"{"type":"Point","coordinates":[0,0]}"#));
    wait(map.invalidate_custom_geometry_source_tile("geometry", tile));
    let wrong = wait_err(map.set_custom_vector_source_tile_data("geometry", tile, None, Default::default()));
    assert_eq!(wrong.message, "source \"geometry\" is not a custom vector source");
    let invalid = wait_err(map.set_custom_geometry_source_tile_data("geometry", tile, "{"));
    assert_eq!(invalid.kind, ErrorKind::InvalidArgument);
    assert_eq!(wait_err(map.invalidate_custom_vector_source_tile("missing", tile)).kind, ErrorKind::NotFound);
}

#[test]
fn layers_move_below_their_before_layer() {
    let (_runtime, map) = styled_map();
    wait(map.add_style_source_json("points", EMPTY_GEOJSON_SOURCE));
    wait(map.add_style_layer_json(r#"{"id":"paper","type":"background"}"#, None));
    wait(map.add_style_layer_json(r#"{"id":"dots","type":"circle","source":"points"}"#, None));
    wait(map.move_style_layer("dots", Some("paper".to_owned())));
    wait(map.move_style_layer("paper", Some("paper".to_owned())));
    let ids: Vec<_> = wait(map.style_layers()).into_iter().map(|layer| layer.id).collect();
    assert_eq!(ids, ["dots", "paper"]);
    let dots = wait(map.style_layer("dots")).unwrap();
    assert_eq!((dots.layer_type, dots.source_id.as_str(), dots.visible), ("circle", "points", true));
    assert_eq!((dots.min_zoom, dots.max_zoom), (f64::NEG_INFINITY, f64::INFINITY));
    let missing = wait_err(map.move_style_layer("dots", Some("absent".to_owned())));
    assert_eq!(missing.message, "before_layer_id \"absent\" does not exist");
}

#[test]
fn style_images_keep_what_they_were_set_with() {
    let (_runtime, map) = styled_map();
    let mut marker = mln_style::image::Impl::new(
        "marker".to_owned(),
        image(2, 1),
        2.0,
        true,
        vec![(0.0, 1.0)],
        Vec::new(),
        None,
        None,
        None,
    )
    .unwrap();
    marker.keep_unused = true;
    wait(map.set_style_image(marker));
    let read = wait(map.style_image("marker")).unwrap();
    assert_eq!((read.pixel_ratio, read.sdf, read.keep_unused), (2.0, true, true));
    assert_eq!(read.stretch_x, [(0.0, 1.0)]);
    wait(map.remove_style_image("marker"));
    assert!(wait(map.style_image("marker")).is_none());
    let missing = wait_err(map.remove_style_image("marker"));
    assert_eq!(
        (missing.kind, missing.message.as_str()),
        (ErrorKind::NotFound, "style image \"marker\" does not exist")
    );
}

#[test]
fn a_layer_move_publishes_one_render_update() {
    let (_runtime, map) = styled_map();
    wait(map.add_style_layer_json(r#"{"id":"a","type":"background"}"#, None));
    wait(map.add_style_layer_json(r#"{"id":"b","type":"background"}"#, None));
    let before = map.snapshot().unwrap().update_generation;
    let moved = wait(map.move_style_layer("a", None));
    assert_eq!((moved.render_update_generation, map.snapshot().unwrap().update_generation), (before + 1, before + 1));
}

#[test]
fn a_command_inside_a_group_names_the_update_that_its_end_shows() {
    let (_runtime, map) = styled_map();
    let before = map.snapshot().unwrap().update_generation;
    wait(map.begin_command_group());
    wait(map.begin_command_group());
    let added = wait(map.add_style_layer_json(r#"{"id":"a","type":"background"}"#, None));
    assert_eq!(added.render_update_generation, before + 1);
    assert_eq!(wait(map.end_command_group()).render_update_generation, before + 1);
    assert_eq!(map.snapshot().unwrap().update_generation, before);
    wait(map.add_style_layer_json(r#"{"id":"b","type":"background"}"#, None));
    assert_eq!(wait(map.end_command_group()).render_update_generation, before + 1);
    assert_eq!(map.snapshot().unwrap().update_generation, before + 1);
}
