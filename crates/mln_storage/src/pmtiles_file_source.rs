// Ports include/mln/storage/pmtiles_file_source.hpp and
// platform/default/src/mln/storage/pmtiles_file_source.cpp.
//
// The work runs on the "PMTilesFileSource" thread (`util::Thread`) as in
// C++: header, metadata and directories are read with range requests through
// the ResourceLoader file source, whose callbacks come back to this thread.
// `tasks` is keyed by a request id instead of the caller's `AsyncRequest*`;
// as in C++, a sub-request replaces the previous one of the same caller and
// nothing removes them when the caller cancels. The TileJSON document is
// built as a rapidjson document would be (members in insertion order,
// duplicates kept) and written with //vendor/rapidjson's `Writer`.
//
// The resource options are shared with the thread behind a mutex
// (C++: in the thread object, read back with a blocking `ask`).
//
// TODO(port): makeThreadPrioritySetter(EXPERIMENTAL_THREAD_PRIORITY_FILE).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mln_actor::{ActorRef, Thread};
use mln_util::compression::{decompress, is_compressed};
use mln_util::constants::{FILE_PROTOCOL, PMTILES_PROTOCOL};
use mln_util::sync::MutexExt;
use mln_vec_map::VecMap;
use pmtiles::{EntryV3, HeaderV3};
use serde_json::Value as Json;

use crate::async_request::AsyncRequest;
use crate::file_source::{Callback, FileSource, FileSourceType};
use crate::file_source_manager::FileSourceManager;
use crate::file_source_request::{FileSourceRequest, FileSourceRequestState};
use crate::json_document::{Document, strtol};
use crate::resource::{Kind, LoadingMethod, Resource};
use crate::resource_options::ResourceOptions;
use crate::response::{Error, Reason, Response};

// https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md#3-header
const PMTILES_HEADER_OFFSET: u64 = 0;
const PMTILES_HEADER_LENGTH: u64 = 127;

// To avoid allocating lots of memory with PMTiles directory caching, set a
// limit so it doesn't grow unlimited
const MAX_DIRECTORY_CACHE_ENTRIES: usize = 100;

// temporary, remove this when it's available in `pmtiles.hpp`
const TILETYPE_MLT: u8 = 0x6;

fn accepts_url(url: &str) -> bool {
    url.starts_with(PMTILES_PROTOCOL)
}

fn extract_url(url: &str) -> String {
    url[PMTILES_PROTOCOL.len().min(url.len())..].to_owned()
}

type ErrorPtr = Option<Box<Error>>;
/// `AsyncCallback` (`std::function<void(std::unique_ptr<Response::Error>)>`).
type AsyncCallback = Rc<dyn Fn(ErrorPtr)>;
/// `AsyncTileCallback`.
type AsyncTileCallback = Rc<dyn Fn((u64, u32), ErrorPtr)>;

fn error(reason: Reason, message: impl Into<String>) -> ErrorPtr {
    Some(Box::new(Error::new(reason, message)))
}

fn json_double(d: f64) -> Json {
    serde_json::Number::from_f64(d).map_or(Json::Null, Json::Number)
}

fn send(req: &ActorRef<FileSourceRequestState>, response: Response) {
    req.invoke(move |request: &FileSourceRequestState| request.set_response(response));
}

/// The members of `PMTilesFileSource::Impl`.
struct State {
    resource_options: Arc<Mutex<ResourceOptions>>,
    file_sources: FileSourceManager,

    file_source: RefCell<Option<Arc<dyn FileSource>>>,
    header_cache: RefCell<VecMap<String, HeaderV3>>,
    metadata_cache: RefCell<VecMap<String, String>>,
    directory_cache: RefCell<VecMap<String, VecMap<String, Vec<EntryV3>>>>,
    directory_cache_control: RefCell<VecMap<String, Vec<String>>>,
    tasks: RefCell<VecMap<u64, Box<dyn AsyncRequest>>>,
}

