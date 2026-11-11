//! How a runtime loads resources: a host's resource provider in front of the
//! network, and the transforms of request URLs and headers.

use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use mln_actor::{ActorRef, Mailbox, scheduler};
use mln_storage::file_source::Callback;
use mln_storage::http_header_transform::HttpHeaders;
use mln_storage::resource::Kind;
use mln_storage::response::Reason;
use mln_storage::{AsyncRequest, FileSource, OnlineFileSource, Resource, ResourceError, ResourceOptions, Response};
use mln_util::error::Error;
use mln_util::mapbox;
use mln_util::sync::MutexExt;

/// Serves a runtime's network requests in place of the library.
///
/// A closure `Fn(ResourceRequest)` is a provider that serves every request.
/// The library loads only `http`, `https` and the tile server's alias
/// scheme, so without a resource transform, a request for another scheme
/// that no provider serves fails at once.
pub trait ResourceProvider: Send + Sync + 'static {
    /// Whether the provider serves a request for a resource of `kind` at
    /// `url`, the URL after the library expands any shorthand in the style's
    /// URL. The library loads the requests that the provider doesn't serve.
    fn serves(&self, _kind: Kind, _url: &str) -> bool {
        true
    }

    /// Serves `request`: answer it with [`ResourceRequest::respond`], or hand
    /// it back with [`ResourceRequest::pass_through`], now or later and on
    /// any thread. It runs on a thread of the library's, which waits for it,
    /// so it should hand slow work elsewhere.
    fn request(&self, request: ResourceRequest);
}

impl<F: Fn(ResourceRequest) + Send + Sync + 'static> ResourceProvider for F {
    fn request(&self, request: ResourceRequest) {
        self(request);
    }
}

/// A maintenance action on a runtime's cache database.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CacheDatabaseOperation {
    /// Deletes the ambient cache's resources and frees their space. Offline
    /// regions keep theirs.
    Clear,
    /// Keeps the ambient cache's resources, but makes maps check each with
    /// its server before they use it again. Offline regions are unaffected.
    Invalidate,
    /// Compacts the database file to free the space it no longer uses.
    Pack,
    /// Deletes the whole database, offline regions included, and starts an
    /// empty one.
    Reset,
}

/// The runtime's hooks that its network file source reads on each request.
#[derive(Default)]
pub(crate) struct ResourceHooks {
    provider: Mutex<Option<Arc<dyn ResourceProvider>>>,
    /// Whether a resource transform is set, which may rewrite a URL of any
    /// scheme into one that the library loads.
    transform_set: AtomicBool,
}

impl ResourceHooks {
    /// Replaces the provider, and gives back the one before.
    pub(crate) fn set_provider(
        &self,
        provider: Option<Arc<dyn ResourceProvider>>,
    ) -> Option<Arc<dyn ResourceProvider>> {
        std::mem::replace(&mut *self.provider.lock_unpoisoned(), provider)
    }

    fn provider(&self) -> Option<Arc<dyn ResourceProvider>> {
        self.provider.lock_unpoisoned().clone()
    }

    pub(crate) fn set_transform_set(&self, set: bool) {
        self.transform_set.store(set, Ordering::Release);
    }
}

/// The runtime's network file source: the library's online file source,
/// behind the host's resource provider.
pub(crate) struct ProviderFileSource {
    online: Arc<dyn FileSource>,
    hooks: Arc<ResourceHooks>,
}

/// Who loads a request.
enum Route {
    /// The provider, with the resolved URL.
    Provider(Arc<dyn ResourceProvider>, String),
    /// The library. `declined` says whether a provider is set, which the
    /// message of a request that the library can't load names.
    Native { declined: bool },
}

impl ProviderFileSource {
    pub(crate) fn new(options: &ResourceOptions, hooks: Arc<ResourceHooks>) -> Self {
        ProviderFileSource { online: Arc::new(OnlineFileSource::new(options)), hooks }
    }

    /// Who loads `resource`. No lock is held while the provider decides.
    fn route(&self, resource: &Resource) -> Route {
        if !resource.has_loading_method(mln_storage::LoadingMethod::Network) {
            return Route::Native { declined: false };
        }
        let Some(provider) = self.hooks.provider() else {
            return Route::Native { declined: false };
        };
        let url = resolved_url(&self.online.get_resource_options(), resource);
        if provider.serves(resource.kind, &url) {
            Route::Provider(provider, url)
        } else {
            Route::Native { declined: true }
        }
    }

    fn native(&self) -> Native {
        Native { online: self.online.clone(), hooks: self.hooks.clone() }
    }
}

/// The library's own loading, which fails a request for a URL scheme that
/// it can't load at once, rather than sending it to the network.
#[derive(Clone)]
struct Native {
    online: Arc<dyn FileSource>,
    hooks: Arc<ResourceHooks>,
}

