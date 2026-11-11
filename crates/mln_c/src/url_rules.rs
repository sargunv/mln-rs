//! The URL patterns of resource provider routes, resource transform rules and
//! HTTP header rules, and the checks on what those carry: text that reaches C
//! strings, and HTTP header fields.
//!
//! A pattern matches the whole URL. Without `MLN_URL_MATCH_GLOB` it matches
//! the one URL that equals it; with it, `*` matches any bytes but `/`, `**`
//! any bytes, `?` one byte but `/`, and `\` makes the next byte match only
//! itself. A glob compiles into a linear automaton, so matching takes time
//! in proportion to the pattern's tokens times the URL's bytes, whatever the
//! pattern.

use std::fmt::Write as _;

use mln_storage::resource::Kind;

use crate::buffer;
use crate::error::Error;
use crate::generated::types::{
    MLN_RESOURCE_KIND_FLAG_ALL, MLN_RESOURCE_KIND_GLYPHS, MLN_RESOURCE_KIND_IMAGE, MLN_RESOURCE_KIND_SOURCE,
    MLN_RESOURCE_KIND_SPRITE_IMAGE, MLN_RESOURCE_KIND_SPRITE_JSON, MLN_RESOURCE_KIND_STYLE, MLN_RESOURCE_KIND_TILE,
    MLN_RESOURCE_KIND_UNKNOWN, MLN_URL_MATCH_GLOB, mln_buffer_view, mln_http_header, mln_http_header_rule,
    mln_resource_kind, mln_resource_route, mln_resource_transform_rule, mln_url_query_parameter,
};

/// The most tokens a glob may hold, which bounds the work of matching one
/// URL. A run of `*` and an escaped byte are a token each.
pub(crate) const MAX_GLOB_TOKENS: usize = 4096;

/// The header's value for a resource kind.
pub(crate) fn abi_kind(kind: Kind) -> mln_resource_kind {
    match kind {
        Kind::Unknown => MLN_RESOURCE_KIND_UNKNOWN,
        Kind::Style => MLN_RESOURCE_KIND_STYLE,
        Kind::Source => MLN_RESOURCE_KIND_SOURCE,
        Kind::Tile => MLN_RESOURCE_KIND_TILE,
        Kind::Glyphs => MLN_RESOURCE_KIND_GLYPHS,
        Kind::SpriteImage => MLN_RESOURCE_KIND_SPRITE_IMAGE,
        Kind::SpriteJSON => MLN_RESOURCE_KIND_SPRITE_JSON,
        Kind::Image => MLN_RESOURCE_KIND_IMAGE,
    }
}

/// The bytes of a view that reaches a C string: neither null with a size nor
/// holding a NUL, which would cut the string short.
///
/// # Safety
///
/// As for [`buffer::bytes`].
pub(crate) unsafe fn c_bytes<'a>(view: mln_buffer_view, name: &str) -> Result<&'a [u8], Error> {
    if view.size != 0 && view.data.is_null() {
        return Err(Error::invalid_argument(format!("{name} has null data with a nonzero size")));
    }
    // SAFETY: the caller's contract.
    let bytes = unsafe { buffer::bytes(view, name) }?;
    if bytes.contains(&0) {
        return Err(Error::invalid_argument(format!("{name} contains embedded NUL")));
    }
    Ok(bytes)
}

/// The UTF-8 text of a view that reaches a C string, as for [`c_bytes`].
///
/// # Safety
///
/// As for [`buffer::bytes`].
pub(crate) unsafe fn c_text<'a>(view: mln_buffer_view, name: &str) -> Result<&'a str, Error> {
    // SAFETY: the caller's contract.
    let bytes = unsafe { c_bytes(view, name) }?;
    core::str::from_utf8(bytes).map_err(|_| Error::invalid_argument(format!("{name} is not valid UTF-8")))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Literal(u8),
    /// `?`
    Any,
    /// `*`
    Segment,
    /// `**`, or a longer run of `*`.
    Spanning,
}

