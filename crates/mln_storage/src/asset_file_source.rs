// Ports include/mln/storage/asset_file_source.hpp and
// platform/default/src/mln/storage/asset_file_source.cpp.
//
// The requests run on the "AssetFileSource" thread (`util::Thread`) as in
// C++. The resource options are kept next to the thread behind a
// mutex (C++ keeps them in the thread object and reads them back with a
// blocking `ask`); the values returned are the same.
//
// TODO(port): makeThreadPrioritySetter(EXPERIMENTAL_THREAD_PRIORITY_FILE).

use std::sync::Mutex;

use mln_actor::{ActorRef, Thread};
use mln_util::constants::ASSET_PROTOCOL;
use mln_util::sync::MutexExt;
use mln_util::url::percent_decode;

use crate::async_request::AsyncRequest;
use crate::file_source::{Callback, FileSource};
use crate::file_source_request::{FileSourceRequest, FileSourceRequestState};
use crate::local_file_request::request_local_file;
use crate::resource::Resource;
use crate::resource_options::ResourceOptions;
use crate::response::{Error, Reason, Response};

fn accepts_url(url: &str) -> bool {
    url.starts_with(ASSET_PROTOCOL)
}

/// `AssetFileSource::Impl`.
pub struct AssetFileSourceThread {
    root: String,
}

impl AssetFileSourceThread {
    /// `request(const Resource&, const ActorRef<FileSourceRequest>&)`.
    pub fn request(&self, resource: &Resource, req: &ActorRef<FileSourceRequestState>) {
        if !accepts_url(&resource.url) {
            let mut response = Response::new();
            response.error = Some(Box::new(Error::new(Reason::Other, "Invalid asset URL")));
            req.invoke(move |request: &FileSourceRequestState| request.set_response(response));
            return;
        }

        // Cut off the protocol and prefix with path.
        let mut path = format!("{}/", self.root).into_bytes();
        path.extend_from_slice(&percent_decode(&resource.url[ASSET_PROTOCOL.len()..]));
        request_local_file(&path, req, resource.data_range);
    }
}

/// `AssetFileSource`: the `Asset` file source (`asset://` URLs, relative to
/// `ResourceOptions::assetPath()`).
pub struct AssetFileSource {
    thread: Thread<AssetFileSourceThread>,
    resource_options: Mutex<ResourceOptions>,
}

impl AssetFileSource {
    pub fn new(resource_options: &ResourceOptions) -> Self {
        let root = resource_options.asset_path().to_owned();
        AssetFileSource {
            thread: Thread::new("AssetFileSource", move |_| AssetFileSourceThread { root }),
            resource_options: Mutex::new(resource_options.clone()),
        }
    }
}

impl FileSource for AssetFileSource {
    fn request(&self, resource: &Resource, callback: Callback) -> Box<dyn AsyncRequest> {
        let req = FileSourceRequest::new(callback);
        let req_ref = req.actor();
        let resource = resource.clone();
        self.thread.actor().invoke(move |t: &AssetFileSourceThread| t.request(&resource, &req_ref));
        Box::new(req)
    }

    fn can_request(&self, resource: &Resource) -> bool {
        accepts_url(&resource.url)
    }

    fn pause(&self) {
        self.thread.pause();
    }

    fn resume(&self) {
        self.thread.resume();
    }

    fn set_resource_options(&self, options: ResourceOptions) {
        *self.resource_options.lock_unpoisoned() = options;
    }

    fn get_resource_options(&self) -> ResourceOptions {
        self.resource_options.lock_unpoisoned().clone()
    }
}