impl Native {
    fn request(&self, resource: &Resource, callback: Callback, declined: bool) -> Box<dyn AsyncRequest> {
        match self.unsupported_scheme(&resource.url) {
            Some(scheme) => respond_later(unsupported_scheme(scheme, &resource.url, declined), callback),
            None => self.online.request(resource, callback),
        }
    }

    /// The scheme of `url` when the online file source can't load it.
    fn unsupported_scheme<'a>(&self, url: &'a str) -> Option<&'a str> {
        if self.hooks.transform_set.load(Ordering::Acquire) {
            return None;
        }
        let scheme = url_scheme(url)?;
        let tile_server = self.online.get_resource_options().tile_server_options();
        let loadable = ["http", "https", tile_server.uri_scheme_alias()]
            .iter()
            .any(|known| !known.is_empty() && known.eq_ignore_ascii_case(scheme));
        (!loadable).then_some(scheme)
    }
}

/// The scheme of `url`, as RFC 3986 spells one: a letter, then letters,
/// digits, `+`, `-` and `.`, before a colon.
fn url_scheme(url: &str) -> Option<&str> {
    let (scheme, _) = url.split_once(':')?;
    let mut bytes = scheme.bytes();
    let first = bytes.next()?;
    (first.is_ascii_alphabetic() && bytes.all(|byte| byte.is_ascii_alphanumeric() || b"+-.".contains(&byte)))
        .then_some(scheme)
}

/// `url` without its query, fragment and user information, so that a
/// message never carries credentials.
fn redacted_url(url: &str) -> String {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    let mut url = url[..end].to_owned();
    if let Some(separator) = url.find("://") {
        let authority = separator + 3;
        let authority_end = url[authority..].find('/').map_or(url.len(), |end| authority + end);
        if let Some(at) = url[authority..authority_end].rfind('@') {
            url.replace_range(authority..=authority + at, "");
        }
    }
    url
}

fn unsupported_scheme(scheme: &str, url: &str, declined: bool) -> Response {
    let advice = if declined {
        "the registered resource provider declined this request; update it to serve this scheme"
    } else {
        "register a resource provider with mln_runtime_set_resource_provider() or Runtime::set_resource_provider to \
         serve this scheme"
    };
    let message =
        format!("unsupported URL scheme \"{scheme}\" for network request \"{}\"; {advice}", redacted_url(url));
    // A reason that the library doesn't retry, since no retry can succeed.
    Response { error: Some(Box::new(ResourceError::new(Reason::Other, message))), ..Response::default() }
}

/// A request that answers with `response` on the requesting thread, once
/// the request has been returned.
fn respond_later(response: Response, callback: Callback) -> Box<dyn AsyncRequest> {
    let target = Rc::new(Immediate(RefCell::new(Some(callback))));
    let mailbox = Mailbox::with_target(&scheduler::get_current(), target as Rc<dyn Any>);
    ActorRef::<Immediate>::new(&mailbox).invoke(move |target| {
        let callback = target.0.borrow_mut().take();
        if let Some(mut callback) = callback {
            callback(response);
        }
    });
    Box::new(ImmediateRequest(mailbox))
}

struct Immediate(RefCell<Option<Callback>>);

/// Dropping it cancels the answer.
struct ImmediateRequest(Arc<Mailbox>);

impl AsyncRequest for ImmediateRequest {}

impl Drop for ImmediateRequest {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// The URL that the online file source would load for `resource`, or the
/// resource's own URL when it has no other form.
fn resolved_url(options: &ResourceOptions, resource: &Resource) -> String {
    let tile_server = options.tile_server_options();
    let api_key = options.api_key();
    let url = &resource.url;
    match resource.kind {
        Kind::Unknown | Kind::Image => url.clone(),
        Kind::Style => mapbox::normalize_style_url(&tile_server, url, api_key),
        Kind::Source => mapbox::normalize_source_url(&tile_server, url, api_key).unwrap_or_else(|_| url.clone()),
        Kind::Glyphs => mapbox::normalize_glyphs_url(&tile_server, url, api_key),
        Kind::SpriteImage | Kind::SpriteJSON => mapbox::normalize_sprite_url(&tile_server, url, api_key),
        Kind::Tile => mapbox::normalize_tile_url(&tile_server, url, api_key),
    }
}

impl FileSource for ProviderFileSource {
    fn request(&self, resource: &Resource, callback: Callback) -> Box<dyn AsyncRequest> {
        let (provider, resolved_url) = match self.route(resource) {
            Route::Provider(provider, resolved_url) => (provider, resolved_url),
            Route::Native { declined } => return self.native().request(resource, callback, declined),
        };
        let target = Rc::new(ProviderTarget {
            callback: RefCell::new(Some(callback)),
            native: self.native(),
            resource: resource.clone(),
            request: RefCell::new(None),
        });
        let mailbox = Mailbox::with_target(&scheduler::get_current(), target as Rc<dyn Any>);
        let shared =
            Arc::new(RequestShared { state: Mutex::new(RequestState::default()), target: ActorRef::new(&mailbox) });
        let request = ResourceRequest { resource: resource.clone(), resolved_url, shared: Some(shared.clone()) };
        provider.request(request);
        Box::new(ProviderRequest { mailbox, shared })
    }

