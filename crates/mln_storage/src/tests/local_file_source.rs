// Ports test/storage/local_file_source.test.cpp. The fixtures are read in
// place (C++: relative to the working directory).

use std::path::{Path, PathBuf};

use mln_actor::RunLoop;
use mln_util::constants::FILE_PROTOCOL;

use super::{cpp_path, req, temp_path};
use crate::file_source::FileSource;
use crate::local_file_source::LocalFileSource;
use crate::resource::{Kind, Resource};
use crate::resource_options::ResourceOptions;
use crate::response::{Reason, Response};

/// `fileURL(path)`: percent-encodes everything but unreserved characters,
/// '/' and ':'.
fn file_url(path: &Path) -> String {
    let bytes = path.as_os_str().as_encoded_bytes();
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::new();
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || b"-._~/:".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0xF) as usize] as char);
        }
    }

    format!("{FILE_PROTOCOL}{}{encoded}", if encoded.starts_with('/') { "" } else { "/" })
}

fn assets() -> PathBuf {
    cpp_path("test/fixtures/storage/assets")
}

fn to_absolute_url(file_name: &str) -> String {
    // `std::filesystem::path / ""` appends a separator.
    let mut path = assets().into_os_string();
    path.push("/");
    path.push(file_name);
    file_url(Path::new(&path))
}

fn local_file_source() -> LocalFileSource {
    LocalFileSource::new(&ResourceOptions::default_options())
}

/// Requests `resource`, runs the loop until the response and returns it
/// (the C++ tests check it inside the callback, after `req.reset()`).
fn request(fs: &LocalFileSource, resource: Resource) -> Response {
    let run_loop = RunLoop::new();
    let req = req();
    let result = std::rc::Rc::new(std::cell::RefCell::new(None));
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

fn data(res: &Response) -> &[u8] {
    res.data.as_deref().map(Vec::as_slice).expect("data")
}

// gtest: LocalFileSource.AcceptsURL
#[test]
fn accepts_url() {
    let fs = local_file_source();
    assert!(fs.can_request(&Resource::style("file://empty")));
    assert!(fs.can_request(&Resource::style("file:///test")));
    assert!(!fs.can_request(&Resource::style("flie://foo")));
    assert!(!fs.can_request(&Resource::style("file:")));
    assert!(!fs.can_request(&Resource::style("style.json")));
    assert!(!fs.can_request(&Resource::style("")));
}

// gtest: LocalFileSource.EmptyFile
#[test]
fn empty_file() {
    let fs = local_file_source();
    let res = request(&fs, Resource::new(Kind::Unknown, to_absolute_url("empty")));
    assert_eq!(None, res.error);
    assert_eq!(b"", data(&res));
}

// gtest: LocalFileSource.NonEmptyFile
#[test]
fn non_empty_file() {
    let fs = local_file_source();
    let res = request(&fs, Resource::new(Kind::Unknown, to_absolute_url("nonempty")));
    assert_eq!(None, res.error);
    assert_eq!(b"content is here\n", data(&res));
}

// gtest: LocalFileSource.NonASCIIPath
#[test]
fn non_ascii_path() {
    // C++: <temp_directory_path>/mln-local-file-source-non-ascii.
    let temp = temp_path("mln-local-file-source-non-ascii");
    let root = PathBuf::from(&*temp);

    let directory = root.join("ké 地図");
    std::fs::create_dir_all(&directory).unwrap();

    let body = "content is here\n";
    let file = directory.join("スタイル.json");
    std::fs::write(&file, body).unwrap();

    let fs = local_file_source();
    let res = request(&fs, Resource::style(&file_url(&file)));
    assert_eq!(None, res.error);
    assert_eq!(body.as_bytes(), data(&res));
}

// C++ runs this one on Windows only (`#if defined(_WIN32)`), where invalid
// UTF-8 fails the path conversion; elsewhere the path is not found. Both
// give an error without data.
// gtest: LocalFileSource.InvalidUTF8Path
#[test]
fn invalid_utf8_path() {
    let fs = local_file_source();
    let res = request(&fs, Resource::style("file:///C:/%FF.json"));
    assert!(res.error.is_some());
    assert!(res.data.is_none());
}

// gtest: LocalFileSource.PartialFile
#[test]
fn partial_file() {
    let fs = local_file_source();

    let mut resource = Resource::new(Kind::Unknown, to_absolute_url("nonempty"));
    resource.data_range = Some((4, 12));

    let res = request(&fs, resource);
    assert_eq!(None, res.error);
    assert_eq!(b"ent is he", data(&res));
}

// Port-only: a range past the end is zero-padded, as C++; one too large to
// allocate (a corrupt PMTiles header) is a read error (C++ throws
// std::length_error out of the request, uncaught).
#[test]
fn ranges_past_the_end() {
    let fs = local_file_source();
    let mut resource = Resource::new(Kind::Unknown, to_absolute_url("nonempty"));
    resource.data_range = Some((12, 20));
    let res = request(&fs, resource.clone());
    assert_eq!(None, res.error);
    assert_eq!(b"ere\n\0\0\0\0\0", data(&res));

    resource.data_range = Some((4, u64::MAX - 1));
    let res = request(&fs, resource);
    assert_eq!(Reason::Other, res.error.as_ref().expect("error").reason);
}

// gtest: LocalFileSource.NonExistentFile
#[test]
fn non_existent_file() {
    let fs = local_file_source();
    let res = request(&fs, Resource::new(Kind::Unknown, to_absolute_url("does_not_exist")));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::NotFound, error.reason);
    assert!(res.data.is_none());
    assert!(error.message.starts_with("Cannot find file "), "{}", error.message);
    assert!(error.message.ends_with("does_not_exist"), "{}", error.message);
}

// gtest: LocalFileSource.InvalidURL
#[test]
fn invalid_url() {
    let fs = local_file_source();
    let res = request(&fs, Resource::new(Kind::Unknown, "test://wrong-scheme"));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::Other, error.reason);
    assert_eq!("Invalid file URL", error.message);
    assert!(res.data.is_none());
}

// gtest: LocalFileSource.ReadDirectory
#[test]
fn read_directory() {
    let fs = local_file_source();
    let res = request(&fs, Resource::new(Kind::Unknown, to_absolute_url("directory")));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::NotFound, error.reason);
    assert!(res.data.is_none());
    assert!(error.message.starts_with("Cannot read directory "), "{}", error.message);
    assert!(error.message.ends_with("directory"), "{}", error.message);
}

// gtest: LocalFileSource.URLEncoding
#[test]
fn url_encoding() {
    let fs = local_file_source();
    let url = to_absolute_url("") + "%6eonempty";
    let res = request(&fs, Resource::new(Kind::Unknown, url));
    assert_eq!(None, res.error);
    assert_eq!(b"content is here\n", data(&res));
}

// gtest: LocalFileSource.URLLimit
#[test]
fn url_limit() {
    // PATH_MAX (limits.h; 4096 on Linux).
    const PATH_MAX: usize = 4096;
    let length = PATH_MAX - to_absolute_url("").len();
    let fs = local_file_source();
    let url = "x".repeat(length);

    let res = request(&fs, Resource::new(Kind::Unknown, to_absolute_url(&url)));
    let error = res.error.as_ref().expect("error");
    assert_eq!(Reason::Other, error.reason);
    assert!(res.data.is_none());
}
