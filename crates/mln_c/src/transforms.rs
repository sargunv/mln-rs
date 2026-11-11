//! `runtime.h`'s transforms: the resource transform, which rewrites a
//! request's URL or fails the request, and the HTTP header transform, which
//! adds headers to it. Each is a host callback or a table of rules that the
//! library applies itself.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mln_storage::ResourceError;
use mln_storage::http_header_transform::HttpHeaders;
use mln_storage::resource::Kind;
use mln_storage::resource_transform::{FinishedCallback, TransformResult};
use mln_storage::response::Reason;
use mln_util::chrono::Timestamp;

use crate::buffer::LentText;
use crate::callback::{Registration, UserData};
use crate::completion::PendingCompletion;
use crate::error::Error;
use crate::generated::types::{
    MLN_RESOURCE_TRANSFORM_ERROR_RETRY_AFTER, MLN_STATUS_OK, mln_buffer_view, mln_completion, mln_diagnostic,
    mln_http_header_transform, mln_http_header_transform_response, mln_resource_kind, mln_resource_transform,
    mln_resource_transform_error, mln_resource_transform_request, mln_runtime, mln_status,
};
use crate::handles::{self, HandleKind, HandleTable};
use crate::url_rules::{self, HeaderRules, QueryRules, abi_kind};
use crate::{abi, lock, resources, runtime};

abi::versioned!(
    mln_resource_transform,
    mln_resource_transform_error,
    mln_http_header_transform,
    mln_http_header_transform_response
);

/// A transform's request that waits for the host's answer.
struct TransformRequest(Mutex<Option<FinishedCallback>>);

impl Drop for TransformRequest {
    fn drop(&mut self) {
        if let Some(finished) = self.0.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
            finished(Err(ResourceError::new(
                Reason::Other,
                "the resource transform released the request without an answer",
            )));
        }
    }
}

static TRANSFORM_REQUESTS: HandleTable<TransformRequest> = HandleTable::new(HandleKind::ResourceTransformRequest);

/// Ends `request`, which fails it if no answer came first.
fn release(request: mln_resource_transform_request) {
    drop(TRANSFORM_REQUESTS.remove(request));
}

/// A handle that a queued call carries: dropping the call before it runs
/// releases the request, so that a released queue fails it.
struct PendingRequest(mln_resource_transform_request);

impl PendingRequest {
    /// The handle, which the host owns from here on.
    fn hand_over(self) -> mln_resource_transform_request {
        let request = self.0;
        core::mem::forget(self);
        request
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        release(self.0);
    }
}

type TransformCallback = unsafe extern "C" fn(*mut c_void, mln_resource_kind, mln_buffer_view, u64);

/// A host's transform callback, which answers through a request handle.
fn host_transform(
    registration: Arc<Registration>,
    callback: TransformCallback,
) -> impl Fn(Kind, &str, FinishedCallback) + Send + Sync + 'static {
    move |kind, url, finished| {
        let object = Arc::new(TransformRequest(Mutex::new(Some(finished))));
        // A full table drops the request, which fails it.
        let Ok(request) = TRANSFORM_REQUESTS.insert(object) else { return };
        let pending = PendingRequest(request);
        let url = LentText::new(url);
        let kind = abi_kind(kind);
        registration.deliver(move |user_data| {
            let request = pending.hand_over();
            // SAFETY: the host's transform callback, with its user data, a
            // URL that outlives the call, and the request it owns.
            unsafe { callback(user_data.get(), kind, url.view(), request) };
        });
    }
}

