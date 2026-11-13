//! `HttpURLConnection` as the `HttpTransport` on Android, as MapLibre
//! Native's Android SDK fetches through the platform: Android's TLS, trust
//! store, proxy settings and network security configuration instead of
//! ureq's. Blocking transfers, each on one of the transport's worker threads,
//! which attach to the app's Java VM ([`init`](crate::android::init)) and
//! stay attached. Before `init`, every request fails.
//!
//! A request sends the headers the ureq transport sends (`Range`,
//! `If-None-Match` or `If-Modified-Since`, user agent "MapLibreNative/1.0"),
//! plus `Accept-Encoding: identity` with a `Range`, since a range of a
//! compressed body isn't a range of the resource. Without one,
//! `HttpURLConnection` asks for gzip and decodes it. The connection has no
//! cache (the ambient cache is the file source's own), and connecting or a
//! read that waits longer than `TIMEOUT_MILLISECONDS` fails.
//!
//! The transport follows redirects itself, as ureq does, also from `http` to
//! `https`, which `HttpURLConnection` wouldn't, and drops the host's headers
//! (`TransferRequest::headers`) once a redirect leaves their origin. The
//! network security configuration applies to each URL of the chain: a plain
//! HTTP URL that it refuses fails with `Reason::Other`, since no retry
//! changes the app's policy.
//!
//! A Java exception fails the transfer with its message, and a `Reason` as
//! the SDK's `HTTPRequest` maps it, except that a timeout is a connection
//! error, as for the other transports. Dropping a request disconnects its
//! connection, which ends a transfer in flight.

use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use jni::errors::Error as JniError;
use jni::objects::{Global, JByteArray, JObject, JString, JThrowable, JValue, JValueOwned};
use jni::strings::JNIString;
use jni::vm::{AttachConfig, DEFAULT_LOCAL_FRAME_CAPACITY};
use jni::{Env, JavaVM, jni_sig, jni_str};
use mln_util::chrono::rfc1123;
use parking_lot::Mutex;

use crate::http_file_source::{HttpTransport, Transfer, TransferRequest, range_header, same_origin};
use crate::redirect::{has_http_scheme, redirect_url};
use crate::response::{Reason, ResourceError};

/// How long connecting, and each read, may wait before the transfer fails:
/// bounds a stalled connection without capping a slow download that still
/// progresses, as the `NSURLSession` transport's idle timeout does.
const TIMEOUT_MILLISECONDS: i32 = 30_000;

/// ureq's default limit.
const MAXIMUM_REDIRECTS: u32 = 10;

/// The size of the Java array that a body is read through.
const BUFFER_SIZE: i32 = 64 * 1024;

/// The transport: requests wait in a queue for the next free one of its
/// worker threads, which runs the transfer.
///
/// Dropping it ends the workers once their current transfers return: a
/// dropped request disconnects its connection, which ends its transfer. It
/// waits up to `STOP_TIMEOUT` for them, and a worker still in a transfer
/// after that ends alone once it returns.
pub(crate) struct Transport {
    requests: Option<mpsc::Sender<TransferRequest>>,
    workers: Vec<JoinHandle<()>>,
    /// Disconnects once every worker has returned.
    ended: Mutex<mpsc::Receiver<()>>,
}

/// How long dropping a [`Transport`] waits for its workers, as the ureq
/// transport does: long enough for a transfer that a disconnect ends, short
/// enough not to hold up a host that drops its runtime behind a stalled
/// name lookup or connect.
const STOP_TIMEOUT: Duration = Duration::from_secs(1);

impl Transport {
    /// Starts as many workers as the online file source's default maximum
    /// of concurrent requests.
    pub(crate) fn new() -> Self {
        let (requests, receiver) = mpsc::channel::<TransferRequest>();
        let (running, ended) = mpsc::channel::<()>();
        let receiver = Arc::new(Mutex::new(receiver));
        let workers = (0..mln_util::constants::DEFAULT_MAXIMUM_CONCURRENT_REQUESTS)
            .filter_map(|i| {
                let receiver = Arc::clone(&receiver);
                let running = running.clone();
                mln_util::thread::spawn(format!("HTTPFileSource {i}"), move || {
                    // Tells the transport that the worker returned, when it drops.
                    let _running = running;
                    worker(&receiver);
                })
                .ok()
            })
            .collect();
        Transport { requests: Some(requests), workers, ended: Mutex::new(ended) }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.requests = None;
        if matches!(self.ended.get_mut().recv_timeout(STOP_TIMEOUT), Err(mpsc::RecvTimeoutError::Disconnected)) {
            for worker in self.workers.drain(..) {
                mln_util::thread::join(worker);
            }
        }
    }
}

