//! `runtime.h`'s resource providers: the host's callback that serves a
//! runtime's network requests, and the request handles it answers through.
//!
//! A request's handle lives until an answer or a release ends it. Answering
//! a request ends its handle whether the answer is taken or refused, and a
//! request that ends without an answer fails.

use core::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ::mln_runtime::{ResourceProvider, ResourceRequest};
use mln_storage::resource::{Kind, Priority, Usage};
use mln_storage::response::Reason;
use mln_storage::{LoadingMethod, Resource, ResourceError, Response, StoragePolicy};
use mln_util::chrono::Timestamp;

use crate::buffer::LentText;
use crate::callback::Registration;
use crate::completion::PendingCompletion;
use crate::error::Error;
use crate::generated::types::{
    MLN_RESOURCE_ERROR_REASON_CONNECTION, MLN_RESOURCE_ERROR_REASON_NOT_FOUND, MLN_RESOURCE_ERROR_REASON_RATE_LIMIT,
    MLN_RESOURCE_ERROR_REASON_SERVER, MLN_RESOURCE_LOADING_METHOD_ALL, MLN_RESOURCE_LOADING_METHOD_CACHE_ONLY,
    MLN_RESOURCE_LOADING_METHOD_NETWORK_ONLY, MLN_RESOURCE_PRIORITY_LOW, MLN_RESOURCE_PRIORITY_REGULAR,
    MLN_RESOURCE_REQUEST_PRIOR_EXPIRES, MLN_RESOURCE_REQUEST_PRIOR_MODIFIED, MLN_RESOURCE_REQUEST_RANGE,
    MLN_RESOURCE_RESPONSE_EXPIRES, MLN_RESOURCE_RESPONSE_MODIFIED, MLN_RESOURCE_RESPONSE_RETRY_AFTER,
    MLN_RESOURCE_RESPONSE_STATUS_ERROR, MLN_RESOURCE_RESPONSE_STATUS_NO_CONTENT,
    MLN_RESOURCE_RESPONSE_STATUS_NOT_MODIFIED, MLN_RESOURCE_RESPONSE_STATUS_OK, MLN_RESOURCE_STORAGE_POLICY_PERMANENT,
    MLN_RESOURCE_STORAGE_POLICY_VOLATILE, MLN_RESOURCE_USAGE_OFFLINE, MLN_RESOURCE_USAGE_ONLINE, MLN_STATUS_OK,
    mln_completion, mln_diagnostic, mln_resource_error_reason, mln_resource_provider, mln_resource_range,
    mln_resource_request, mln_resource_request_cancel_handler, mln_resource_request_info, mln_resource_response,
    mln_runtime, mln_status,
};
use crate::handles::{HandleKind, HandleTable};
use crate::url_rules::{self, Routes, abi_kind};
use crate::{abi, lock, runtime};

abi::versioned!(mln_resource_provider, mln_resource_request_cancel_handler);

/// A request that the host holds a handle to, until it answers or releases
/// it.
struct RequestObject {
    request: std::sync::Mutex<Option<ResourceRequest>>,
    /// Whether the request took a cancel callback, which it takes once.
    cancel_registered: AtomicBool,
}

static REQUESTS: HandleTable<RequestObject> = HandleTable::new(HandleKind::ResourceRequest);

/// Ends `handle`, which fails its request if no answer came first.
fn release(handle: mln_resource_request) {
    if let Ok(object) = REQUESTS.remove(handle) {
        let request = lock(&object.request).take();
        drop(request);
    }
}

/// A handle that a queued call carries: dropping the call before it runs
/// releases the request, so that a released queue fails it.
struct PendingHandle(mln_resource_request);

impl PendingHandle {
    /// The handle, which the host owns from here on.
    fn hand_over(self) -> mln_resource_request {
        let handle = self.0;
        core::mem::forget(self);
        handle
    }
}

impl Drop for PendingHandle {
    fn drop(&mut self) {
        release(self.0);
    }
}

fn unix_ms(timestamp: Timestamp) -> i64 {
    timestamp.secs_since_epoch().saturating_mul(1000)
}

/// The timestamp that `unix_ms` names, to the second, as the cache stores
/// them.
fn timestamp(unix_ms: i64) -> Timestamp {
    Timestamp::from_secs(unix_ms / 1000)
}

/// What a provider's callback learns of a request, owned so that a queued
/// call can lend it later.
struct RequestInfo {
    fields: u32,
    requested_url: LentText,
    resolved_url: LentText,
    kind: u32,
    loading_method: u32,
    priority: u32,
    usage: u32,
    storage_policy: u32,
    range: mln_resource_range,
    prior_modified_unix_ms: i64,
    prior_expires_unix_ms: i64,
    prior_etag: LentText,
    prior_data: Option<mln_storage::Data>,
}