/// A compiled glob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Glob {
    tokens: Vec<Token>,
}

impl Glob {
    /// The tokens that `pattern` compiles into.
    pub(crate) fn token_count(pattern: &[u8]) -> usize {
        let mut count = 0;
        let mut index = 0;
        while index < pattern.len() {
            match pattern[index] {
                b'*' => {
                    while pattern.get(index + 1) == Some(&b'*') {
                        index += 1;
                    }
                }
                b'\\' if index + 1 < pattern.len() => index += 1,
                _ => {}
            }
            count += 1;
            index += 1;
        }
        count
    }

    pub(crate) fn new(pattern: &[u8]) -> Self {
        let mut tokens = Vec::with_capacity(Self::token_count(pattern));
        let mut index = 0;
        while index < pattern.len() {
            let token = match pattern[index] {
                b'*' => {
                    let start = index;
                    while pattern.get(index + 1) == Some(&b'*') {
                        index += 1;
                    }
                    if index == start { Token::Segment } else { Token::Spanning }
                }
                b'?' => Token::Any,
                b'\\' if index + 1 < pattern.len() => {
                    index += 1;
                    Token::Literal(pattern[index])
                }
                byte => Token::Literal(byte),
            };
            tokens.push(token);
            index += 1;
        }
        Self { tokens }
    }

    pub(crate) fn matches(&self, candidate: &[u8]) -> bool {
        let tokens = &self.tokens;
        // `states[i]` says whether the first `i` tokens match the bytes seen
        // so far.
        let mut states = vec![false; tokens.len() + 1];
        states[0] = true;
        for (index, token) in tokens.iter().enumerate() {
            states[index + 1] = matches!(token, Token::Segment | Token::Spanning) && states[index];
        }
        for &byte in candidate {
            let mut before = states[0];
            states[0] = false;
            for (index, token) in tokens.iter().enumerate() {
                let old = states[index + 1];
                states[index + 1] = match token {
                    Token::Segment => states[index] || (old && byte != b'/'),
                    Token::Spanning => states[index] || old,
                    Token::Any => before && byte != b'/',
                    Token::Literal(literal) => before && byte == *literal,
                };
                before = old;
            }
        }
        states[tokens.len()]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Matcher {
    Exact(Vec<u8>),
    Glob(Glob),
}

/// The requests that a route or rule covers: kinds and a URL pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UrlPattern {
    kinds: u32,
    matcher: Matcher,
}

impl UrlPattern {
    pub(crate) fn matches(&self, kind: Kind, url: &str) -> bool {
        let kind = abi_kind(kind);
        let kind_matches = kind < 32 && (self.kinds >> kind) & 1 != 0;
        kind_matches
            && match &self.matcher {
                Matcher::Exact(exact) => exact == url.as_bytes(),
                Matcher::Glob(glob) => glob.matches(url.as_bytes()),
            }
    }