impl State {
    fn get_file_source(&self) -> Arc<dyn FileSource> {
        let mut file_source = self.file_source.borrow_mut();
        file_source
            .get_or_insert_with(|| {
                let resource_options = self.resource_options.lock_unpoisoned().clone();
                self.file_sources
                    .get_file_source(FileSourceType::ResourceLoader, &resource_options)
                    .expect("ResourceLoader file source")
            })
            .clone()
    }

    /// `tasks[req] = getFileSource()->request(resource, callback)`.
    fn request(self: &Rc<Self>, req: u64, resource: &Resource, callback: impl FnMut(Response) + 'static) {
        let file_source = self.get_file_source();
        let request = file_source.request(resource, Box::new(callback));
        let previous = self.tasks.borrow_mut().insert(req, request);
        drop(previous);
    }

    fn get_header(self: &Rc<Self>, url: &str, req: u64, callback: AsyncCallback) {
        if self.header_cache.borrow().contains_key(url) {
            callback(None);
            return;
        }

        let mut resource = Resource::new(Kind::Source, url);
        resource.loading_method = LoadingMethod::All;

        resource.data_range = Some((PMTILES_HEADER_OFFSET, PMTILES_HEADER_OFFSET + PMTILES_HEADER_LENGTH - 1));

        let this = self.clone();
        let url = url.to_owned();
        self.request(req, &resource, move |response: Response| {
            if let Some(response_error) = &response.error {
                let mut message = format!("Error fetching PMTiles header: {}", response_error.message);

                if response_error.message.is_empty() && response_error.reason == Reason::NotFound {
                    if let Some(path) = url.strip_prefix(FILE_PROTOCOL) {
                        message += &format!("path not found: {path}");
                    } else {
                        message += &format!("url not found: {url}");
                    }
                }

                callback(error(response_error.reason, message));

                return;
            }

            if response.not_modified {
                return;
            }

            let Some(data) = &response.data else {
                callback(error(Reason::Other, "PMTiles header response has no data"));
                return;
            };

            let header = pmtiles::deserialize_header(&data[..data.len().min(127)]).map_err(|e| e.to_string()).and_then(
                |header| {
                    let supported = |c: u8| c == pmtiles::COMPRESSION_NONE || c == pmtiles::COMPRESSION_GZIP;
                    if !supported(header.internal_compression) || !supported(header.tile_compression) {
                        return Err("Compression method not supported".to_owned());
                    }
                    Ok(header)
                },
            );
            match header {
                Ok(header) => {
                    this.header_cache.borrow_mut().entry(url.clone()).or_insert(header);
                    callback(None);
                }
                Err(e) => callback(error(Reason::Other, format!("Error parsing PMTiles header: {e}"))),
            }
        });
    }