impl RequestInfo {
    fn new(resource: &Resource, resolved_url: &str) -> Self {
        let mut fields = 0;
        let mut flag = |set: bool, bit: u32| {
            if set {
                fields |= bit;
            }
        };
        flag(resource.data_range.is_some(), MLN_RESOURCE_REQUEST_RANGE);
        flag(resource.prior_modified.is_some(), MLN_RESOURCE_REQUEST_PRIOR_MODIFIED);
        flag(resource.prior_expires.is_some(), MLN_RESOURCE_REQUEST_PRIOR_EXPIRES);
        let (start, end) = resource.data_range.unwrap_or_default();
        Self {
            fields,
            requested_url: LentText::new(&resource.url),
            resolved_url: LentText::new(resolved_url),
            kind: abi_kind(resource.kind),
            loading_method: match resource.loading_method {
                LoadingMethod::CacheOnly => MLN_RESOURCE_LOADING_METHOD_CACHE_ONLY,
                LoadingMethod::NetworkOnly => MLN_RESOURCE_LOADING_METHOD_NETWORK_ONLY,
                _ => MLN_RESOURCE_LOADING_METHOD_ALL,
            },
            priority: match resource.priority {
                Priority::Regular => MLN_RESOURCE_PRIORITY_REGULAR,
                Priority::Low => MLN_RESOURCE_PRIORITY_LOW,
            },
            usage: match resource.usage {
                Usage::Online => MLN_RESOURCE_USAGE_ONLINE,
                Usage::Offline => MLN_RESOURCE_USAGE_OFFLINE,
            },
            storage_policy: match resource.storage_policy {
                StoragePolicy::Permanent => MLN_RESOURCE_STORAGE_POLICY_PERMANENT,
                StoragePolicy::Volatile => MLN_RESOURCE_STORAGE_POLICY_VOLATILE,
            },
            range: mln_resource_range { start, end },
            prior_modified_unix_ms: resource.prior_modified.map_or(0, unix_ms),
            prior_expires_unix_ms: resource.prior_expires.map_or(0, unix_ms),
            prior_etag: LentText::new(resource.prior_etag.as_deref().unwrap_or_default()),
            prior_data: resource.prior_data.clone(),
        }
    }

    /// The header's view, valid while `self` is.
    fn view(&self) -> mln_resource_request_info {
        let prior_data = self.prior_data.as_deref().map_or(&[][..], Vec::as_slice);
        mln_resource_request_info {
            size: size_of::<mln_resource_request_info>() as u32,
            fields: self.fields,
            requested_url: self.requested_url.view(),
            resolved_url: self.resolved_url.view(),
            kind: self.kind,
            loading_method: self.loading_method,
            priority: self.priority,
            usage: self.usage,
            storage_policy: self.storage_policy,
            range: self.range,
            prior_modified_unix_ms: self.prior_modified_unix_ms,
            prior_expires_unix_ms: self.prior_expires_unix_ms,
            prior_etag: self.prior_etag.view(),
            prior_data: if prior_data.is_empty() { core::ptr::null() } else { prior_data.as_ptr() },
            prior_data_size: prior_data.len(),
        }
    }
}

type ProviderCallback = unsafe extern "C" fn(*mut c_void, *const mln_resource_request_info, mln_resource_request);

/// A host's `mln_resource_provider`.
struct HostProvider {
    registration: Arc<Registration>,
    callback: ProviderCallback,
    routes: Routes,
}

impl ResourceProvider for HostProvider {
    fn serves(&self, kind: Kind, url: &str) -> bool {
        self.routes.matches(kind, url)
    }

    fn request(&self, request: ResourceRequest) {
        let info = RequestInfo::new(request.resource(), request.resolved_url());
        let object = Arc::new(RequestObject {
            request: std::sync::Mutex::new(Some(request)),
            cancel_registered: AtomicBool::new(false),
        });
        // A full table drops the request, which fails it.
        let Ok(handle) = REQUESTS.insert(object) else { return };
        let pending = PendingHandle(handle);
        let callback = self.callback;
        self.registration.deliver(move |user_data| {
            let handle = pending.hand_over();
            let info = info.view();
            // SAFETY: the host's provider callback, with its user data, a
            // request view that outlives the call, and the handle it owns.
            unsafe { callback(user_data.get(), &info, handle) };
        });
    }
}