    /// Checks and copies the pattern of `what` `index`.
    ///
    /// # Safety
    ///
    /// As for [`buffer::bytes`] on `url`.
    unsafe fn new(what: &str, index: usize, kinds: u32, flags: u32, url: mln_buffer_view) -> Result<Self, Error> {
        let reject = |reason: &str| Error::invalid_argument(format!("{what} {index} {reason}"));
        // SAFETY: the caller's contract.
        let url = unsafe { c_bytes(url, "url") }.map_err(|_| reject("has an invalid url"))?;
        if kinds & !MLN_RESOURCE_KIND_FLAG_ALL != 0 {
            return Err(reject("has unknown kind bits"));
        }
        if flags & !MLN_URL_MATCH_GLOB != 0 {
            return Err(reject("has unknown flag bits"));
        }
        let matcher = if flags & MLN_URL_MATCH_GLOB != 0 {
            if Glob::token_count(url) > MAX_GLOB_TOKENS {
                return Err(reject(&format!("exceeds the glob limit of {MAX_GLOB_TOKENS} tokens")));
            }
            Matcher::Glob(Glob::new(url))
        } else {
            Matcher::Exact(url.to_vec())
        };
        // Zero kinds cover every kind.
        let kinds = if kinds == 0 { MLN_RESOURCE_KIND_FLAG_ALL } else { kinds };
        Ok(Self { kinds, matcher })
    }
}

/// A struct of the headers that leads with its `size`, in an array whose
/// elements a caller built against later headers may make larger.
///
/// # Safety
///
/// The type is a `#[repr(C)]` bindgen struct whose first field is
/// `size: u32`.
unsafe trait SizedElement: Copy {}

// SAFETY: bindgen structs of the headers that start with `size: u32`.
unsafe impl SizedElement for mln_resource_route {}
// SAFETY: as above.
unsafe impl SizedElement for mln_http_header_rule {}
// SAFETY: as above.
unsafe impl SizedElement for mln_resource_transform_rule {}

const _: () = {
    assert!(core::mem::offset_of!(mln_resource_route, size) == 0);
    assert!(core::mem::offset_of!(mln_http_header_rule, size) == 0);
    assert!(core::mem::offset_of!(mln_resource_transform_rule, size) == 0);
};

/// Copies a caller's array of `count` sized elements, stepping by each one's
/// declared size. An element smaller than this library's struct, or whose
/// size would misalign the next, is rejected.
///
/// # Safety
///
/// `elements` is null, or points to `count` readable elements, each of the
/// size it declares.
unsafe fn sized_elements<T: SizedElement>(
    elements: *const T,
    count: usize,
    what: &str,
    plural: &str,
    count_name: &str,
) -> Result<Vec<T>, Error> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if elements.is_null() {
        return Err(Error::invalid_argument(format!("{plural} must not be null with a nonzero {count_name}")));
    }
    let mut copied = Vec::with_capacity(count.min(1024));
    let mut cursor = elements.cast::<u8>();
    for index in 0..count {
        // SAFETY: the caller's contract makes the element readable, and the
        // checks on the sizes before keep it aligned.
        let size = unsafe { cursor.cast::<u32>().read() } as usize;
        if size < size_of::<T>() || size % align_of::<T>() != 0 {
            return Err(Error::invalid_argument(format!("{what} {index} has an invalid size")));
        }
        // SAFETY: as above, and the element holds at least a `T`.
        copied.push(unsafe { cursor.cast::<T>().read() });
        // SAFETY: the next element starts `size` bytes on, within the
        // caller's array, or just past its end after the last.
        cursor = unsafe { cursor.add(size) };
    }
    Ok(copied)
}

/// The items of a caller's plain array.
///
/// # Safety
///
/// `items` is null, or points to `count` readable items.
unsafe fn items<'a, T>(items: *const T, count: usize, error: impl FnOnce() -> Error) -> Result<&'a [T], Error> {
    if count == 0 {
        return Ok(&[]);
    }
    if items.is_null() {
        return Err(error());
    }
    // SAFETY: non-null, and the caller's contract.
    Ok(unsafe { core::slice::from_raw_parts(items, count) })
}

/// A resource provider's routes. Without routes, it serves every request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Routes(Vec<UrlPattern>);

impl Routes {
    /// Checks and copies a caller's routes.
    ///
    /// # Safety
    ///
    /// As for [`sized_elements`], and each route's `url` is a valid view.
    pub(crate) unsafe fn copy(routes: *const mln_resource_route, count: usize) -> Result<Self, Error> {
        // SAFETY: the caller's contract.
        let routes = unsafe { sized_elements(routes, count, "route", "routes", "route_count") }?;
        let patterns = routes
            .iter()
            .enumerate()
            // SAFETY: the caller's contract.
            .map(|(index, route)| unsafe { UrlPattern::new("route", index, route.kinds, route.flags, route.url) })
            .collect::<Result<_, _>>()?;
        Ok(Self(patterns))
    }

