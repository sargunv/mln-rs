// Ports include/mln/style/source.hpp and src/mln/style/source.cpp.
//
// C++ `Source` is an abstract class with data members (`baseImpl`,
// `observer`, `loaded`); the data is `SourceBase` and the virtual interface
// is the `Source` trait.
//
// Deviation (structural): C++ sources call their `SourceObserver*` (the
// owning `Style::Impl`) from setters and from asynchronous request
// callbacks, which hold a `WeakPtr` to the source. A Rust callback cannot
// point into the style's collection, so a source keeps its mutable state
// behind interior mutability in an `Rc` (`SourceBase` fields are cells; the
// concrete sources are handles to an `Rc`'d inner state), the callbacks hold
// a `Weak` to it, and every source method takes `&self`. The style's
// observer (see style_impl.rs) then reads the changed `baseImpl` through the
// source, as C++ `sources.update(source)` does.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use mln_storage::FileSource;
use mln_style_types::SourceType;
use mln_util::chrono::Duration;
use mln_util::immutable::Immutable;
use mln_util::logging::{Event, Log};

use crate::layer::LayerTypeInfo;
use crate::observer::SourceObserver;
pub use crate::source_impl::{Impl, ImplBase, static_immutable_cast};

/// The data members of `Source`.
pub struct SourceBase {
    base_impl: RefCell<Immutable<dyn Impl>>,
    /// `SourceObserver* observer` (default `&nullObserver`).
    observer: RefCell<Option<Rc<dyn SourceObserver>>>,
    loaded: Cell<bool>,
}

impl std::fmt::Debug for SourceBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceBase")
            .field("base_impl", &self.base_impl.borrow())
            .field("loaded", &self.loaded.get())
            .finish_non_exhaustive()
    }
}

/// `SourceObserver nullObserver`.
struct NullSourceObserver;
impl SourceObserver for NullSourceObserver {}

impl SourceBase {
    /// `Source(Immutable<Impl>)`.
    pub fn new(impl_: Immutable<dyn Impl>) -> Self {
        SourceBase { base_impl: RefCell::new(impl_), observer: RefCell::new(None), loaded: Cell::new(false) }
    }

    pub fn base_impl(&self) -> Immutable<dyn Impl> {
        self.base_impl.borrow().clone()
    }

    /// `baseImpl = std::move(newImpl)`.
    pub fn set_base_impl(&self, impl_: Immutable<dyn Impl>) {
        *self.base_impl.borrow_mut() = impl_;
    }

    pub fn observer(&self) -> Rc<dyn SourceObserver> {
        match &*self.observer.borrow() {
            Some(observer) => observer.clone(),
            None => Rc::new(NullSourceObserver),
        }
    }

    pub fn set_observer(&self, observer: Option<Rc<dyn SourceObserver>>) {
        *self.observer.borrow_mut() = observer;
    }

    pub fn loaded(&self) -> bool {
        self.loaded.get()
    }

    pub fn set_loaded(&self, loaded: bool) {
        self.loaded.set(loaded);
    }
}

/// `style::Source`.
pub trait Source {
    fn base(&self) -> &SourceBase;

    fn as_any(&self) -> &dyn Any;

    /// `loadDescription(FileSource&)`.
    fn load_description(&self, file_source: &dyn FileSource);

    fn supports_layer_type(&self, type_info: &LayerTypeInfo) -> bool;

    /// `createMutable()`: a copy of the current impl.
    fn create_mutable(&self) -> Box<dyn Impl>;

    // Non-virtual members of the C++ class.

    /// The `baseImpl` member.
    fn base_impl(&self) -> Immutable<dyn Impl> {
        self.base().base_impl()
    }

    /// The `loaded` member.
    fn loaded(&self) -> bool {
        self.base().loaded()
    }

    fn get_type(&self) -> SourceType {
        self.base_impl().type_
    }

    fn get_id(&self) -> String {
        self.base_impl().id.clone()
    }

    fn get_attribution(&self) -> Option<String> {
        self.base_impl().get_attribution()
    }

    fn is_volatile(&self) -> bool {
        self.base_impl().is_volatile()
    }

    fn set_observer(&self, observer: Option<Rc<dyn SourceObserver>>) {
        self.base().set_observer(observer);
    }

    fn get_prefetch_zoom_delta(&self) -> Option<u8> {
        self.base_impl().get_prefetch_zoom_delta()
    }

    fn get_minimum_tile_update_interval(&self) -> Duration {
        self.base_impl().get_minimum_tile_update_interval()
    }

    fn get_max_overscale_factor_for_parent_tiles(&self) -> Option<u8> {
        self.base_impl().get_max_overscale_factor_for_parent_tiles()
    }

    fn dump_debug_logs(&self) {
        Log::info(Event::General, format!("Source::id: {}", self.get_id()));
        Log::info(Event::General, format!("Source::loaded: {}", i32::from(self.loaded())));
    }
}

/// Whether `a` and `b` are the same source, rather than two that share an
/// ID: handles that clone one source are the same.
pub fn same_source(a: &dyn Source, b: &dyn Source) -> bool {
    std::ptr::eq(a.base(), b.base())
}

/// Replaces the impl with a mutated copy and notifies the observer, as the
/// C++ setters do (`createMutable()`, mutate, `baseImpl = ...`,
/// `observer->onSourceChanged(*this)`).
fn mutate_impl(source: &dyn Source, f: impl FnOnce(&mut dyn Impl)) {
    let mut new_impl = source.create_mutable();
    f(new_impl.as_mut());
    source.base().set_base_impl(Immutable::from(new_impl));
    source.base().observer().on_source_changed(source);
}

/// `setVolatile(bool)`.
pub fn set_volatile(source: &dyn Source, set: bool) {
    if source.is_volatile() == set {
        return;
    }
    mutate_impl(source, |impl_| impl_.base_mut().set_volatile(set));
}

/// `setPrefetchZoomDelta(std::optional<uint8_t>)`.
pub fn set_prefetch_zoom_delta(source: &dyn Source, delta: Option<u8>) {
    if source.get_prefetch_zoom_delta() == delta {
        return;
    }
    mutate_impl(source, |impl_| impl_.base_mut().set_prefetch_zoom_delta(delta));
}

/// `setMinimumTileUpdateInterval(Duration)`.
pub fn set_minimum_tile_update_interval(source: &dyn Source, interval: Duration) {
    if source.get_minimum_tile_update_interval() == interval {
        return;
    }
    mutate_impl(source, |impl_| impl_.base_mut().set_minimum_tile_update_interval(interval));
}

/// `setMaxOverscaleFactorForParentTiles(std::optional<uint8_t>)`.
pub fn set_max_overscale_factor_for_parent_tiles(source: &dyn Source, overscale_factor: Option<u8>) {
    if source.get_max_overscale_factor_for_parent_tiles() == overscale_factor {
        return;
    }
    mutate_impl(source, |impl_| impl_.base_mut().set_max_overscale_factor_for_parent_tiles(overscale_factor));
}

// Debug is not a supertrait, for the reason at `impl Debug for dyn Layer`
// (layer.rs).
impl std::fmt::Debug for dyn Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let base = self.base_impl();
        f.debug_struct("Source").field("id", &base.id).field("type", &base.type_).finish_non_exhaustive()
    }
}