    fn forward(&self, resource: &Resource, response: &Response, callback: Option<Box<dyn FnOnce() + Send>>) {
        self.online.forward(resource, response, callback);
    }

    fn supports_cache_only_requests(&self) -> bool {
        self.online.supports_cache_only_requests()
    }

    fn can_request(&self, resource: &Resource) -> bool {
        matches!(self.route(resource), Route::Provider(..)) || self.online.can_request(resource)
    }

    fn pause(&self) {
        self.online.pause();
    }

    fn resume(&self) {
        self.online.resume();
    }

    fn set_resource_transform(&self, transform: mln_storage::resource_transform::ResourceTransform) {
        self.online.set_resource_transform(transform);
    }

    fn set_http_header_transform(&self, transform: mln_storage::http_header_transform::HttpHeaderTransform) {
        self.online.set_http_header_transform(transform);
    }

    fn set_property(&self, key: &str, value: &feature::Value) {
        self.online.set_property(key, value);
    }

    fn get_property(&self, key: &str) -> feature::Value {
        self.online.get_property(key)
    }

    fn set_resource_options(&self, options: ResourceOptions) {
        self.online.set_resource_options(options);
    }

    fn get_resource_options(&self) -> ResourceOptions {
        self.online.get_resource_options()
    }
}

/// The requesting side of a provider's request, on the requesting thread.
struct ProviderTarget {
    callback: RefCell<Option<Callback>>,
    native: Native,
    resource: Resource,
    /// The library's request, once the provider passed through.
    request: RefCell<Option<Box<dyn AsyncRequest>>>,
}

impl ProviderTarget {
    fn respond(&self, mut response: Response) {
        // A revalidation withholds the cached copy from the requester until
        // the answer, so a "not modified" answer delivers that copy, as the
        // online file source's does.
        if response.not_modified
            && let Some(data) = &self.resource.prior_data
        {
            response.data = Some(data.clone());
            response.not_modified = false;
            response.modified = response.modified.or(self.resource.prior_modified);
            if response.etag.is_none() {
                response.etag.clone_from(&self.resource.prior_etag);
            }
        }
        let callback = self.callback.borrow_mut().take();
        if let Some(mut callback) = callback {
            callback(response);
        }
    }

    fn pass_through(&self) {
        let callback = self.callback.borrow_mut().take();
        if let Some(callback) = callback {
            let request = self.native.request(&self.resource, callback, true);
            *self.request.borrow_mut() = Some(request);
        }
    }
}

#[derive(Default)]
struct RequestState {
    /// The requester dropped the request.
    cancelled: bool,
    /// The provider answered or passed through.
    answered: bool,
    on_cancel: Option<Box<dyn FnOnce() + Send>>,
}

struct RequestShared {
    state: Mutex<RequestState>,
    target: ActorRef<ProviderTarget>,
}

/// What the requester holds: dropping it cancels the provider's request.
struct ProviderRequest {
    mailbox: Arc<Mailbox>,
    shared: Arc<RequestShared>,
}

impl AsyncRequest for ProviderRequest {}

impl Drop for ProviderRequest {
    fn drop(&mut self) {
        let on_cancel = {
            let mut state = self.shared.state.lock_unpoisoned();
            state.cancelled = true;
            if state.answered { None } else { state.on_cancel.take() }
        };
        self.mailbox.close();
        if let Some(on_cancel) = on_cancel {
            on_cancel();
        }
    }
}

/// A network request that a [`ResourceProvider`] serves.
///
/// Answer it once, with [`respond`](Self::respond) or
/// [`pass_through`](Self::pass_through). Dropping it unanswered fails the
/// request.
pub struct ResourceRequest {
    resource: Resource,
    resolved_url: String,
    shared: Option<Arc<RequestShared>>,
}

impl ResourceRequest {
    /// The resource as the style names it, with the cache's earlier copy in
    /// its `prior_*` fields.
    pub fn resource(&self) -> &Resource {
        &self.resource
    }

