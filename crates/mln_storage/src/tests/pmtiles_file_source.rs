// Ports test/storage/pmtiles_file_source.test.cpp. The fixtures are read in
// place (C++: relative to the working directory).

use std::cell::RefCell;
use std::rc::Rc;

use mln_actor::RunLoop;
use mln_util::constants::{FILE_PROTOCOL, PMTILES_PROTOCOL};
use mln_util::tileset::Scheme;

use super::{cpp_path, req};
use crate::file_source::FileSource;
use crate::file_source_manager::FileSourceManager;
use crate::pmtiles_file_source::PMTilesFileSource;
use crate::resource::{Kind, LoadingMethod, Resource};
use crate::resource_options::ResourceOptions;
use crate::response::{Reason, Response};

fn to_absolute_url(file_name: &str) -> String {
    let path = cpp_path("test/fixtures/storage/pmtiles").join(file_name);
    format!("{PMTILES_PROTOCOL}{FILE_PROTOCOL}{}", path.display())
}

fn pmtiles_file_source() -> Rc<PMTilesFileSource> {
    Rc::new(PMTilesFileSource::new(&FileSourceManager::new(), &ResourceOptions::default_options()))
}

fn tile(url: &str, z: i8) -> Resource {
    Resource::tile(url, 1.0, 0, 0, z, Scheme::XYZ, LoadingMethod::All)
}

/// Requests `resource`, runs the loop until the response and returns it.
fn request(fs: &PMTilesFileSource, resource: Resource) -> Response {
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

// gtest: PMTilesFileSource.AcceptsURL
#[test]
fn accepts_url() {
    let pmtiles = pmtiles_file_source();
    assert!(pmtiles.can_request(&Resource::style("pmtiles:///test")));
    assert!(!pmtiles.can_request(&Resource::style("pmtile://test")));
    assert!(!pmtiles.can_request(&Resource::style("pmtiles:")));
    assert!(!pmtiles.can_request(&Resource::style("")));
}

// Nonexistent pmtiles file raises error
// gtest: PMTilesFileSource.NonExistentFile
#[test]
fn non_existent_file() {
    let pmtiles = pmtiles_file_source();
    let res = request(&pmtiles, Resource::new(Kind::Unknown, to_absolute_url("does_not_exist")));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::NotFound, error.reason);
    assert!(error.message.contains("Cannot find file"), "{}", error.message);
    assert!(res.data.is_none());
}

// Existing pmtiles file default request returns TileJSON
// gtest: PMTilesFileSource.TileJSON
#[test]
fn tile_json() {
    let pmtiles = pmtiles_file_source();
    let res = request(&pmtiles, Resource::new(Kind::Unknown, to_absolute_url("geography-class-png.pmtiles")));
    assert_eq!(None, res.error);
    let data = res.data.expect("data");
    // basic test that TileJSON included a tile URL
    assert!(String::from_utf8_lossy(&data).contains("geography-class-png.pmtiles"));
}

// Existing tiles return tile data
// gtest: PMTilesFileSource.Tile
#[test]
fn tile_data() {
    let pmtiles = pmtiles_file_source();
    let res = request(&pmtiles, tile(&to_absolute_url("geography-class-png.pmtiles"), 0));
    assert_eq!(None, res.error);
    assert!(res.data.is_some());
    assert!(!res.no_content);
}

// Nonexistent tiles do not raise errors, they simply return no content
// gtest: PMTilesFileSource.NonExistentTile
#[test]
fn non_existent_tile() {
    let pmtiles = pmtiles_file_source();
    let res = request(&pmtiles, tile(&to_absolute_url("geography-class-png.pmtiles"), 4));
    assert_eq!(None, res.error);
    assert!(res.data.is_none());
    assert!(res.no_content);
}

// A tile whose bytes start with gzip magic but are otherwise corrupt must
// yield an error response — the decompression failure must not propagate as
// an exception.
// gtest: PMTilesFileSource.CorruptGzipTile
#[test]
fn corrupt_gzip_tile() {
    let pmtiles = pmtiles_file_source();
    let res = request(&pmtiles, tile(&to_absolute_url("corrupt-gzip-tile.pmtiles"), 0));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::Other, error.reason);
    assert!(error.message.contains("Error decompressing PMTiles tile:"), "{}", error.message);
}

// Metadata whose bytes start with gzip magic but are otherwise corrupt must
// yield an error response — the decompression failure must not propagate as
// an exception.
// gtest: PMTilesFileSource.CorruptGzipMetadata
#[test]
fn corrupt_gzip_metadata() {
    let pmtiles = pmtiles_file_source();
    let res = request(&pmtiles, Resource::new(Kind::Unknown, to_absolute_url("corrupt-gzip-metadata.pmtiles")));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::Other, error.reason);
    assert!(error.message.contains("Error decompressing PMTiles metadata:"), "{}", error.message);
}

// Regression test for https://github.com/maplibre/maplibre-native/issues/3690
// — the loading method for PMTiles sub-requests changed from `Network` to
// `All` so that http(s)-hosted archives flow through the ambient cache. The
// file:// path bypasses the cache by design, so this test can only confirm
// that issuing the same tile twice still succeeds; a true cache-hit
// assertion requires an http(s) PMTiles fixture and a mock server.
// gtest: PMTilesFileSource.RepeatedTileRequest
#[test]
fn repeated_tile_request() {
    let run_loop = RunLoop::new();
    let pmtiles = pmtiles_file_source();

    let req1 = req();
    let req2 = req();

    let (r1, r2, fs, stop) = (req1.clone(), req2.clone(), pmtiles.clone(), run_loop.clone());
    *req1.borrow_mut() = Some(pmtiles.request(
        &tile(&to_absolute_url("geography-class-png.pmtiles"), 0),
        Box::new(move |res1: Response| {
            r1.borrow_mut().take();
            assert_eq!(None, res1.error);
            let first_size = res1.data.as_ref().expect("data").len();

            let (r2c, stop) = (r2.clone(), stop.clone());
            *r2.borrow_mut() = Some(fs.request(
                &tile(&to_absolute_url("geography-class-png.pmtiles"), 0),
                Box::new(move |res2: Response| {
                    r2c.borrow_mut().take();
                    assert_eq!(None, res2.error);
                    assert_eq!(first_size, res2.data.as_ref().expect("data").len());
                    stop.stop();
                }),
            ));
        }),
    ));

    run_loop.run();
}

// An archive whose header flags tile_compression=GZIP but whose individual
// tiles are stored without compression must be served without error
// (uncompressed bytes passed through as-is).
// gtest: PMTilesFileSource.UncompressedTile
#[test]
fn uncompressed_tile() {
    let pmtiles = pmtiles_file_source();
    let res = request(&pmtiles, tile(&to_absolute_url("uncompressed-tiles.pmtiles"), 0));
    assert_eq!(None, res.error);
    assert!(res.data.is_some());
    assert!(!res.no_content);
}