    pub(crate) fn matches(&self, kind: Kind, url: &str) -> bool {
        self.0.is_empty() || self.0.iter().any(|route| route.matches(kind, url))
    }
}

/// An HTTP header's name and value, checked as
/// `mln_http_header_transform_response_set_header()` checks them. A failure's
/// message names the header but never its value.
///
/// # Safety
///
/// As for [`buffer::bytes`] on both views.
pub(crate) unsafe fn http_header(name: mln_buffer_view, value: mln_buffer_view) -> Result<(String, String), Error> {
    // SAFETY: the caller's contract.
    let name = unsafe { c_bytes(name, "HTTP header name") }?;
    // SAFETY: the caller's contract.
    let value = unsafe { c_bytes(value, "HTTP header value") }?;
    let is_token = |byte: &u8| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(byte);
    if name.is_empty() || !name.iter().all(is_token) {
        return Err(Error::invalid_argument("HTTP header name is not a valid field name"));
    }
    if !value.iter().all(|&byte| byte == b'\t' || (byte >= 0x20 && byte != 0x7F)) {
        return Err(Error::invalid_argument("HTTP header value contains a disallowed control byte"));
    }
    let value =
        core::str::from_utf8(value).map_err(|_| Error::invalid_argument("HTTP header value must be valid UTF-8"))?;
    // A field name is ASCII.
    let name = String::from_utf8_lossy(name).into_owned();
    if mln_runtime::is_managed_http_header(&name) {
        return Err(Error::invalid_argument(format!(
            "HTTP header \"{name}\" is managed by the library or the platform transport"
        )));
    }
    Ok((name, value.to_owned()))
}

/// Headers to add to the requests that a pattern covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeaderRule {
    pattern: UrlPattern,
    headers: Vec<(String, String)>,
}

/// An HTTP header transform's rules: the first that covers a request adds
/// its headers, and no other.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HeaderRules(Vec<HeaderRule>);

impl HeaderRules {
    /// Checks and copies a caller's rules.
    ///
    /// # Safety
    ///
    /// As for [`sized_elements`], and each rule's `url` and `headers` are
    /// valid.
    pub(crate) unsafe fn copy(rules: *const mln_http_header_rule, count: usize) -> Result<Self, Error> {
        // SAFETY: the caller's contract.
        let rules = unsafe { sized_elements(rules, count, "header rule", "rules", "rule_count") }?;
        let mut copied = Vec::with_capacity(rules.len());
        for (index, rule) in rules.iter().enumerate() {
            // SAFETY: the caller's contract.
            let pattern = unsafe { UrlPattern::new("header rule", index, rule.kinds, rule.flags, rule.url) }?;
            // SAFETY: the caller's contract.
            let headers: &[mln_http_header] = unsafe {
                items(rule.headers, rule.header_count, || {
                    Error::invalid_argument(format!("header rule {index} has null headers with a nonzero header_count"))
                })
            }?;
            let headers = headers
                .iter()
                // SAFETY: the caller's contract.
                .map(|header| unsafe { http_header(header.name, header.value) })
                .collect::<Result<_, _>>()
                .map_err(|error| Error::invalid_argument(format!("header rule {index}: {}", error.message())))?;
            copied.push(HeaderRule { pattern, headers });
        }
        Ok(Self(copied))
    }

    /// The headers for a request of `kind` to `url`.
    pub(crate) fn headers(&self, kind: Kind, url: &str) -> Vec<(String, String)> {
        self.0.iter().find(|rule| rule.pattern.matches(kind, url)).map(|rule| rule.headers.clone()).unwrap_or_default()
    }
}

/// Query parameters to add to the URLs that a pattern covers, encoded and
/// joined with `&`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueryRule {
    pattern: UrlPattern,
    query: String,
}

/// A resource transform's rules: the first that covers a request adds its
/// parameters, and no other.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct QueryRules(Vec<QueryRule>);

