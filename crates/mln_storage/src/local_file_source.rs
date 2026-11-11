// Ports include/mln/storage/local_file_source.hpp and
// platform/default/src/mln/storage/local_file_source.cpp.
//
// The requests run on the "LocalFileSource" thread (`util::Thread`) as in
// C++. The resource options are kept next to the thread behind a
// mutex (C++ keeps them in the thread object and reads them back with a
// blocking `ask`); the values returned are the same.
//
// TODO(port): makeThreadPrioritySetter(EXPERIMENTAL_THREAD_PRIORITY_FILE).

use std::sync::Mutex;

use mln_actor::{ActorRef, Thread};
use mln_util::constants::FILE_PROTOCOL;
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
    url.starts_with(FILE_PROTOCOL)
}

/// `pathFromFileURL(url)`: the percent-decoded path after `file://`.
fn path_from_file_url(url: &str) -> Vec<u8> {
    #[allow(unused_mut)]
    let mut path = percent_decode(&url[FILE_PROTOCOL.len()..]);
    #[cfg(windows)]
    {
        // file:///C:/path minus the scheme is /C:/path, which Windows rejects.
        if path.len() >= 3 && path[0] == b'/' && path[2] == b':' && path[1].is_ascii_alphabetic() {
            path.remove(0);
        }
    }
    path
}

/// `LocalFileSource::Impl`.
pub struct LocalFileSourceThread;

impl LocalFileSourceThread {
    /// `request(const Resource&, const ActorRef<FileSourceRequest>&)`.
    pub fn request(&self, resource: &Resource, req: &ActorRef<FileSourceRequestState>) {
        if !accepts_url(&resource.url) {
            let mut response = Response::new();
            response.error = Some(Box::new(Error::new(Reason::Other, "Invalid file URL")));
            req.invoke(move |request: &FileSourceRequestState| request.set_response(response));
            return;
        }

        let path = path_from_file_url(&resource.url);
        request_local_file(&path, req, resource.data_range);
    }
}

/// `LocalFileSource`: the `FileSystem` file source (`file://` URLs).
pub struct LocalFileSource {
    thread: Thread<LocalFileSourceThread>,
    resource_options: Mutex<ResourceOptions>,
}

impl LocalFileSource {
    pub fn new(resource_options: &ResourceOptions) -> Self {
        LocalFileSource {
            thread: Thread::new("LocalFileSource", |_| LocalFileSourceThread),
            resource_options: Mutex::new(resource_options.clone()),
        }
    }
}

impl FileSource for LocalFileSource {
    fn request(&self, resource: &Resource, callback: Callback) -> Box<dyn AsyncRequest> {
        let req = FileSourceRequest::new(callback);
        let req_ref = req.actor();
        let resource = resource.clone();
        self.thread.actor().invoke(move |t: &LocalFileSourceThread| t.request(&resource, &req_ref));
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
