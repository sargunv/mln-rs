//! maplibre-native-ffi's C ABI, as a C library (`libmaplibre_native_c`, shared
//! and static) for hosts that link maplibre-native-ffi's.
//!
//! The headers in `include/` are maplibre-native-ffi's. `src/generated` comes
//! from them (`just generate-c-abi`): their types, and a stub for each function
//! this crate doesn't define, which fails with `MLN_STATUS_UNSUPPORTED`. To
//! implement a function, define it outside `generated/` as
//! `#[unsafe(no_mangle)] pub unsafe extern "C" fn` with the header's signature
//! and regenerate; the generated code then checks that signature.
//! maplibre-native-ffi's ABI suite runs against the library (`just c-abi-test`).
//!
//! Each header's functions live in the module of its name. The modules they
//! share hold the C-only rules: `export!` bodies, `handles`, struct versioning
//! (`abi`), `buffer`s, `callback` registrations, `delivery` queues and
//! `completion`s, the event batches that handlers receive (`events`), the
//! headers' geometry values (`geo`), and the
//! `parse_thread` that parses for callers that wait.

// Every export takes raw pointers from C callers.
#![allow(unsafe_code)]

#[macro_use]
mod export;

mod abi;
mod base;
mod buffer;
mod callback;
mod camera;
mod completion;
mod delivery;
mod diagnostic;
mod error;
mod events;
mod geo;
mod handles;
mod logging;
mod map;
mod parse_thread;
mod plugin;
mod projection;
mod render_target;
mod resources;
mod runtime;
mod style_expression;
mod transforms;
mod url_rules;

#[rustfmt::skip]
#[allow(dead_code, non_camel_case_types, non_snake_case, non_upper_case_globals, unused_variables, clippy::all)]
mod generated {
    pub mod types;
    mod stubs;
    mod signatures;
}

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks `mutex`, poisoned or not: no code that holds one of this crate's
/// locks leaves the data half-changed if it panics.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