impl QueryRules {
    /// Checks and copies a caller's rules. A failure's message names the
    /// parameter but never its value.
    ///
    /// # Safety
    ///
    /// As for [`sized_elements`], and each rule's `url` and `parameters` are
    /// valid.
    pub(crate) unsafe fn copy(rules: *const mln_resource_transform_rule, count: usize) -> Result<Self, Error> {
        const WHAT: &str = "resource transform rule";
        // SAFETY: the caller's contract.
        let rules = unsafe { sized_elements(rules, count, WHAT, "rules", "rule_count") }?;
        let mut copied = Vec::with_capacity(rules.len());
        for (index, rule) in rules.iter().enumerate() {
            // SAFETY: the caller's contract.
            let pattern = unsafe { UrlPattern::new(WHAT, index, rule.kinds, rule.flags, rule.url) }?;
            // SAFETY: the caller's contract.
            let parameters: &[mln_url_query_parameter] = unsafe {
                items(rule.parameters, rule.parameter_count, || {
                    Error::invalid_argument(format!(
                        "{WHAT} {index} has null parameters with a nonzero parameter_count"
                    ))
                })
            }?;
            let mut query = String::new();
            for (number, parameter) in parameters.iter().enumerate() {
                let reject =
                    |reason: &str| Error::invalid_argument(format!("{WHAT} {index} parameter {number} has {reason}"));
                if parameter.name.size == 0 {
                    return Err(reject("an empty name"));
                }
                // SAFETY: the caller's contract.
                let name = unsafe { c_text(parameter.name, "name") }.map_err(|_| reject("an invalid name"))?;
                // SAFETY: the caller's contract.
                let value = unsafe { c_text(parameter.value, "value") }.map_err(|_| reject("an invalid value"))?;
                if !query.is_empty() {
                    query.push('&');
                }
                percent_encode(&mut query, name);
                query.push('=');
                percent_encode(&mut query, value);
            }
            copied.push(QueryRule { pattern, query });
        }
        Ok(Self(copied))
    }

    /// `url` with the parameters of the first rule that covers it, or as it
    /// is.
    pub(crate) fn apply(&self, kind: Kind, url: &str) -> String {
        match self.0.iter().find(|rule| rule.pattern.matches(kind, url)) {
            Some(rule) => append_query(url, &rule.query),
            None => url.to_owned(),
        }
    }
}

/// Percent-encodes every byte of `text` but RFC 3986's unreserved
/// characters.
fn percent_encode(out: &mut String, text: &str) {
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
}

