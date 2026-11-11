// Ports test/include/mln/test/stub_file_source.hpp and
// test/src/mln/test/stub_file_source.cpp, the FileSource of the C++ unit
// tests (here for the tests of every crate that needs one).
//
// The asynchronous mode polls the pending requests from an AsyncTask on the
// current RunLoop, re-armed every millisecond while requests are pending
// (C++: a 1 ms repeating `util::Timer`, a TODO(port) of the RunLoop). The
// stub lives on the thread that created it; requests must come from that
// thread.
//
// TODO(port): the style/source/tile/glyphs/image responders beyond the
// generic `set_response`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::Mutex;

use feature::Value;
use mln_actor::{AsyncTask, LocalObject};
use mln_util::sync::MutexExt;
use mln_vec_map::VecMap;

use crate::async_request::AsyncRequest;
use crate::file_source::{Callback, FileSource};
use crate::resource::{Kind, Resource};
use crate::resource_options::ResourceOptions;
use crate::response::Response;

/// `StubFileSource::ResponseType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseType {
    Asynchronous,
    Synchronous,
    /// Requests wait for `respond_to_all`.
    Manual,
}

/// A responder: `std::function<std::optional<Response>(const Resource&)>`.
pub type Responder = Rc<dyn Fn(&Resource) -> Option<Response>>;

#[derive(Default)]
struct State {
    pending: VecMap<u64, (Resource, Responder, Rc<RefCell<Callback>>)>,
    next_id: u64,
    timer: Option<AsyncTask>,
    responders: HashMap<Kind, Responder>,
    // gtests-core: the C++ `response` member (the responder for every kind;
    // by default `defaultResponse`, which dispatches to the per-kind ones).
    response: Option<Responder>,
    properties: HashMap<String, Value>,
}

/// `StubFileSource`.
pub struct StubFileSource {
    type_: ResponseType,
    state: LocalObject<RefCell<State>>,
    resource_options: Mutex<ResourceOptions>,
}

struct StubFileRequest {
    id: u64,
    state: Weak<RefCell<State>>,
}

impl AsyncRequest for StubFileRequest {}

impl Drop for StubFileRequest {
    /// `~StubFileRequest() { fileSource.remove(this); }`
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state.borrow_mut().pending.remove(&self.id);
        }
    }
}

fn unexpected(resource: &Resource) -> Option<Response> {
    // C++ throws std::runtime_error("unexpected ... request").
    panic!("unexpected {:?} request: {}", resource.kind, resource.url)
}

impl StubFileSource {
    pub fn new(type_: ResponseType) -> Self {
        let state = Rc::new(RefCell::new(State::default()));
        if type_ == ResponseType::Asynchronous {
            let weak = Rc::downgrade(&state);
            let timer = AsyncTask::new(move || {
                if let Some(state) = weak.upgrade() {
                    Self::poll(&state);
                }
            });
            state.borrow_mut().timer = Some(timer);
        }
        StubFileSource {
            type_,
            state: LocalObject::from_rc(state),
            resource_options: Mutex::new(ResourceOptions::default()),
        }
    }

    fn state(&self) -> Rc<RefCell<State>> {
        self.state.get().expect("StubFileSource used from another thread")
    }

    /// Sets the responder for one resource kind (`styleResponse`,
    /// `spriteJSONResponse`, `spriteImageResponse`, ...).
    pub fn set_response(&self, kind: Kind, responder: impl Fn(&Resource) -> Option<Response> + 'static) {
        self.state().borrow_mut().responders.insert(kind, Rc::new(responder));
    }

    // gtests-core: assigning `response` in C++ (test/map/map.test.cpp
    // DisabledSources, test/map/prefetch.test.cpp).
    /// Replaces the responder for all kinds (C++ `fileSource->response = ...`).
    pub fn set_global_response(&self, responder: impl Fn(&Resource) -> Option<Response> + 'static) {
        self.state().borrow_mut().response = Some(Rc::new(responder));
    }