impl HttpTransport for Transport {
    fn start(&self, request: TransferRequest) {
        if let Some(requests) = &self.requests {
            let _ = requests.send(request);
        }
    }
}

fn worker(requests: &Mutex<mpsc::Receiver<TransferRequest>>) {
    loop {
        let request = requests.lock().recv();
        let Ok(request) = request else {
            return; // the transport is gone
        };
        if request.is_cancelled() {
            continue;
        }
        let transfer = match crate::android::java_vm() {
            Some(vm) => transfer(vm, &request),
            None => Err(Box::new(ResourceError::new(Reason::Other, "HTTP requests need Android initialization"))),
        };
        request.finish(transfer);
    }
}

/// What one request of a redirect chain got.
enum Exchange {
    Done(Transfer),
    Redirect(String),
}

/// Fetches `request`'s resource, following its redirects.
fn transfer(vm: &JavaVM, request: &TransferRequest) -> Result<Transfer, Box<ResourceError>> {
    let mut url = request.resource().url.clone();
    let mut headers = request.headers();
    // The worker's own name, which the VM would otherwise replace with
    // "Thread-N" when the thread first attaches.
    let name = JNIString::from(std::thread::current().name().unwrap_or("HTTPFileSource"));
    for _ in 0..=MAXIMUM_REDIRECTS {
        // A frame of local references for each request of the chain.
        let exchange = vm
            .attach_current_thread_with_config(
                || AttachConfig::new().thread_name(&name),
                Some(DEFAULT_LOCAL_FRAME_CAPACITY),
                |env| Ok::<_, JniError>(exchange(env, request, &url, headers)),
            )
            .map_err(|error| Box::new(ResourceError::new(Reason::Other, format!("JNI failed: {error}"))))??;
        match exchange {
            Exchange::Done(transfer) => return Ok(transfer),
            Exchange::Redirect(next) => {
                if !same_origin(&url, &next) {
                    headers = &[];
                }
                url = next;
            }
        }
    }
    Err(Box::new(ResourceError::new(Reason::Other, "Too many redirects")))
}

/// One GET of `url` for `request`, with the host's `headers`, which doesn't
/// follow a redirect. Leaves no Java exception pending.
fn exchange(
    env: &mut Env<'_>,
    request: &TransferRequest,
    url: &str,
    headers: &[(String, String)],
) -> Result<Exchange, Box<ResourceError>> {
    if !has_http_scheme(url) {
        return Err(Box::new(ResourceError::new(Reason::Other, format!("Unsupported URL {url}"))));
    }
    let connection = open(env, request, url, headers).map_err(|error| Box::new(failure(env, &error)))?;
    let global = env.new_global_ref(&connection).map_err(|error| Box::new(failure(env, &error)))?;
    request.set_cancel_handler(move || disconnect(&global));
    // A connection that hasn't connected yet ignores `disconnect`.
    if request.is_cancelled() {
        return Err(Box::new(ResourceError::new(Reason::Other, "Cancelled")));
    }
    respond(env, &connection, url).map_err(|error| Box::new(failure(env, &error)))
}