/// `mln_runtime_set_resource_provider()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_set_resource_provider(
    runtime: mln_runtime,
    provider: *const mln_resource_provider,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let object = runtime::resolve(runtime)?;
        // SAFETY: the caller's contract.
        let provider = unsafe { abi::read_sized(provider) }?;
        let Some(callback) = provider.callback else {
            return Err(Error::invalid_argument("provider requires a callback"));
        };
        let registration = Arc::new(Registration::new(
            "provider",
            true,
            provider.user_data,
            provider.release_user_data,
            provider.queue,
        )?);
        // SAFETY: the caller's contract.
        let routes = unsafe { Routes::copy(provider.routes, provider.route_count) }?;
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::optional(completion) }?;
        // The runtime owns the user data from before the command goes out,
        // since it may replace the provider, and drop it, before this call
        // returns.
        registration.accept();
        let command = object.runtime.set_resource_provider(HostProvider {
            registration: Arc::clone(&registration),
            callback,
            routes,
        });
        if command.is_err() {
            registration.revoke();
        }
        runtime::complete_command(command, pending)
    })
}

/// `mln_runtime_clear_resource_provider()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_clear_resource_provider(
    runtime: mln_runtime,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let object = runtime::resolve(runtime)?;
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::optional(completion) }?;
        runtime::complete_command(object.runtime.clear_resource_provider(), pending)
    })
}

fn error_reason(reason: mln_resource_error_reason) -> Option<Reason> {
    match reason {
        crate::generated::types::MLN_RESOURCE_ERROR_REASON_OTHER => Some(Reason::Other),
        MLN_RESOURCE_ERROR_REASON_NOT_FOUND => Some(Reason::NotFound),
        MLN_RESOURCE_ERROR_REASON_SERVER => Some(Reason::Server),
        MLN_RESOURCE_ERROR_REASON_CONNECTION => Some(Reason::Connection),
        MLN_RESOURCE_ERROR_REASON_RATE_LIMIT => Some(Reason::RateLimit),
        _ => None,
    }
}

/// The library's error for a reason the headers name, `Other` for any other.
pub(crate) fn resource_error(
    reason: mln_resource_error_reason,
    message: String,
    retry_after: Option<Timestamp>,
) -> ResourceError {
    ResourceError::with_retry_after(error_reason(reason).unwrap_or(Reason::Other), message, retry_after)
}

/// Whether the headers name `reason`.
pub(crate) fn is_error_reason(reason: mln_resource_error_reason) -> bool {
    error_reason(reason).is_some()
}

fn failed(message: &str) -> Response {
    Response { error: Some(Box::new(ResourceError::new(Reason::Other, message))), ..Response::default() }
}

/// The response that a host's answer describes.
///
/// A malformed view in it, or too small a `size`, refuses the answer. A
/// response whose contents the library can't take, such as unknown `fields`
/// bits or status, becomes a failure that names what is wrong.
///
/// # Safety
///
/// `response` is null or points to a readable `mln_resource_response` of the
/// `size` it declares, whose views are valid.
unsafe fn host_response(response: *const mln_resource_response) -> Result<Response, Error> {
    if response.is_null() {
        return Err(Error::invalid_argument("response must not be null"));
    }
    // SAFETY: non-null, and the caller's contract.
    let size = unsafe { (&raw const (*response).size).read() } as usize;
    if size < size_of::<mln_resource_response>() {
        return Err(Error::invalid_argument("mln_resource_response.size is too small"));
    }
    // The size covers every member, so each is readable. `must_revalidate`
    // is read as a byte, since a host may store any value in a C `bool`.
    // SAFETY: as above.
    let (fields, status, reason, bytes, byte_count, error_message, must_revalidate, etag) = unsafe {
        (
            (&raw const (*response).fields).read(),
            (&raw const (*response).status).read(),
            (&raw const (*response).error_reason).read(),
            (&raw const (*response).bytes).read(),
            (&raw const (*response).byte_count).read(),
            (&raw const (*response).error_message).read(),
            (&raw const (*response).must_revalidate).cast::<u8>().read() != 0,
            (&raw const (*response).etag).read(),
        )
    };
    // SAFETY: as above.
    let (modified, expires, retry_after) = unsafe {
        (
            (&raw const (*response).modified_unix_ms).read(),
            (&raw const (*response).expires_unix_ms).read(),
            (&raw const (*response).retry_after_unix_ms).read(),
        )
    };
    // SAFETY: the caller's contract.
    let etag = unsafe { url_rules::c_text(etag, "etag") }?;
    // SAFETY: the caller's contract.
    let error_message = unsafe { url_rules::c_text(error_message, "error_message") }?;

    let known = MLN_RESOURCE_RESPONSE_MODIFIED | MLN_RESOURCE_RESPONSE_EXPIRES | MLN_RESOURCE_RESPONSE_RETRY_AFTER;
    if fields & !known != 0 {
        return Ok(failed("mln_resource_response.fields contains unknown bits"));
    }
    if !(MLN_RESOURCE_RESPONSE_STATUS_OK..=MLN_RESOURCE_RESPONSE_STATUS_NOT_MODIFIED).contains(&status) {
        return Ok(failed("resource provider returned an unknown response status"));
    }
    if byte_count != 0 && bytes.is_null() {
        return Ok(failed("resource provider returned a null byte buffer"));
    }
    let set = |bit: u32, unix_ms: i64| (fields & bit != 0).then(|| timestamp(unix_ms));
    let mut native = Response {
        no_content: status == MLN_RESOURCE_RESPONSE_STATUS_NO_CONTENT,
        not_modified: status == MLN_RESOURCE_RESPONSE_STATUS_NOT_MODIFIED,
        must_revalidate,
        modified: set(MLN_RESOURCE_RESPONSE_MODIFIED, modified),
        expires: set(MLN_RESOURCE_RESPONSE_EXPIRES, expires),
        etag: (!etag.is_empty()).then(|| etag.to_owned()),
        ..Response::default()
    };
    if status == MLN_RESOURCE_RESPONSE_STATUS_ERROR {
        let message = if error_message.is_empty() { "resource provider failed" } else { error_message };
        let retry_after = set(MLN_RESOURCE_RESPONSE_RETRY_AFTER, retry_after);
        native.error = Some(Box::new(resource_error(reason, message.to_owned(), retry_after)));
    } else if status == MLN_RESOURCE_RESPONSE_STATUS_OK {
        let bytes = if byte_count == 0 {
            Vec::new()
        } else {
            // SAFETY: non-null, and the caller's contract makes `byte_count`
            // bytes readable.
            unsafe { core::slice::from_raw_parts(bytes, byte_count) }.to_vec()
        };
        native.data = Some(Arc::new(bytes));
    }
    Ok(native)
}

