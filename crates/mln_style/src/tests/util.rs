//! Test helpers: the C++ `FixtureLog` (per thread, see mln_util::logging)
//! and `convertJSON`.

use std::sync::{Arc, Mutex};

use mln_util::logging::{Event, EventSeverity, Log, Observer};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogMessage {
    pub severity: EventSeverity,
    pub event: Event,
    pub code: i64,
    pub msg: String,
}

struct Recorder(Arc<Mutex<Vec<(LogMessage, bool)>>>);

impl Observer for Recorder {
    fn on_record(&self, severity: EventSeverity, event: Event, code: i64, msg: &str) -> bool {
        self.0.lock().unwrap().push((LogMessage { severity, event, code, msg: msg.to_owned() }, false));
        true
    }
}

/// `FixtureLog`: records this thread's log messages while alive.
pub struct FixtureLog {
    messages: Arc<Mutex<Vec<(LogMessage, bool)>>>,
}

impl FixtureLog {
    pub fn new() -> Self {
        let messages = Arc::new(Mutex::new(Vec::new()));
        Log::set_thread_observer(Some(Box::new(Recorder(messages.clone()))));
        FixtureLog { messages }
    }

    /// `count(message)`: the number of matching messages, which are then
    /// checked (C++ ignores the code when it is -1 in the pattern).
    pub fn count(&self, severity: EventSeverity, event: Event, msg: &str) -> usize {
        let mut messages = self.messages.lock().unwrap();
        let mut count = 0;
        for (m, checked) in messages.iter_mut() {
            if m.severity == severity && m.event == event && m.msg == msg {
                *checked = true;
                count += 1;
            }
        }
        count
    }

    /// `unchecked()`: the messages no `count` call has matched.
    pub fn unchecked(&self) -> Vec<LogMessage> {
        self.messages.lock().unwrap().iter().filter(|(_, checked)| !checked).map(|(m, _)| m.clone()).collect()
    }

    /// `empty()`.
    pub fn is_empty(&self) -> bool {
        self.messages.lock().unwrap().is_empty()
    }

    pub fn messages(&self) -> Vec<LogMessage> {
        self.messages.lock().unwrap().iter().map(|(m, _)| m.clone()).collect()
    }
}

impl Drop for FixtureLog {
    fn drop(&mut self) {
        Log::set_thread_observer(None);
    }
}

/// `mln::util::read_file(path)` for a path relative to the C++ repository
/// (in the copy of the files these tests read, testdata/).
pub fn read_file(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// `mln::util::read_file(path)` as bytes.
pub fn read_file_bytes(relative: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(relative);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// `convertJSON<T>(json, error, args...)` (style/conversion/json.hpp).
pub fn parse_json(json: &str) -> serde_json::Value {
    serde_json::from_str(json).expect("test JSON must parse")
}

pub use mln_conversion::value_to_json;

/// Ports test/include/mln/test/stub_geometry_tile_feature.hpp.
pub struct StubGeometryTileFeature {
    pub properties: feature::PropertyMap,
    pub id: feature::Identifier,
    pub type_: mln_util::geometry::FeatureType,
    pub geometry: mln_tile_id::geometry_tile_data::GeometryCollection,
}

impl StubGeometryTileFeature {
    pub fn new(properties: feature::PropertyMap) -> Self {
        StubGeometryTileFeature {
            properties,
            id: feature::Identifier::Null,
            type_: mln_util::geometry::FeatureType::Point,
            geometry: Default::default(),
        }
    }

    /// `StubGeometryTileFeature(FeatureType, GeometryCollection)`.
    pub fn with_geometry(
        type_: mln_util::geometry::FeatureType,
        geometry: mln_tile_id::geometry_tile_data::GeometryCollection,
    ) -> Self {
        StubGeometryTileFeature { properties: Default::default(), id: feature::Identifier::Null, type_, geometry }
    }

    /// `StubGeometryTileFeature(id, type, geometry, properties)` (no geometry).
    pub fn with_id_type(
        id: feature::Identifier,
        type_: mln_util::geometry::FeatureType,
        properties: feature::PropertyMap,
    ) -> Self {
        StubGeometryTileFeature { properties, id, type_, geometry: Default::default() }
    }
}

impl mln_tile_id::geometry_tile_data::GeometryTileFeature for StubGeometryTileFeature {
    fn get_type(&self) -> mln_util::geometry::FeatureType {
        self.type_
    }

    fn get_value(&self, key: &str) -> Option<feature::Value> {
        self.properties.get(key).cloned()
    }

    fn get_properties(&self) -> &feature::PropertyMap {
        &self.properties
    }

    fn get_id(&self) -> feature::Identifier {
        self.id.clone()
    }

    fn get_geometries(&self) -> &mln_tile_id::geometry_tile_data::GeometryCollection {
        &self.geometry
    }
}

/// `PropertyMap{{key, value}}`.
pub fn property_map(entries: &[(&str, feature::Value)]) -> feature::PropertyMap {
    entries.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
}