/// A connection for `url`, with the request headers, that hasn't connected.
fn open<'local>(
    env: &mut Env<'local>,
    request: &TransferRequest,
    url: &str,
    headers: &[(String, String)],
) -> Result<JObject<'local>, JniError> {
    let url = env.new_string(url)?;
    let url = env.new_object(jni_str!("java/net/URL"), jni_sig!("(Ljava/lang/String;)V"), &[JValue::from(&url)])?;
    let connection =
        env.call_method(&url, jni_str!("openConnection"), jni_sig!("()Ljava/net/URLConnection;"), &[])?.l()?;
    env.call_method(&connection, jni_str!("setInstanceFollowRedirects"), jni_sig!("(Z)V"), &[JValue::Bool(false)])?;
    env.call_method(&connection, jni_str!("setUseCaches"), jni_sig!("(Z)V"), &[JValue::Bool(false)])?;
    for setter in [jni_str!("setConnectTimeout"), jni_str!("setReadTimeout")] {
        env.call_method(&connection, setter, jni_sig!("(I)V"), &[JValue::Int(TIMEOUT_MILLISECONDS)])?;
    }

    let mut set_header = |name: &str, value: &str| -> Result<(), JniError> {
        let (name, value) = (env.new_string(name)?, env.new_string(value)?);
        env.call_method(
            &connection,
            jni_str!("setRequestProperty"),
            jni_sig!("(Ljava/lang/String;Ljava/lang/String;)V"),
            &[JValue::from(&name), JValue::from(&value)],
        )?;
        env.delete_local_ref(name);
        env.delete_local_ref(value);
        Ok(())
    };
    let resource = request.resource();
    if let Some(range) = range_header(resource) {
        set_header("Range", &range)?;
        set_header("Accept-Encoding", "identity")?;
    }
    // If there's already a response, set the correct etags/modified headers to
    // make sure we are getting a 304 response if possible. This avoids
    // redownloading unchanged data.
    if let Some(prior_etag) = &resource.prior_etag {
        set_header("If-None-Match", prior_etag)?;
    } else if let Some(prior_modified) = resource.prior_modified {
        set_header("If-Modified-Since", &rfc1123(prior_modified))?;
    }
    set_header("User-Agent", "MapLibreNative/1.0")?;
    for (name, value) in headers {
        set_header(name, value)?;
    }
    Ok(connection)
}

/// Connects, and reads the response of `url`'s `connection`.
fn respond(env: &mut Env<'_>, connection: &JObject<'_>, url: &str) -> Result<Exchange, JniError> {
    let status = env.call_method(connection, jni_str!("getResponseCode"), jni_sig!("()I"), &[])?.i()?;
    let headers = response_headers(env, connection)?;
    if matches!(status, 301 | 302 | 303 | 307 | 308)
        && let Some(next) = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("location"))
            .and_then(|(_, location)| redirect_url(url, location))
    {
        discard_body(env, connection, status);
        return Ok(Exchange::Redirect(next));
    }
    let body = if status == 200 || status == 206 {
        read_body(env, connection)
    } else {
        discard_body(env, connection, status);
        Ok(Vec::new())
    };
    Ok(Exchange::Done(Transfer { status: i64::from(status), headers, body }))
}

/// The response headers of `connection`, in order.
fn response_headers(env: &mut Env<'_>, connection: &JObject<'_>) -> Result<Vec<(String, String)>, JniError> {
    let mut headers = Vec::new();
    for index in 0.. {
        let header = env.with_local_frame(4, |env| -> Result<_, JniError> {
            let value = env
                .call_method(
                    connection,
                    jni_str!("getHeaderField"),
                    jni_sig!("(I)Ljava/lang/String;"),
                    &[JValue::Int(index)],
                )?
                .l()?;
            if value.is_null() {
                return Ok(None);
            }
            let name = env
                .call_method(
                    connection,
                    jni_str!("getHeaderFieldKey"),
                    jni_sig!("(I)Ljava/lang/String;"),
                    &[JValue::Int(index)],
                )?
                .l()?;
            // The status line is a field without a name.
            Ok(Some((string(env, name)?, string(env, value)?)))
        })?;
        match header {
            None => break,
            Some((Some(name), Some(value))) => headers.push((name, value)),
            Some(_) => {}
        }
    }
    Ok(headers)
}

/// The text of `string`, or `None` for null.
fn string(env: &Env<'_>, string: JObject<'_>) -> Result<Option<String>, JniError> {
    if string.is_null() {
        return Ok(None);
    }
    env.cast_local::<JString>(string)?.try_to_string(env).map(Some)
}

/// The body of `connection`'s response. Leaves no Java exception pending.
fn read_body(env: &mut Env<'_>, connection: &JObject<'_>) -> Result<Vec<u8>, Box<ResourceError>> {
    let opened = (|| -> Result<_, JniError> {
        let length = env.call_method(connection, jni_str!("getContentLength"), jni_sig!("()I"), &[])?.i()?;
        let stream =
            env.call_method(connection, jni_str!("getInputStream"), jni_sig!("()Ljava/io/InputStream;"), &[])?.l()?;
        let buffer = JByteArray::new(env, BUFFER_SIZE as usize)?;
        Ok((length, stream, buffer))
    })();
    let (length, stream, buffer) = opened.map_err(|error| Box::new(failure(env, &error)))?;

    let mut body = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
    let mut chunk = vec![0i8; BUFFER_SIZE as usize];
    let read = loop {
        let read = env
            .call_method(
                &stream,
                jni_str!("read"),
                jni_sig!("([BII)I"),
                &[JValue::from(&buffer), JValue::Int(0), JValue::Int(BUFFER_SIZE)],
            )
            .and_then(JValueOwned::i);
        match read {
            Ok(read) if read < 0 => break Ok(()),
            Ok(read) => {
                let chunk = &mut chunk[..read as usize];
                if let Err(error) = buffer.get_region(env, 0, chunk) {
                    break Err(error);
                }
                body.extend(chunk.iter().map(|byte| byte.cast_unsigned()));
            }
            Err(error) => break Err(error),
        }
    };
    let read = read.map_err(|error| Box::new(failure(env, &error)));
    close(env, &stream);
    read.map(|()| body)
}