    fn get_metadata(self: &Rc<Self>, url: &str, req: u64, callback: AsyncCallback) {
        if self.metadata_cache.borrow().contains_key(url) {
            callback(None);
            return;
        }

        let this = self.clone();
        let url = url.to_owned();
        self.get_header(
            &url.clone(),
            req,
            Rc::new(move |error_ptr: ErrorPtr| {
                if error_ptr.is_some() {
                    callback(error_ptr);
                    return;
                }

                let header = this.header_cache.borrow()[&url];

                let parse_callback = {
                    let (this, url, callback) = (this.clone(), url.clone(), callback.clone());
                    move |data: &[u8]| {
                        let mut doc = if data.is_empty() { Document::default() } else { Document::parse_object(data) };

                        doc.add_member("tilejson", Json::String("3.0.0".to_owned()));

                        if !doc.has_member("scheme") {
                            doc.add_member("scheme", Json::Null);
                        }

                        *doc.get_mut("scheme") = Json::String("xyz".to_owned());

                        if !doc.has_member("tiles") {
                            doc.add_member("tiles", Json::Null);
                        }

                        if !doc.get_mut("tiles").is_array() {
                            *doc.get_mut("tiles") = Json::Array(vec![Json::String(format!("{PMTILES_PROTOCOL}{url}"))]);
                        }

                        // Translate tile type field to source encoding.
                        if header.tile_type == TILETYPE_MLT {
                            doc.add_member("encoding", Json::String("mlt".to_owned()));
                        }

                        if !doc.has_member("bounds") {
                            doc.add_member("bounds", Json::Null);
                        }

                        if !doc.get_mut("bounds").is_array() {
                            *doc.get_mut("bounds") = Json::Array(vec![
                                json_double(f64::from(header.min_lon_e7) / 1e7),
                                json_double(f64::from(header.min_lat_e7) / 1e7),
                                json_double(f64::from(header.max_lon_e7) / 1e7),
                                json_double(f64::from(header.max_lat_e7) / 1e7),
                            ]);
                        }

                        if !doc.has_member("center") {
                            doc.add_member("center", Json::Null);
                        }

                        if !doc.get_mut("center").is_array() {
                            *doc.get_mut("center") = Json::Array(vec![
                                json_double(f64::from(header.center_lon_e7) / 1e7),
                                json_double(f64::from(header.center_lat_e7) / 1e7),
                                Json::from(header.center_zoom),
                            ]);
                        }

                        for (name, zoom) in [("minzoom", header.min_zoom), ("maxzoom", header.max_zoom)] {
                            if !doc.has_member(name) {
                                doc.add_member(name, Json::Null);
                            }

                            let value = doc.get_mut(name);

                            if let Json::String(s) = value {
                                *value = Json::from(strtol(s) as i32);
                            }

                            if !value.is_number() {
                                *value = Json::from(zoom);
                            }
                        }

                        let metadata = doc.serialize();
                        this.metadata_cache.borrow_mut().entry(url.clone()).or_insert(metadata);

                        callback(None);
                    }
                };

                if header.json_metadata_bytes > 0 {
                    let mut resource = Resource::new(Kind::Source, url.clone());
                    resource.loading_method = LoadingMethod::All;
                    resource.data_range = Some((
                        header.json_metadata_offset,
                        header.json_metadata_offset.wrapping_add(header.json_metadata_bytes).wrapping_sub(1),
                    ));

                    let callback = callback.clone();
                    this.request(req, &resource, move |response_metadata: Response| {
                        if let Some(e) = &response_metadata.error {
                            callback(error(e.reason, format!("Error fetching PMTiles metadata: {}", e.message)));

                            return;
                        }

                        if response_metadata.not_modified {
                            return;
                        }

                        let Some(data) = &response_metadata.data else {
                            callback(error(
                                Reason::Other,
                                format!(
                                    "PMTiles metadata response has no data (range={}-{})",
                                    header.json_metadata_offset,
                                    header
                                        .json_metadata_offset
                                        .wrapping_add(header.json_metadata_bytes)
                                        .wrapping_sub(1)
                                ),
                            ));
                            return;
                        };

                        let mut data: Vec<u8> = data.to_vec();

                        if header.internal_compression == pmtiles::COMPRESSION_GZIP {
                            match decompress(&data) {
                                Ok(decompressed) => data = decompressed,
                                Err(e) => {
                                    callback(error(
                                        Reason::Other,
                                        format!("Error decompressing PMTiles metadata: {e}"),
                                    ));
                                    return;
                                }
                            }
                        }

                        parse_callback(&data);
                    });

                    return;
                }

                parse_callback(&[]);
            }),
        );
    }

    fn store_directory(
        &self,
        url: &str,
        directory_offset: u64,
        directory_length: u64,
        directory_data: &[u8],
    ) -> Result<(), String> {
        let directory_cache_key = format!("{url}|{directory_offset}|{directory_length}");
        let directory = pmtiles::deserialize_directory(directory_data).map_err(|e| e.to_string())?;

        let mut cache = self.directory_cache.borrow_mut();
        let mut control = self.directory_cache_control.borrow_mut();
        let cache = cache.entry(url.to_owned()).or_default();
        let control = control.entry(url.to_owned()).or_default();

        cache.entry(directory_cache_key.clone()).or_insert(directory);
        control.push(directory_cache_key);

        if control.len() > MAX_DIRECTORY_CACHE_ENTRIES {
            cache.remove(&control[0]);
            control.remove(0);
        }
        Ok(())
    }