/// `mln_runtime_set_resource_transform()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_set_resource_transform(
    runtime: mln_runtime,
    transform: *const mln_resource_transform,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let object = runtime::resolve(runtime)?;
        // SAFETY: the caller's contract.
        let transform = unsafe { abi::read_sized(transform) }?;
        let Some(callback) = transform.callback else {
            if transform.rule_count == 0 {
                return Err(Error::invalid_argument(
                    "a transform requires a callback or rules; use the clear function",
                ));
            }
            if !transform.user_data.is_null()
                || transform.release_user_data.is_some()
                || transform.queue != handles::NULL
            {
                return Err(Error::invalid_argument(
                    "a rules resource transform must not carry user_data, release_user_data, or a queue",
                ));
            }
            // SAFETY: the caller's contract.
            let rules = unsafe { QueryRules::copy(transform.rules, transform.rule_count) }?;
            // SAFETY: the caller's contract.
            let pending = unsafe { PendingCompletion::optional(completion) }?;
            let command = object.runtime.set_resource_transform(move |kind, url: &str, finished: FinishedCallback| {
                finished(Ok(rules.apply(kind, url)));
            });
            return runtime::complete_command(command, pending);
        };
        if transform.rule_count != 0 {
            return Err(Error::invalid_argument("a resource transform callback takes no rules"));
        }
        let registration = Arc::new(Registration::new(
            "resource transform",
            true,
            transform.user_data,
            transform.release_user_data,
            transform.queue,
        )?);
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::optional(completion) }?;
        // As for a resource provider, the runtime owns the user data from
        // before the command goes out.
        registration.accept();
        let command = object.runtime.set_resource_transform(host_transform(Arc::clone(&registration), callback));
        if command.is_err() {
            registration.revoke();
        }
        runtime::complete_command(command, pending)
    })
}

/// `mln_runtime_clear_resource_transform()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_clear_resource_transform(
    runtime: mln_runtime,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let object = runtime::resolve(runtime)?;
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::optional(completion) }?;
        runtime::complete_command(object.runtime.clear_resource_transform(), pending)
    })
}

/// Ends `request` with `result`.
fn answer(request: mln_resource_transform_request, result: TransformResult) -> Result<(), Error> {
    let object = TRANSFORM_REQUESTS.remove(request)?;
    let finished = lock(&object.0).take();
    let finished = finished.ok_or_else(|| Error::invalid_state("the resource transform request was answered"))?;
    finished(result);
    Ok(())
}

/// `mln_resource_transform_request_complete()`.
///
/// # Safety
///
/// `url` is a valid view, and `out_diagnostic` is null or valid for the
/// header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_resource_transform_request_complete(
    request: mln_resource_transform_request,
    url: mln_buffer_view,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    let status = export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let url = unsafe { url_rules::c_text(url, "url") }?;
        if url.is_empty() {
            return Err(Error::invalid_argument("url must not be empty"));
        }
        answer(request, Ok(url.to_owned()))
    });
    if status != MLN_STATUS_OK {
        release(request);
    }
    status
}

/// `mln_resource_transform_request_fail()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_resource_transform_request_fail(
    request: mln_resource_transform_request,
    error: *const mln_resource_transform_error,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    let status = export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let error = unsafe { abi::read_sized(error) }?;
        let fields = abi::Fields::new(
            error.fields,
            MLN_RESOURCE_TRANSFORM_ERROR_RETRY_AFTER,
            "mln_resource_transform_error.fields",
        )?;
        if !resources::is_error_reason(error.reason) {
            return Err(Error::invalid_argument(format!(
                "mln_resource_transform_error.reason {} is out of range",
                error.reason
            )));
        }
        // SAFETY: the caller's contract.
        let message = unsafe { url_rules::c_text(error.message, "message") }?;
        let message = if message.is_empty() { "resource transform failed" } else { message };
        let retry_after = fields
            .get(MLN_RESOURCE_TRANSFORM_ERROR_RETRY_AFTER, error.retry_after_unix_ms)
            .map(|unix_ms| Timestamp::from_secs(unix_ms / 1000));
        answer(request, Err(resources::resource_error(error.reason, message.to_owned(), retry_after)))
    });
    if status != MLN_STATUS_OK {
        release(request);
    }
    status
}

/// `mln_resource_transform_request_release()`.
#[unsafe(no_mangle)]
pub extern "C" fn mln_resource_transform_request_release(request: mln_resource_transform_request) {
    release(request);
}