/// Closes the body of `connection`'s response with `status` unread, so that
/// the connection can serve another request.
fn discard_body(env: &mut Env<'_>, connection: &JObject<'_>, status: i32) {
    // Java throws for `getInputStream` of an error status.
    let (method, signature) = if status >= 400 {
        (jni_str!("getErrorStream"), jni_sig!("()Ljava/io/InputStream;"))
    } else {
        (jni_str!("getInputStream"), jni_sig!("()Ljava/io/InputStream;"))
    };
    match env.call_method(connection, method, signature, &[]).and_then(JValueOwned::l) {
        Ok(stream) if !stream.is_null() => close(env, &stream),
        Ok(_) => {}
        Err(_) => env.exception_clear(),
    }
}

/// Closes `stream`, ignoring any failure. Leaves no Java exception pending.
fn close(env: &mut Env<'_>, stream: &JObject<'_>) {
    if env.call_method(stream, jni_str!("close"), jni_sig!("()V"), &[]).is_err() {
        env.exception_clear();
    }
}

/// Disconnects `connection`, from any thread, which ends a transfer in
/// flight with an exception. A thread that wasn't attached to the VM is
/// detached again afterwards, and keeps its name.
fn disconnect(connection: &Global<JObject<'static>>) {
    let Some(vm) = crate::android::java_vm() else { return };
    let name = std::thread::current().name().map(JNIString::from);
    let _ = vm.attach_current_thread_with_config(
        || {
            let config = AttachConfig::new().scoped(true);
            match &name {
                Some(name) => config.thread_name(name),
                None => config,
            }
        },
        Some(DEFAULT_LOCAL_FRAME_CAPACITY),
        |env| -> Result<(), JniError> {
            if env.call_method(connection, jni_str!("disconnect"), jni_sig!("()V"), &[]).is_err() {
                env.exception_clear();
            }
            Ok(())
        },
    );
}

/// The error for a JNI call's failure: the pending Java exception's, which
/// it clears, or else `error`'s.
fn failure(env: &mut Env<'_>, error: &JniError) -> ResourceError {
    let Some(exception) = env.exception_occurred() else {
        return ResourceError::new(Reason::Other, format!("JNI failed: {error}"));
    };
    env.exception_clear();
    let reason = reason(env, &exception);
    let message = message(env, &exception).unwrap_or_else(|| "Error processing the request".to_owned());
    ResourceError::new(reason, message)
}

/// The `Reason` for a Java exception: `Connection` for what the SDK's
/// `HTTPRequest` counts as a connection error, and for a timeout, else
/// `Other`.
fn reason(env: &mut Env<'_>, exception: &JThrowable<'_>) -> Reason {
    let connection_errors = [
        jni_str!("java/net/UnknownHostException"),
        // Also `ConnectException` and `NoRouteToHostException`.
        jni_str!("java/net/SocketException"),
        jni_str!("java/net/ProtocolException"),
        jni_str!("javax/net/ssl/SSLException"),
        // Also `SocketTimeoutException`.
        jni_str!("java/io/InterruptedIOException"),
    ];
    for class in connection_errors {
        let is_instance = env.is_instance_of(exception, class);
        if env.exception_check() {
            env.exception_clear();
        }
        if is_instance.unwrap_or(false) {
            return Reason::Connection;
        }
    }
    Reason::Other
}

/// The message of a Java exception, if it has one. Leaves no Java exception
/// pending.
fn message(env: &mut Env<'_>, exception: &JThrowable<'_>) -> Option<String> {
    let message = env
        .call_method(exception, jni_str!("getMessage"), jni_sig!("()Ljava/lang/String;"), &[])
        .and_then(JValueOwned::l)
        .and_then(|message| string(env, message));
    if env.exception_check() {
        env.exception_clear();
    }
    message.ok().flatten()
}