    fn get_directory(
        self: &Rc<Self>,
        url: &str,
        req: u64,
        directory_offset: u64,
        directory_length: u32,
        callback: AsyncCallback,
    ) {
        let directory_cache_key = format!("{url}|{directory_offset}|{directory_length}");

        let cached =
            self.directory_cache.borrow().get(url).is_some_and(|cache| cache.contains_key(&directory_cache_key));
        if cached {
            {
                let mut controls = self.directory_cache_control.borrow_mut();
                let control = controls.get_mut(url).expect("directory cache control");
                if control.last() != Some(&directory_cache_key) {
                    control.push(directory_cache_key.clone());

                    if let Some(position) = control.iter().position(|key| *key == directory_cache_key) {
                        control.remove(position);
                    }
                }
            }

            callback(None);
            return;
        }

        let this = self.clone();
        let url = url.to_owned();
        self.get_header(
            &url.clone(),
            req,
            Rc::new(move |error_ptr: ErrorPtr| {
                if error_ptr.is_some() {
                    callback(error_ptr);
                    return;
                }

                let header = this.header_cache.borrow()[&url];

                let mut resource = Resource::new(Kind::Source, url.clone());
                resource.loading_method = LoadingMethod::All;
                resource.data_range = Some((
                    directory_offset,
                    directory_offset.wrapping_add(u64::from(directory_length)).wrapping_sub(1),
                ));

                let (this2, url, callback) = (this.clone(), url.clone(), callback.clone());
                this.request(req, &resource, move |response: Response| {
                    if let Some(e) = &response.error {
                        callback(error(e.reason, format!("Error fetching PMTiles directory: {}", e.message)));

                        return;
                    }

                    if response.not_modified {
                        return;
                    }

                    let Some(data) = &response.data else {
                        callback(error(Reason::Other, "PMTiles directory response has no data"));
                        return;
                    };

                    let result = (|| -> Result<(), String> {
                        let mut directory_data: Vec<u8> = data.to_vec();

                        if header.internal_compression == pmtiles::COMPRESSION_GZIP {
                            directory_data = decompress(&directory_data)?;
                        }

                        this2.store_directory(&url, directory_offset, u64::from(directory_length), &directory_data)
                    })();
                    match result {
                        Ok(()) => callback(None),
                        Err(e) => callback(error(Reason::Other, format!("Error parsing PMTiles directory: {e}"))),
                    }
                });
            }),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn get_tile_address(
        self: &Rc<Self>,
        url: &str,
        req: u64,
        tile_id: u64,
        directory_offset: u64,
        directory_length: u32,
        directory_depth: u32,
        callback: AsyncTileCallback,
    ) {
        if directory_depth > 3 {
            callback(
                (0, 0),
                error(Reason::Other, "Error fetching PMTiles tile address: Maximum directory depth exceeded"),
            );

            return;
        }

        let this = self.clone();
        let url = url.to_owned();
        self.get_directory(
            &url.clone(),
            req,
            directory_offset,
            directory_length,
            Rc::new(move |error_ptr: ErrorPtr| {
                if error_ptr.is_some() {
                    callback((0, 0), error_ptr);
                    return;
                }

                let header = this.header_cache.borrow()[&url];
                let entry = {
                    let cache = this.directory_cache.borrow();
                    let directory = &cache[&url][&format!("{url}|{directory_offset}|{directory_length}")];
                    pmtiles::find_tile(directory, tile_id)
                };

                if entry.length > 0 {
                    if entry.run_length > 0 {
                        callback((header.tile_data_offset.wrapping_add(entry.offset), entry.length), None);
                        return;
                    }

                    this.get_tile_address(
                        &url,
                        req,
                        tile_id,
                        header.leaf_dirs_offset.wrapping_add(entry.offset),
                        entry.length,
                        directory_depth + 1,
                        callback.clone(),
                    );
                    return;
                }

                callback((0, 0), None);
            }),
        );
    }
}

/// `PMTilesFileSource::Impl`.
pub struct PMTilesFileSourceThread {
    state: Rc<State>,
}

impl PMTilesFileSourceThread {
    /// Generate a tilejson resource from .pmtiles file
    pub fn request_tilejson(&self, req: u64, resource: &Resource, r: &ActorRef<FileSourceRequestState>) {
        let url = extract_url(&resource.url);

        let (this, r, url2) = (self.state.clone(), r.clone(), url.clone());
        self.state.get_metadata(
            &url,
            req,
            Rc::new(move |error_ptr: ErrorPtr| {
                let mut response = Response::new();

                if error_ptr.is_some() {
                    response.error = error_ptr;
                    send(&r, response);
                    return;
                }

                let metadata = this.metadata_cache.borrow()[&url2].clone();
                response.data = Some(Arc::new(metadata.into_bytes()));
                send(&r, response);
            }),
        );
    }

    /// Load data for specific tile
    pub fn request_tile(&self, req: u64, resource: &Resource, r: &ActorRef<FileSourceRequestState>) {
        let url = extract_url(&resource.url);
        // C++ dereferences `resource.tileData` unchecked.
        let Some(tile_data) = resource.tile_data.clone() else {
            let mut response = Response::new();
            response.error = Some(Box::new(Error::new(Reason::Other, "tile resource without tile data")));
            send(r, response);
            return;
        };

        let (this, r, url2) = (self.state.clone(), r.clone(), url.clone());
        self.state.get_header(
            &url,
            req,
            Rc::new(move |error_ptr: ErrorPtr| {
                if error_ptr.is_some() {
                    let mut response = Response::new();
                    response.no_content = true;
                    response.error = error_ptr;
                    send(&r, response);
                    return;
                }

                let header = this.header_cache.borrow()[&url2];

                // `std::cmp_less(z, min_zoom) || std::cmp_greater(z, max_zoom)`.
                let z = i32::from(tile_data.z);
                if z < i32::from(header.min_zoom) || z > i32::from(header.max_zoom) {
                    let mut response = Response::new();
                    response.no_content = true;
                    send(&r, response);
                    return;
                }

                let tile_id = match pmtiles::zxy_to_tileid(tile_data.z as u8, tile_data.x as u32, tile_data.y as u32) {
                    Ok(tile_id) => tile_id,
                    Err(e) => {
                        let mut response = Response::new();
                        response.no_content = true;
                        response.error = error(Reason::Other, format!("Invalid tile: {e}"));
                        send(&r, response);
                        return;
                    }
                };

                let (this2, r, url) = (this.clone(), r.clone(), url2.clone());
                this.get_tile_address(
                    &url2,
                    req,
                    tile_id,
                    header.root_dir_offset,
                    header.root_dir_bytes as u32,
                    0,
                    Rc::new(move |tile_address: (u64, u32), tile_error: ErrorPtr| {
                        if tile_error.is_some() {
                            let mut response = Response::new();
                            response.no_content = true;
                            response.error = tile_error;
                            send(&r, response);
                            return;
                        }

                        if tile_address.0 == 0 && tile_address.1 == 0 {
                            let mut response = Response::new();
                            response.no_content = true;
                            send(&r, response);
                            return;
                        }

                        let mut tile_resource = Resource::new(Kind::Source, url.clone());
                        tile_resource.loading_method = LoadingMethod::All;
                        tile_resource.data_range = Some((
                            tile_address.0,
                            tile_address.0.wrapping_add(u64::from(tile_address.1)).wrapping_sub(1),
                        ));

                        let r = r.clone();
                        this2.request(req, &tile_resource, move |tile_response: Response| {
                            let mut response = Response::new();
                            response.no_content = true;

                            if let Some(e) = &tile_response.error {
                                response.error = error(e.reason, format!("Error fetching PMTiles tile: {}", e.message));
                                send(&r, response);
                                return;
                            }

                            if tile_response.not_modified {
                                return;
                            }

                            let Some(data) = &tile_response.data else {
                                response.error =
                                    error(Reason::Other, "Error fetching PMTiles tile: response has no data");
                                send(&r, response);
                                return;
                            };

                            response.data = Some(data.clone());
                            response.no_content = false;
                            response.modified = tile_response.modified;
                            response.expires = tile_response.expires;
                            response.etag = tile_response.etag.clone();

                            if header.tile_compression == pmtiles::COMPRESSION_GZIP {
                                // Support uncompressed tiles
                                if is_compressed(data) {
                                    match decompress(data) {
                                        Ok(decompressed) => response.data = Some(Arc::new(decompressed)),
                                        Err(e) => {
                                            response.error =
                                                error(Reason::Other, format!("Error decompressing PMTiles tile: {e}"))
                                        }
                                    }
                                } else {
                                    response.data = Some(data.clone());
                                }
                            }

                            send(&r, response);
                        });
                    }),
                );
            }),
        );
    }
}

impl Drop for PMTilesFileSourceThread {
    fn drop(&mut self) {
        // The sub-requests' callbacks hold the state; drop them here.
        let tasks = std::mem::take(&mut *self.state.tasks.borrow_mut());
        drop(tasks);
    }
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// `PMTilesFileSource`: the `Pmtiles` file source (`pmtiles://` URLs of
/// local or remote archives).
pub struct PMTilesFileSource {
    thread: Thread<PMTilesFileSourceThread>,
    resource_options: Arc<Mutex<ResourceOptions>>,
}

impl PMTilesFileSource {
    /// A PMTiles file source that reads archives through `file_sources`'
    /// `ResourceLoader`.
    pub fn new(file_sources: &FileSourceManager, resource_options: &ResourceOptions) -> Self {
        let resource_options = Arc::new(Mutex::new(resource_options.clone()));
        let ro = resource_options.clone();
        let file_sources = file_sources.clone();
        PMTilesFileSource {
            thread: Thread::new("PMTilesFileSource", move |_| PMTilesFileSourceThread {
                state: Rc::new(State {
                    resource_options: ro,
                    file_sources,
                    file_source: RefCell::new(None),
                    header_cache: RefCell::new(VecMap::new()),
                    metadata_cache: RefCell::new(VecMap::new()),
                    directory_cache: RefCell::new(VecMap::new()),
                    directory_cache_control: RefCell::new(VecMap::new()),
                    tasks: RefCell::new(VecMap::new()),
                }),
            }),
            resource_options,
        }
    }
}

impl FileSource for PMTilesFileSource {
    fn request(&self, resource: &Resource, callback: Callback) -> Box<dyn AsyncRequest> {
        let req = FileSourceRequest::new(callback);
        let req_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let req_ref = req.actor();
        let resource = resource.clone();

        // assume if there is a tile request, that the pmtiles file has been
        // validated
        if resource.kind == Kind::Tile {
            self.thread.actor().invoke(move |t: &PMTilesFileSourceThread| t.request_tile(req_id, &resource, &req_ref));
            return Box::new(req);
        }

        // return TileJSON
        self.thread.actor().invoke(move |t: &PMTilesFileSourceThread| t.request_tilejson(req_id, &resource, &req_ref));
        Box::new(req)
    }

    fn can_request(&self, resource: &Resource) -> bool {
        accepts_url(&resource.url)
    }

    fn set_resource_options(&self, options: ResourceOptions) {
        *self.resource_options.lock_unpoisoned() = options;
    }

    fn get_resource_options(&self) -> ResourceOptions {
        self.resource_options.lock_unpoisoned().clone()
    }
}