/// The headers that header transform callbacks are setting now, by the
/// `context` of their `mln_http_header_transform_response`. An ID rather than
/// a pointer, so that a response kept past its callback names nothing.
static COLLECTING: Mutex<Option<HashMap<usize, HttpHeaders>>> = Mutex::new(None);
static NEXT_COLLECTION: AtomicUsize = AtomicUsize::new(1);

/// The headers that one callback sets, from its start until [`finish`].
struct Collection(usize);

impl Collection {
    fn start() -> Self {
        let id = NEXT_COLLECTION.fetch_add(1, Ordering::Relaxed);
        lock(&COLLECTING).get_or_insert_with(HashMap::new).insert(id, HttpHeaders::new());
        Self(id)
    }

    fn finish(self) -> HttpHeaders {
        lock(&COLLECTING).as_mut().and_then(|collecting| collecting.remove(&self.0)).unwrap_or_default()
    }
}

impl Drop for Collection {
    fn drop(&mut self) {
        if let Some(collecting) = lock(&COLLECTING).as_mut() {
            collecting.remove(&self.0);
        }
    }
}

type HeaderCallback = unsafe extern "C" fn(
    *mut c_void,
    mln_resource_kind,
    mln_buffer_view,
    *mut mln_http_header_transform_response,
) -> mln_status;

/// A host's header transform callback. Its registration releases the user
/// data once the runtime drops the transform.
struct HostHeaders {
    registration: Registration,
    user_data: UserData,
    callback: HeaderCallback,
}

impl HostHeaders {
    /// The headers that the callback sets, or none when it fails.
    fn headers(&self, kind: Kind, url: &str) -> HttpHeaders {
        let url = LentText::new(url);
        let collection = Collection::start();
        let mut response = mln_http_header_transform_response {
            size: size_of::<mln_http_header_transform_response>() as u32,
            context: collection.0 as *mut c_void,
        };
        // SAFETY: the host's header callback, with its user data, and a URL
        // and response that outlive the call.
        let status = unsafe { (self.callback)(self.user_data.get(), abi_kind(kind), url.view(), &mut response) };
        let headers = collection.finish();
        if status == MLN_STATUS_OK { headers } else { HttpHeaders::new() }
    }
}

/// `mln_runtime_set_http_header_transform()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_set_http_header_transform(
    runtime: mln_runtime,
    transform: *const mln_http_header_transform,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let object = runtime::resolve(runtime)?;
        // SAFETY: the caller's contract.
        let transform = unsafe { abi::read_sized(transform) }?;
        let Some(callback) = transform.callback else {
            if transform.rule_count == 0 {
                return Err(Error::invalid_argument(
                    "a transform requires a callback or rules; use the clear function",
                ));
            }
            if !transform.user_data.is_null() || transform.release_user_data.is_some() {
                return Err(Error::invalid_argument(
                    "a rules HTTP header transform must not carry user_data or release_user_data",
                ));
            }
            // SAFETY: the caller's contract.
            let rules = unsafe { HeaderRules::copy(transform.rules, transform.rule_count) }?;
            // SAFETY: the caller's contract.
            let pending = unsafe { PendingCompletion::optional(completion) }?;
            let command = object.runtime.set_http_header_transform(move |kind, url| rules.headers(kind, url));
            return runtime::complete_command(command, pending);
        };
        if transform.rule_count != 0 {
            return Err(Error::invalid_argument("an HTTP header transform callback takes no rules"));
        }
        let registration = Registration::new(
            "HTTP header transform",
            true,
            transform.user_data,
            transform.release_user_data,
            handles::NULL,
        )?;
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::optional(completion) }?;
        let host = Arc::new(HostHeaders { registration, user_data: UserData::new(transform.user_data), callback });
        // As for a resource provider, the runtime owns the user data from
        // before the command goes out.
        host.registration.accept();
        let command = object.runtime.set_http_header_transform({
            let host = Arc::clone(&host);
            move |kind, url| host.headers(kind, url)
        });
        if command.is_err() {
            host.registration.revoke();
        }
        runtime::complete_command(command, pending)
    })
}