/// `url` with `query` at the end of its query, before any fragment.
fn append_query(url: &str, query: &str) -> String {
    let fragment = url.find('#').unwrap_or(url.len());
    let (base, rest) = url.split_at(fragment);
    let mut rewritten = String::with_capacity(url.len() + query.len() + 1);
    rewritten.push_str(base);
    if !query.is_empty() {
        if !base.contains('?') {
            rewritten.push('?');
        } else if !base.ends_with(['?', '&']) {
            rewritten.push('&');
        }
        rewritten.push_str(query);
    }
    rewritten.push_str(rest);
    rewritten
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glob(pattern: &str, url: &str) -> bool {
        Glob::new(pattern.as_bytes()).matches(url.as_bytes())
    }

    #[test]
    fn globs_match_segments_trees_and_single_bytes() {
        assert!(glob("https://*.example.com/tiles/**", "https://a.example.com/tiles/1/2/3.pbf"));
        assert!(!glob("https://*.example.com/tiles/**", "https://a.b/c.example.com/tiles/1"));
        assert!(glob("http://127.0.0.1:*/routed/**", "http://127.0.0.1:8080/routed/style.json"));
        assert!(!glob("custom://*", "custom://a/b"));
        assert!(glob("custom://**", "custom://a/b"));
        assert!(glob("custom://?", "custom://a"));
        assert!(!glob("custom://?", "custom:///"));
        assert!(!glob("custom://?", "custom://ab"));
        assert!(glob("**", ""));
        assert!(glob("a*", "a"));
        assert!(!glob("", "a"));
        assert!(glob("", ""));
    }

    #[test]
    fn an_escape_matches_its_byte_only() {
        assert!(glob(r"a\*b", "a*b"));
        assert!(!glob(r"a\*b", "axb"));
        assert!(glob(r"a\?", "a?"));
        assert!(!glob(r"a\?", "ab"));
        // A trailing backslash is a literal one.
        assert!(glob("a\\", "a\\"));
    }

    #[test]
    fn tokens_count_runs_and_escapes_once() {
        assert_eq!(Glob::token_count(b"****"), 1);
        assert_eq!(Glob::token_count(b"\\?\\?"), 2);
        assert_eq!(Glob::token_count(b"a?*b**"), 5);
        let many = "?".repeat(4096 * 4);
        let started = std::time::Instant::now();
        assert!(!Glob::new(b"**a**b**c**d").matches(many.as_bytes()));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_query_goes_before_the_fragment() {
        assert_eq!(append_query("http://h/a", "k=v"), "http://h/a?k=v");
        assert_eq!(append_query("http://h/a?x=1", "k=v"), "http://h/a?x=1&k=v");
        assert_eq!(append_query("http://h/a?", "k=v"), "http://h/a?k=v");
        assert_eq!(append_query("http://h/a?x=1&", "k=v"), "http://h/a?x=1&k=v");
        assert_eq!(append_query("http://h/a#top", "k=v"), "http://h/a?k=v#top");
        assert_eq!(append_query("http://h/a", ""), "http://h/a");
    }

    #[test]
    fn parameters_encode_all_but_unreserved_bytes() {
        let mut out = String::new();
        percent_encode(&mut out, "a/b \u{e9}&c=~-._+%41");
        assert_eq!(out, "a%2Fb%20%C3%A9%26c%3D~-._%2B%2541");
    }

    fn view(text: &[u8]) -> mln_buffer_view {
        mln_buffer_view { data: text.as_ptr().cast(), size: text.len() }
    }

    #[test]
    fn headers_must_be_fields_the_library_does_not_manage() {
        // SAFETY: each view names a live slice.
        unsafe {
            assert_eq!(http_header(view(b"X-Token"), view(b"a b")).unwrap(), ("X-Token".into(), "a b".into()));
            let refused: [(&[u8], &[u8]); 6] = [
                (b"Bad Name", b"v"),
                (b"", b"v"),
                (b"X", b"bad\r\nvalue"),
                (b"range", b"bytes=0-1"),
                (b"X", b"\xff"),
                (b"X", b"a\0b"),
            ];
            for (name, value) in refused {
                let error = http_header(view(name), view(value)).unwrap_err();
                assert!(!error.message().contains("value\""), "{error}");
            }
            let null = mln_buffer_view { data: core::ptr::null(), size: 1 };
            assert!(http_header(null, view(b"v")).is_err());
        }
    }

    #[test]
    fn patterns_cover_their_kinds() {
        // SAFETY: the view names a live slice.
        let pattern = unsafe { UrlPattern::new("route", 0, 0, 0, view(b"custom://a")) }.unwrap();
        assert!(pattern.matches(Kind::Style, "custom://a"));
        assert!(pattern.matches(Kind::Image, "custom://a"));
        assert!(!pattern.matches(Kind::Style, "custom://b"));
        let tiles =
            unsafe { UrlPattern::new("route", 0, 1 << MLN_RESOURCE_KIND_TILE, MLN_URL_MATCH_GLOB, view(b"**")) }
                .unwrap();
        assert!(tiles.matches(Kind::Tile, "anything"));
        assert!(!tiles.matches(Kind::Style, "anything"));
        let error = unsafe { UrlPattern::new("route", 3, 0, 1 << 31, view(b"**")) }.unwrap_err();
        assert_eq!(error.message(), "route 3 has unknown flag bits");
    }
}
