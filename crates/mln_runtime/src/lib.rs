//! Thread-safe handles over mln-rs's maps, which mirror
//! maplibre-native-ffi's C API one to one.
//!
//! A [`Runtime`] owns a thread, and the file sources, cache and offline
//! regions of its maps. Each [`Map`] lives on its runtime's thread, and its
//! handle sends commands there: they run in the order sent, across the
//! runtime and all its maps, and report through a [`Completion`]. A map
//! publishes a [`MapSnapshot`] after each command, which any thread reads
//! without waiting, and the events of the runtime and its maps reach one
//! handler in batches.
//!
//! The synchronous, single-thread core that these handles drive is
//! `mln_map`'s; [`Map::with_core`] runs a closure on it.

mod completion;
mod event;
mod feature_state;
mod map;
mod offline;
mod resources;
mod runtime;

pub use completion::Completion;
pub use event::{EventHandler, EventMask, EventSource, EventType, RuntimeEvent, RuntimeEventKind};
pub use feature_state::{FeatureState, FeatureStateSelector};
pub use map::{Applied, CameraQuery, CommandGroup, Map, MapId, MapOptions};
pub use mln_map::MapSnapshot;
pub use mln_util::{Error, ErrorKind};
pub use offline::Offline;
pub use resources::{CacheDatabaseOperation, ResourceProvider, ResourceRequest};
pub use runtime::{Runtime, RuntimeOptions};
