// Ports test/storage/sync_file_source.test.cpp: a file source that answers
// inside `request`, under a Map that loads a style through it. A separate
// test target because it needs mln_map and mln_headless, which
// depend on mln_storage.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use mln_actor::RunLoop;
use mln_headless::HeadlessFrontend;
use mln_map::{Map, MapOptions, NullMapObserver};
use mln_storage::response::{Error, Reason};
use mln_storage::{AsyncRequest, Callback, FileSource, Resource, ResourceOptions, Response};
use mln_util::size::Size;

fn read_file(relative: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(relative);
    std::fs::read(&path).unwrap_or_else(|e| panic!("Cannot read file {}: {e}", path.display()))
}

/// C++ returns `nullptr` as the request; the port's requests are never null,
/// so it returns one that does nothing.
struct NoRequest;

impl AsyncRequest for NoRequest {}

/// `SyncFileSource`.
#[derive(Default)]
struct SyncFileSource {
    assets: Mutex<HashMap<String, Arc<Vec<u8>>>>,
}

impl SyncFileSource {
    fn add(&self, key: &str, data: Vec<u8>) {
        self.assets.lock().unwrap().entry(key.to_owned()).or_insert(Arc::new(data));
    }
}

impl FileSource for SyncFileSource {
    fn request(&self, resource: &Resource, mut callback: Callback) -> Box<dyn AsyncRequest> {
        let mut response = Response::new();
        let data = self.assets.lock().unwrap().get(&resource.url).cloned();
        match data {
            None => {
                response.error = Some(Box::new(Error::new(Reason::NotFound, format!("Not Found: {}", resource.url))));
            }
            Some(data) => response.data = Some(data),
        }
        callback(response);
        Box::new(NoRequest)
    }

    fn can_request(&self, _resource: &Resource) -> bool {
        true
    }

    fn set_resource_options(&self, _options: ResourceOptions) {}

    fn get_resource_options(&self) -> ResourceOptions {
        ResourceOptions::default()
    }
}

// gtest: SyncFileSource.LoadSyncRender
#[test]
fn load_sync_render() {
    let _run_loop = RunLoop::new();
    let fs = Arc::new(SyncFileSource::default());
    fs.add("maptiler://maps/streets", read_file("test/fixtures/resources/source_vector.json"));
    fs.add("maptiler://sprites/streets/sprite", read_file("test/fixtures/resources/sprite.png"));
    let frontend = HeadlessFrontend::new(Size::new(512, 512), 1.0);
    let mut map = Map::with_file_source(&frontend, Rc::new(NullMapObserver), fs, MapOptions::new()).unwrap();
    let style = String::from_utf8(read_file("test/fixtures/resources/style_vector.json")).unwrap();
    map.get_style_mut().load_json(&style);
}