/// `mln_runtime_clear_http_header_transform()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_runtime_clear_http_header_transform(
    runtime: mln_runtime,
    completion: *const mln_completion,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        let object = runtime::resolve(runtime)?;
        // SAFETY: the caller's contract.
        let pending = unsafe { PendingCompletion::optional(completion) }?;
        runtime::complete_command(object.runtime.clear_http_header_transform(), pending)
    })
}

/// `mln_http_header_transform_response_set_header()`.
///
/// # Safety
///
/// `response` is null or valid for the header's use of it, `name` and
/// `value` are valid views, and `out_diagnostic` is null or valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_http_header_transform_response_set_header(
    response: *mut mln_http_header_transform_response,
    name: mln_buffer_view,
    value: mln_buffer_view,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let response = unsafe { abi::read_sized(response.cast_const()) }?;
        // SAFETY: the caller's contract.
        let (name, value) = unsafe { url_rules::http_header(name, value) }?;
        let mut collecting = lock(&COLLECTING);
        let headers = collecting.as_mut().and_then(|collecting| collecting.get_mut(&(response.context as usize)));
        let Some(headers) = headers else {
            return Err(Error::invalid_state("HTTP headers can only be set during an HTTP header transform callback"));
        };
        match headers.iter_mut().find(|(existing, _)| existing.eq_ignore_ascii_case(&name)) {
            Some(existing) => *existing = (name, value),
            None => headers.push((name, value)),
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::types::{MLN_STATUS_INVALID_ARGUMENT, MLN_STATUS_INVALID_STATE};

    fn view(text: &str) -> mln_buffer_view {
        mln_buffer_view { data: text.as_ptr().cast(), size: text.len() }
    }

    fn set(response: &mut mln_http_header_transform_response, name: &str, value: &str) -> mln_status {
        // SAFETY: a local response and views, and a null diagnostic.
        unsafe {
            mln_http_header_transform_response_set_header(response, view(name), view(value), core::ptr::null_mut())
        }
    }

    #[test]
    fn headers_are_set_only_during_their_callback() {
        let collection = Collection::start();
        let mut response = mln_http_header_transform_response {
            size: size_of::<mln_http_header_transform_response>() as u32,
            context: collection.0 as *mut c_void,
        };
        assert_eq!(set(&mut response, "X-Token", "a"), MLN_STATUS_OK);
        assert_eq!(set(&mut response, "x-token", "b"), MLN_STATUS_OK);
        assert_eq!(set(&mut response, "X-Other", "c"), MLN_STATUS_OK);
        assert_eq!(set(&mut response, "Range", "bytes=0-1"), MLN_STATUS_INVALID_ARGUMENT);
        assert_eq!(
            collection.finish(),
            vec![("x-token".to_owned(), "b".to_owned()), ("X-Other".to_owned(), "c".to_owned())]
        );
        // A finished collection, and one that never started, take nothing.
        assert_eq!(set(&mut response, "X-Token", "a"), MLN_STATUS_INVALID_STATE);
        response.context = core::ptr::null_mut();
        assert_eq!(set(&mut response, "X-Token", "a"), MLN_STATUS_INVALID_STATE);
    }

    #[test]
    fn a_released_transform_request_fails_once() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let finished: FinishedCallback = Box::new(move |result| sender.send(result).unwrap());
        let request = TRANSFORM_REQUESTS.insert(Arc::new(TransformRequest(Mutex::new(Some(finished))))).unwrap();
        mln_resource_transform_request_release(request);
        let error = receiver.try_recv().unwrap().unwrap_err();
        assert!(error.message.contains("without an answer"), "{error}");
        // SAFETY: a local view and a null diagnostic.
        let again =
            unsafe { mln_resource_transform_request_complete(request, view("custom://a"), core::ptr::null_mut()) };
        assert_eq!(again, MLN_STATUS_INVALID_STATE);
        assert!(receiver.try_recv().is_err());
    }
}
