// Ports test/storage/mbtiles_file_source.test.cpp. The fixture is read in
// place (C++: relative to the working directory).

use std::cell::RefCell;
use std::rc::Rc;

use mln_actor::RunLoop;
use mln_util::tileset::Scheme;

use super::{cpp_path, req, temp_path};
use crate::file_source::FileSource;
use crate::mbtiles_file_source::MBTilesFileSource;
use crate::resource::{Kind, LoadingMethod, Resource};
use crate::resource_options::ResourceOptions;
use crate::response::{Reason, Response};
use crate::sqlite3::{Database, OpenFlag};

fn to_absolute_url(file_name: &str) -> String {
    let path = cpp_path("test/fixtures/storage/mbtiles").join(file_name);
    format!("mbtiles://{}", path.display())
}

fn mbtiles_file_source() -> MBTilesFileSource {
    MBTilesFileSource::new(&ResourceOptions::default_options())
}

/// Requests `resource`, runs the loop until the response and returns it.
fn request(fs: &MBTilesFileSource, resource: Resource) -> Response {
    let run_loop = RunLoop::new();
    let req = req();
    let result = Rc::new(RefCell::new(None));
    let (r, out, stop) = (req.clone(), result.clone(), run_loop.clone());
    *req.borrow_mut() = Some(fs.request(
        &resource,
        Box::new(move |res: Response| {
            r.borrow_mut().take();
            *out.borrow_mut() = Some(res);
            stop.stop();
        }),
    ));
    run_loop.run();

    result.borrow_mut().take().expect("response")
}

// gtest: MBTilesFileSource.AcceptsURL
#[test]
fn accepts_url() {
    let mbtiles = mbtiles_file_source();
    assert!(mbtiles.can_request(&Resource::style("mbtiles:///test")));
    assert!(!mbtiles.can_request(&Resource::style("mbtile://test")));
    assert!(!mbtiles.can_request(&Resource::style("mbtiles:")));
    assert!(!mbtiles.can_request(&Resource::style("")));
}

// mbtiles paths must be absolute
// gtest: MBTilesFileSource.AbsolutePath
#[test]
fn absolute_path() {
    let mbtiles = mbtiles_file_source();
    let res = request(&mbtiles, Resource::new(Kind::Unknown, "mbtiles://not_absolute"));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::Other, error.reason);
    assert!(error.message.contains("absolute"));
    assert!(res.data.is_none());
}

// An empty path is not absolute
// gtest: MBTilesFileSource.EmptyPath
#[test]
fn empty_path() {
    let mbtiles = mbtiles_file_source();
    let res = request(&mbtiles, Resource::new(Kind::Unknown, "mbtiles://"));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::Other, error.reason);
    assert!(error.message.contains("absolute"));
}

// Nonexistent mbtiles file raises error
// gtest: MBTilesFileSource.NonExistentFile
#[test]
fn non_existent_file() {
    let mbtiles = mbtiles_file_source();
    let res = request(&mbtiles, Resource::new(Kind::Unknown, to_absolute_url("does_not_exist")));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::NotFound, error.reason);
    assert!(error.message.contains("path not found"));
    assert!(res.data.is_none());
}

// A file that is not an mbtiles database raises error
// gtest: MBTilesFileSource.NotADatabase
#[test]
fn not_a_database() {
    let mbtiles = mbtiles_file_source();
    let path = cpp_path("test/fixtures/storage/pmtiles/geography-class-png.pmtiles");
    let res = request(&mbtiles, Resource::new(Kind::Unknown, format!("mbtiles://{}", path.display())));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::Other, error.reason);
    assert!(res.data.is_none());
}

// Metadata that does not parse raises error
// gtest: MBTilesFileSource.BadMetadata
#[test]
fn bad_metadata() {
    let path = temp_path("mbtiles_bad_metadata.mbtiles");
    Database::open(&path, OpenFlag::ReadWriteCreate)
        .and_then(|db| {
            db.exec(
                "CREATE TABLE metadata (name TEXT, value TEXT); INSERT INTO metadata VALUES ('minzoom', 'zero'), \
                 ('maxzoom', '1');",
            )
        })
        .expect("create the database");

    let mbtiles = mbtiles_file_source();
    let res = request(&mbtiles, Resource::new(Kind::Unknown, format!("mbtiles://{path}")));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::Other, error.reason);
    assert!(res.data.is_none());
}

// Existing mbtiles file default request returns TileJSON
// gtest: MBTilesFileSource.TileJSON
#[test]
fn tile_json() {
    let mbtiles = mbtiles_file_source();
    let res = request(&mbtiles, Resource::new(Kind::Unknown, to_absolute_url("geography-class-png.mbtiles")));
    assert_eq!(None, res.error);
    let data = res.data.expect("data");
    // basic test that TileJSON included a tile URL
    assert!(String::from_utf8_lossy(&data).contains("geography-class-png.mbtiles?file={x}/{y}/{z}"));
}

// Existing tiles return tile data
// gtest: MBTilesFileSource.Tile
#[test]
fn tile() {
    let mbtiles = mbtiles_file_source();
    let res = request(
        &mbtiles,
        Resource::tile(
            &to_absolute_url("geography-class-png.mbtiles?file={z}/{x}/{y}.png"),
            1.0,
            0,
            0,
            0,
            Scheme::XYZ,
            LoadingMethod::All,
        ),
    );
    assert_eq!(None, res.error);
    assert!(res.data.is_some());
    assert!(!res.no_content);
}

// Nonexistent tiles do not raise errors, they simply return no content
// gtest: MBTilesFileSource.NonExistentTile
#[test]
fn non_existent_tile() {
    let mbtiles = mbtiles_file_source();
    let res = request(
        &mbtiles,
        Resource::tile(
            &to_absolute_url("geography-class-png.mbtiles?file={z}/{x}/{y}.png"),
            1.0,
            0,
            0,
            4,
            Scheme::XYZ,
            LoadingMethod::All,
        ),
    );
    assert_eq!(None, res.error);
    assert!(res.data.is_none());
    assert!(res.no_content);
}

// Tiles from a nonexistent mbtiles file raise errors
// gtest: MBTilesFileSource.NonExistentFileTile
#[test]
fn non_existent_file_tile() {
    let mbtiles = mbtiles_file_source();
    let res = request(
        &mbtiles,
        Resource::tile(
            &to_absolute_url("does_not_exist?file={z}/{x}/{y}.png"),
            1.0,
            0,
            0,
            0,
            Scheme::XYZ,
            LoadingMethod::All,
        ),
    );
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::Other, error.reason);
    assert!(res.data.is_none());
}