/// Ends `handle` with `answer`, which the request refuses once the library
/// cancelled it.
fn answer(
    handle: mln_resource_request,
    answer: impl FnOnce(ResourceRequest) -> Result<(), mln_util::Error>,
) -> Result<(), Error> {
    let object = REQUESTS.remove(handle)?;
    let request = lock(&object.request).take();
    let request = request.ok_or_else(|| Error::invalid_state("the resource request was answered"))?;
    answer(request).map_err(Error::from)
}

/// `mln_resource_request_complete()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_resource_request_complete(
    handle: mln_resource_request,
    response: *const mln_resource_response,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    let status = export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let response = unsafe { host_response(response) }?;
        answer(handle, |request| request.respond(response))
    });
    if status != MLN_STATUS_OK {
        release(handle);
    }
    status
}

/// `mln_resource_request_pass_through()`.
///
/// # Safety
///
/// `out_diagnostic` is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_resource_request_pass_through(
    handle: mln_resource_request,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    let status = export!(out_diagnostic, { answer(handle, ResourceRequest::pass_through) });
    if status != MLN_STATUS_OK {
        release(handle);
    }
    status
}

/// `mln_resource_request_is_cancelled()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_resource_request_is_cancelled(
    handle: mln_resource_request,
    out_cancelled: *mut bool,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_cancelled), {
        let object = REQUESTS.get(handle)?;
        let cancelled = lock(&object.request).as_ref().is_none_or(ResourceRequest::is_cancelled);
        // SAFETY: non-null, and the caller's contract.
        unsafe { out_cancelled.write(cancelled) };
        Ok(())
    })
}