    /// The URL to load: the resource's after the library expands any
    /// shorthand in it, and before any resource transform.
    pub fn resolved_url(&self) -> &str {
        &self.resolved_url
    }

    /// Whether the library no longer needs the request.
    pub fn is_cancelled(&self) -> bool {
        self.shared.as_ref().is_none_or(|shared| shared.state.lock_unpoisoned().cancelled)
    }

    /// Calls `on_cancel`, on the requesting thread, if the library cancels
    /// the request before it is answered. Fails with
    /// [`ErrorKind::Cancelled`](mln_util::ErrorKind::Cancelled) when the
    /// library already cancelled it, and
    /// [`ErrorKind::InvalidState`](mln_util::ErrorKind::InvalidState) when
    /// the request has a cancel callback already.
    pub fn on_cancel(&self, on_cancel: impl FnOnce() + Send + 'static) -> Result<(), Error> {
        let Some(shared) = &self.shared else {
            return Err(Error::invalid_state("the request was answered"));
        };
        let mut state = shared.state.lock_unpoisoned();
        if state.cancelled {
            return Err(Error::cancelled("the library cancelled the request"));
        }
        if state.on_cancel.is_some() {
            return Err(Error::invalid_state("the request has a cancel callback already"));
        }
        state.on_cancel = Some(Box::new(on_cancel));
        Ok(())
    }

    /// Answers the request with `response`. Fails with
    /// [`ErrorKind::InvalidState`](mln_util::ErrorKind::InvalidState) when
    /// the library cancelled the request.
    pub fn respond(mut self, response: Response) -> Result<(), Error> {
        self.answer(move |target| target.respond(response))
    }

    /// Hands the request back to the library, which loads it from the
    /// network as it would without a provider. Fails with
    /// [`ErrorKind::InvalidState`](mln_util::ErrorKind::InvalidState) when
    /// the library cancelled the request.
    pub fn pass_through(mut self) -> Result<(), Error> {
        self.answer(ProviderTarget::pass_through)
    }

    fn answer(&mut self, answer: impl FnOnce(&ProviderTarget) + Send + 'static) -> Result<(), Error> {
        let Some(shared) = self.shared.take() else {
            return Err(Error::invalid_state("the request was answered"));
        };
        let on_cancel = {
            let mut state = shared.state.lock_unpoisoned();
            if state.cancelled {
                return Err(Error::invalid_state("the library cancelled the request"));
            }
            state.answered = true;
            state.on_cancel.take()
        };
        drop(on_cancel);
        shared.target.invoke(answer);
        Ok(())
    }
}

impl Drop for ResourceRequest {
    fn drop(&mut self) {
        if self.shared.is_some() {
            let mut response = Response::new();
            response.error =
                Some(Box::new(ResourceError::new(Reason::Other, "resource request was released without a response")));
            let _ = self.answer(move |target| target.respond(response));
        }
    }
}

impl std::fmt::Debug for ResourceRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceRequest")
            .field("kind", &self.resource.kind)
            .field("url", &self.resource.url)
            .field("resolved_url", &self.resolved_url)
            .finish()
    }
}

/// The headers that the library or the platform manages, which a header
/// transform can't set.
const MANAGED_HEADERS: [&str; 15] = [
    "accept-encoding",
    "connection",
    "content-length",
    "host",
    "if-modified-since",
    "if-none-match",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "range",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "user-agent",
];

/// Whether the library or the platform manages the HTTP header `name`, in
/// any letter case, so that a header transform can't set it.
pub fn is_managed_http_header(name: &str) -> bool {
    MANAGED_HEADERS.iter().any(|managed| managed.eq_ignore_ascii_case(name))
}

/// `headers` without the ones the library manages, and with each name once:
/// a later value replaces an earlier one, whatever the letter case.
pub(crate) fn sanitize_headers(headers: HttpHeaders) -> HttpHeaders {
    let mut sanitized: HttpHeaders = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        if is_managed_http_header(&name) {
            mln_util::logging::Log::warning(
                mln_util::logging::Event::HttpRequest,
                format!("a header transform can't set the managed header {name}"),
            );
            continue;
        }
        match sanitized.iter_mut().find(|(existing, _)| existing.eq_ignore_ascii_case(&name)) {
            Some(existing) => existing.1 = value,
            None => sanitized.push((name, value)),
        }
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_and_repeated_headers_are_dropped() {
        let headers = vec![
            ("X-Token".to_owned(), "a".to_owned()),
            ("Range".to_owned(), "bytes=0-1".to_owned()),
            ("x-token".to_owned(), "b".to_owned()),
            ("User-Agent".to_owned(), "me".to_owned()),
        ];
        assert_eq!(vec![("X-Token".to_owned(), "b".to_owned())], sanitize_headers(headers));
    }
}
