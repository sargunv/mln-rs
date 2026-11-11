// Ports include/mln/storage/file_source.hpp.

use feature::Value;

use crate::async_request::AsyncRequest;
use crate::http_header_transform::HttpHeaderTransform;
use crate::resource::Resource;
use crate::resource_options::ResourceOptions;
use crate::resource_transform::ResourceTransform;
use crate::response::Response;

// TODO: Rename to ResourceProviderType
/// `FileSourceType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FileSourceType {
    Asset,
    // TODO: split to separate types
    // - Cache for fast KV store (FASTER, LevelDB, RocksDB)
    // - Database for read-only offline use-cases
    Database,
    FileSystem,
    Network,
    Mbtiles,
    Pmtiles,
    /// Resource loader acts as a proxy and has logic for request delegation
    /// to Asset, Cache, and other file sources.
    ResourceLoader,
}

/// `FileSource::Callback` (`std::function<void(Response)>`). It runs on the
/// thread that made the request, so it need not be `Send`.
pub type Callback = Box<dyn FnMut(Response) + 'static>;

/// `FileSource`. File sources are shared between threads
/// (`std::shared_ptr<FileSource>`), hence `Send + Sync`.
pub trait FileSource: Send + Sync {
    /// Request a resource. The callback will be called asynchronously, in
    /// the same thread as the request was made. This thread must have an
    /// active RunLoop. The request may be cancelled before completion by
    /// releasing the returned AsyncRequest. If the request is cancelled
    /// before the callback is executed, the callback will not be executed.
    fn request(&self, resource: &Resource, callback: Callback) -> Box<dyn AsyncRequest>;

    /// Allows to forward response from one source to another. Optionally,
    /// callback can be provided to receive notification for forward
    /// operation.
    fn forward(&self, _resource: &Resource, _response: &Response, _callback: Option<Box<dyn FnOnce() + Send>>) {}

    /// When a file source supports consulting a local cache only, it must
    /// return true. Cache-only requests are requests that aren't as urgent,
    /// but could be useful, e.g. to cover part of the map while loading. The
    /// FileSource should only do cheap actions to retrieve the data, e.g.
    /// load it from a cache, but not from the internet.
    fn supports_cache_only_requests(&self) -> bool {
        false
    }

    /// Checks whether a resource could be requested from this file source.
    fn can_request(&self, resource: &Resource) -> bool;

    /// Pause file request activity.
    ///
    /// If pause is called then no revalidation or network request activity
    /// will occur.
    fn pause(&self) {}

    /// Resume file request activity.
    ///
    /// Calling resume will unpause the file source and process any tasks
    /// that expired while the file source was paused.
    fn resume(&self) {}

    /// Sets a transformer for resource URLs (online file sources).
    fn set_resource_transform(&self, _transform: ResourceTransform) {}

    /// Port-only: sets the headers to add to HTTP requests (online and HTTP
    /// file sources).
    fn set_http_header_transform(&self, _transform: HttpHeaderTransform) {}

    /// Generic setter method
    fn set_property(&self, _key: &str, _value: &Value) {}
    /// Generic getter method
    fn get_property(&self, _key: &str) -> Value {
        Value::Null
    }

    fn set_resource_options(&self, options: ResourceOptions);
    fn get_resource_options(&self) -> ResourceOptions;

    /// The concrete file source, to reach the API of a specific type (such
    /// as the database's) on one from a [`FileSourceManager`]. `None` unless
    /// overridden.
    ///
    /// [`FileSourceManager`]: crate::FileSourceManager
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }
}

// Properties that may be supported by online file sources:

/// Property name to set / get an access token.
/// type: std::string
pub const API_KEY_KEY: &str = "api-tkey";

/// Property name to set / get base url.
/// type: std::string
pub const API_BASE_URL_KEY: &str = "api-base-url";

/// Property name to set / get maximum number of concurrent requests.
/// type: unsigned
pub const MAX_CONCURRENT_REQUESTS_KEY: &str = "max-concurrent-requests";

// Properties that may be supported by database file sources:

/// Property to set database mode. When set, database opens in read-only
/// mode; database opens in read-write-create mode otherwise. type: bool
pub const READ_ONLY_MODE_KEY: &str = "read-only-mode";