/// `mln_resource_request_set_cancel_callback()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_resource_request_set_cancel_callback(
    handle: mln_resource_request,
    handler: *const mln_resource_request_cancel_handler,
    out_cancelled: *mut bool,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let handler = unsafe { abi::read_sized(handler) }?;
        if out_cancelled.is_null() {
            return Err(Error::invalid_argument("out_cancelled must not be null"));
        }
        let Some(callback) = handler.callback else {
            return Err(Error::invalid_argument("cancel handler requires a callback"));
        };
        let registration = Arc::new(Registration::new(
            "cancel handler",
            true,
            handler.user_data,
            handler.release_user_data,
            handler.queue,
        )?);
        let object = REQUESTS.get(handle)?;
        if object.cancel_registered.swap(true, Ordering::AcqRel) {
            return Err(Error::invalid_state("the resource request has a cancel callback already"));
        }
        let cancel = Arc::clone(&registration);
        let on_cancel = move || {
            cancel.deliver(move |user_data| {
                // SAFETY: the host's cancel callback, with its user data and
                // the handle it borrows.
                unsafe { callback(user_data.get(), handle) };
            });
            // The registration releases its user data as the callback
            // returns, or in the queue's dispatch after it.
            drop(cancel);
        };
        let registered = match lock(&object.request).as_ref() {
            Some(request) => request.on_cancel(on_cancel),
            None => Err(mln_util::Error::invalid_state("the resource request was answered")),
        };
        let cancelled = match registered {
            Ok(()) => {
                // A cancel that already ran holds the registration until
                // this accepts it, and its release waits for that.
                registration.accept();
                false
            }
            // The callback never runs, and the user data stays with the
            // caller.
            Err(error) if error.kind == mln_util::ErrorKind::Cancelled => true,
            Err(error) => return Err(error.into()),
        };
        // SAFETY: non-null, and the caller's contract.
        unsafe { out_cancelled.write(cancelled) };
        Ok(())
    })
}

/// `mln_resource_request_release()`.
#[unsafe(no_mangle)]
pub extern "C" fn mln_resource_request_release(handle: mln_resource_request) {
    release(handle);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::types::MLN_STATUS_INVALID_ARGUMENT;

    #[test]
    fn request_handles_reject_the_null_handle() {
        let mut cancelled = false;
        // SAFETY: local arguments and null diagnostics.
        unsafe {
            mln_resource_request_release(0);
            assert_eq!(mln_resource_request_pass_through(0, core::ptr::null_mut()), MLN_STATUS_INVALID_ARGUMENT);
            assert_eq!(
                mln_resource_request_is_cancelled(0, &mut cancelled, core::ptr::null_mut()),
                MLN_STATUS_INVALID_ARGUMENT
            );
        }
    }

    fn response() -> mln_resource_response {
        mln_resource_response {
            size: size_of::<mln_resource_response>() as u32,
            // SAFETY: zeros are a valid response.
            ..unsafe { core::mem::zeroed() }
        }
    }

    #[test]
    fn a_response_carries_what_its_fields_set() {
        let body = b"tile";
        let answer = mln_resource_response {
            fields: MLN_RESOURCE_RESPONSE_MODIFIED,
            bytes: body.as_ptr(),
            byte_count: body.len(),
            modified_unix_ms: 1_700_000_000_999,
            expires_unix_ms: 5,
            ..response()
        };
        // SAFETY: a local response.
        let native = unsafe { host_response(&answer) }.unwrap();
        assert_eq!(native.data.as_deref().map(Vec::as_slice), Some(&body[..]));
        assert_eq!(native.modified, Some(Timestamp::from_secs(1_700_000_000)));
        assert_eq!(native.expires, None);
        assert!(native.error.is_none());

        let error = mln_resource_response {
            status: MLN_RESOURCE_RESPONSE_STATUS_ERROR,
            error_reason: MLN_RESOURCE_ERROR_REASON_RATE_LIMIT,
            fields: MLN_RESOURCE_RESPONSE_RETRY_AFTER,
            retry_after_unix_ms: 2000,
            ..response()
        };
        // SAFETY: a local response.
        let failure = unsafe { host_response(&error) }.unwrap().error.unwrap();
        assert_eq!(
            *failure,
            ResourceError::with_retry_after(
                Reason::RateLimit,
                "resource provider failed",
                Some(Timestamp::from_secs(2))
            )
        );
    }

    #[test]
    fn a_malformed_response_is_refused_or_fails_its_request() {
        let etag = b"a\0b";
        let refused = mln_resource_response {
            etag: crate::generated::types::mln_buffer_view { data: etag.as_ptr().cast(), size: etag.len() },
            ..response()
        };
        // SAFETY: local responses.
        unsafe {
            assert_eq!(host_response(&refused).unwrap_err().status(), MLN_STATUS_INVALID_ARGUMENT);
            assert_eq!(host_response(core::ptr::null()).unwrap_err().status(), MLN_STATUS_INVALID_ARGUMENT);
            let small = mln_resource_response { size: 4, ..response() };
            assert_eq!(host_response(&small).unwrap_err().status(), MLN_STATUS_INVALID_ARGUMENT);
            let unknown = mln_resource_response { status: 9, ..response() };
            let error = host_response(&unknown).unwrap().error.unwrap();
            assert_eq!(error.message, "resource provider returned an unknown response status");
            let null = mln_resource_response { byte_count: 3, ..response() };
            assert_eq!(
                host_response(&null).unwrap().error.unwrap().message,
                "resource provider returned a null byte buffer"
            );
        }
    }
}