    /// The `response` member a request captures: the global responder if
    /// one was set, else `defaultResponse`, which dispatches to the per-kind
    /// responder current when it is called (so a responder replaced while a
    /// request is pending, e.g. after an error response, answers the retry).
    fn response(&self) -> Responder {
        let state = self.state();
        if let Some(response) = state.borrow().response.clone() {
            return response;
        }
        let weak = Rc::downgrade(&state);
        Rc::new(move |resource: &Resource| {
            let responder = weak.upgrade().and_then(|state| state.borrow().responders.get(&resource.kind).cloned());
            match responder {
                Some(responder) => responder(resource),
                None => unexpected(resource),
            }
        })
    }

    /// `respondToAll()`: answers the requests pending now. Only for
    /// `ResponseType::Manual`.
    pub fn respond_to_all(&self) {
        assert_eq!(self.type_, ResponseType::Manual, "this method can only be called with ResponseType::Manual");
        let state = self.state();
        let ids: Vec<u64> = state.borrow().pending.keys().copied().collect();
        for id in ids {
            // A callback can cancel or start requests, so take each one out
            // of `pending` before calling it.
            let Some((resource, responder, callback)) = state.borrow_mut().pending.remove(&id) else {
                continue;
            };
            if let Some(res) = responder(&resource) {
                (callback.borrow_mut())(res);
            }
        }
    }

    fn schedule_poll(state: &Rc<RefCell<State>>) {
        if let Some(timer) = &state.borrow().timer {
            timer.send();
        }
    }

    /// The timer callback.
    fn poll(state: &Rc<RefCell<State>>) {
        // Explicit copy to avoid iterator invalidation if ~StubFileRequest
        // gets called within the loop.
        let pending: Vec<(u64, Resource, Responder, Rc<RefCell<Callback>>)> = state
            .borrow()
            .pending
            .iter()
            .map(|(id, (resource, responder, callback))| (*id, resource.clone(), responder.clone(), callback.clone()))
            .collect();
        for (id, resource, responder, callback) in pending {
            if !state.borrow().pending.contains_key(&id) {
                continue;
            }
            if let Some(res) = responder(&resource) {
                // This must be before calling the callback, because it's
                // possible that the callback could deallocate the
                // AsyncRequest itself.
                if res.error.is_none() {
                    state.borrow_mut().pending.remove(&id);
                }

                (callback.borrow_mut())(res);
            }
        }
        if !state.borrow().pending.is_empty() {
            // The next timer tick.
            std::thread::sleep(std::time::Duration::from_millis(1));
            Self::schedule_poll(state);
        }
    }
}

impl FileSource for StubFileSource {
    fn request(&self, resource: &Resource, mut callback: Callback) -> Box<dyn AsyncRequest> {
        let state = self.state();
        let id = {
            let mut s = state.borrow_mut();
            s.next_id += 1;
            s.next_id
        };
        let req = StubFileRequest { id, state: Rc::downgrade(&state) };
        let responder = self.response();
        match self.type_ {
            ResponseType::Synchronous => {
                if let Some(res) = responder(resource) {
                    callback(res);
                }
            }
            ResponseType::Asynchronous | ResponseType::Manual => {
                state.borrow_mut().pending.insert(id, (resource.clone(), responder, Rc::new(RefCell::new(callback))));
                Self::schedule_poll(&state);
            }
        }
        Box::new(req)
    }

    fn can_request(&self, _resource: &Resource) -> bool {
        true
    }

    fn set_property(&self, key: &str, value: &Value) {
        self.state().borrow_mut().properties.insert(key.to_owned(), value.clone());
    }

    fn get_property(&self, key: &str) -> Value {
        self.state().borrow().properties.get(key).cloned().unwrap_or(Value::Null)
    }

    fn set_resource_options(&self, options: ResourceOptions) {
        *self.resource_options.lock_unpoisoned() = options;
    }

    fn get_resource_options(&self) -> ResourceOptions {
        self.resource_options.lock_unpoisoned().clone()
    }
}
