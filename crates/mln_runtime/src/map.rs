//! A handle to a map that lives on its runtime's thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mln_actor::RunLoopHandle;
use mln_map::{
    BoundOptions, Camera, CameraDelta, CameraOptions, CameraUpdate, FreeCameraOptions, MapDebugOptions, MapEvent,
    MapMode, MapSnapshot, ProjectionMode,
};
use mln_text::GlyphCache;
use mln_util::error::{Error, ErrorKind};
use mln_util::geo::LatLngBounds;
use mln_util::size::Size;
use mln_util::sync::MutexExt;

use crate::completion::{Completion, Resolver};
use crate::event::{EventMask, EventSource, RuntimeEvent, RuntimeEventKind};
use crate::feature_state::{FeatureState, FeatureStateSelector, FeatureStateStore};
use crate::render::{AttachOptions, RenderSession, RenderTarget, SessionId, Worker, worker_post};
use crate::runtime::{Outbox, Runtime, RuntimeCore, RuntimeShared, post};
use crate::style::{CallbackSources, GeoJsonUpdates};

/// The ID of a map, unique within its runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MapId(pub u64);

/// How to create a map.
#[derive(Clone, Debug)]
pub struct MapOptions {
    pub map: mln_map::MapOptions,
    /// The events that the map reports.
    pub event_mask: EventMask,
    /// JSON text of the font faces that the map falls back to for glyphs
    /// that the style's fonts lack, as `Style::set_fallback_font_faces`
    /// takes it.
    pub fallback_font_faces: Option<String>,
    /// Whether the renderers of the map keep tiles in memory after they
    /// leave the view, so they show again without loading. On by default.
    pub tile_cache_enabled: bool,
}

impl Default for MapOptions {
    fn default() -> Self {
        MapOptions {
            map: mln_map::MapOptions::default(),
            event_mask: EventMask::default(),
            fallback_font_faces: None,
            tile_cache_enabled: true,
        }
    }
}

impl MapOptions {
    pub fn new(map: mln_map::MapOptions) -> Self {
        MapOptions { map, ..Self::default() }
    }

    pub fn with_event_mask(mut self, event_mask: EventMask) -> Self {
        self.event_mask = event_mask;
        self
    }

    pub fn with_fallback_font_faces(mut self, json: impl Into<String>) -> Self {
        self.fallback_font_faces = Some(json.into());
        self
    }

    pub fn with_tile_cache_enabled(mut self, enabled: bool) -> Self {
        self.tile_cache_enabled = enabled;
        self
    }
}

/// What a map command left behind once it ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Applied {
    /// The command's place among the map's commands: the map's creation is
    /// 1, and each command takes the next number when it is sent.
    pub sequence: u64,
    /// The map's generation after the command: a snapshot with this
    /// generation or a later one includes the command.
    pub generation: u64,
    /// The map's update generation after the command: a frame that renders
    /// this update or a later one shows the command.
    pub render_update_generation: u64,
}

/// The camera, and the generation of the map that it belongs to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraQuery {
    pub generation: u64,
    pub camera: Camera,
}

/// What the runtime's thread publishes for a map's handles.
pub(crate) struct Published {
    snapshot: Mutex<Option<MapSnapshot>>,
    /// The sequence number of the last command that ran.
    applied: AtomicU64,
    event_mask: AtomicU64,
}

impl Published {
    pub(crate) fn new(event_mask: EventMask) -> Self {
        Published {
            snapshot: Mutex::new(None),
            applied: AtomicU64::new(0),
            event_mask: AtomicU64::new(event_mask.bits()),
        }
    }
}

pub(crate) struct MapShared {
    id: MapId,
    runtime: Arc<RuntimeShared>,
    mode: MapMode,
    pixel_ratio: f32,
    /// The render session attached to the map, from its attach until its
    /// detach completes.
    session: Mutex<Option<SessionId>>,
    /// The sequence number of the last command sent. It stays locked while
    /// a command is numbered and posted, so the runtime's thread runs the
    /// map's commands in the order of their numbers.
    sent: Mutex<u64>,
    released: AtomicBool,
    published: Arc<Published>,
}

impl MapShared {
    pub(crate) fn new(
        id: MapId,
        runtime: Arc<RuntimeShared>,
        options: &mln_map::MapOptions,
        published: Arc<Published>,
    ) -> Self {
        MapShared {
            id,
            runtime,
            mode: options.map_mode(),
            pixel_ratio: options.pixel_ratio(),
            session: Mutex::new(None),
            sent: Mutex::new(1),
            released: AtomicBool::new(false),
            published,
        }
    }

    /// Numbers a command and posts it to the runtime's thread.
    pub(crate) fn send<R: Send + 'static>(
        &self,
        command: impl FnOnce(&mut RuntimeCore, &mut Outbox, MapId, u64, Resolver<R>) + Send + 'static,
    ) -> Result<Completion<R>, Error> {
        let mut sent = self.sent.lock_unpoisoned();
        if self.released.load(Ordering::SeqCst) {
            return Err(released());
        }
        let (completion, resolver) = self.runtime.completion();
        let id = self.id;
        *sent += 1;
        let sequence = *sent;
        self.runtime.post(move |core, outbox| command(core, outbox, id, sequence, resolver));
        Ok(completion)
    }

    /// Releases the map: no command is sent after its removal. Fails when
    /// the map was released already.
    fn release(&self, done: Option<Resolver<()>>) -> Result<(), Error> {
        {
            let _sent = self.sent.lock_unpoisoned();
            if self.released.swap(true, Ordering::SeqCst) {
                return Err(released());
            }
            let id = self.id;
            self.runtime.post(move |core, outbox| remove(core, id, done, outbox));
        }
        self.runtime.map_released();
        Ok(())
    }
}

impl Drop for MapShared {
    fn drop(&mut self) {
        let _ = self.release(None);
    }
}

/// A handle to a map, which lives on its runtime's thread.
///
/// Each command takes the next sequence number when it is sent and runs on
/// the runtime's thread after every command sent before it, to this map,
/// another map or the runtime. It returns a [`Completion`] of its result, or
/// an error when it can't be sent, and then never completes. A command sent
/// before the map is built waits for it. `Clone` is another handle to the
/// same map, and any thread may use one.
///
/// The map lives until [`release`](Self::release) or until its last handle
/// drops. A command on a released map fails with
/// [`ErrorKind::InvalidState`](mln_util::ErrorKind).
#[derive(Clone)]
pub struct Map {
    pub(crate) shared: Arc<MapShared>,
}

impl std::fmt::Debug for Map {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Map").field("id", &self.shared.id).finish()
    }
}

impl PartialEq for Map {
    fn eq(&self, other: &Map) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

impl Eq for Map {}

fn released() -> Error {
    Error::invalid_state("the map was released")
}

pub(crate) fn style_not_loaded() -> Error {
    Error::invalid_state("style is not loaded")
}

/// The error of a command whose map failed to be built, or was released
/// while the command was on its way.
pub(crate) fn missing_map() -> Error {
    Error::invalid_state("the map failed to be built or was released")
}

impl Map {
    pub(crate) fn new(shared: MapShared) -> Self {
        Map { shared: Arc::new(shared) }
    }

    pub fn id(&self) -> MapId {
        self.shared.id
    }

    /// When the map renders, as its options set it.
    pub fn mode(&self) -> MapMode {
        self.shared.mode
    }

    /// The runtime that the map lives on.
    pub fn runtime(&self) -> Runtime {
        Runtime { shared: self.shared.runtime.clone() }
    }

    /// The sequence number of the last command sent to the map.
    pub fn sent_sequence(&self) -> u64 {
        *self.shared.sent.lock_unpoisoned()
    }

    /// The sequence number of the last command that ran.
    pub fn applied_sequence(&self) -> u64 {
        self.shared.published.applied.load(Ordering::SeqCst)
    }

    /// The map's state after the last command that ran, without waiting for
    /// the commands still on their way. To read the state after them, send a
    /// command and read the snapshot once it completes, or use a query such
    /// as [`camera`](Self::camera).
    ///
    /// Fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind) until the
    /// map is built, and after it is released.
    pub fn snapshot(&self) -> Result<MapSnapshot, Error> {
        if self.shared.released.load(Ordering::SeqCst) {
            return Err(released());
        }
        self.shared.published.snapshot.lock_unpoisoned().ok_or_else(|| Error::invalid_state("the map isn't built"))
    }

    /// The events that the map reports, as of the last command that ran.
    pub fn event_mask(&self) -> EventMask {
        EventMask::from_bits(self.shared.published.event_mask.load(Ordering::SeqCst)).unwrap_or_default()
    }

    /// Sends a command that runs `apply` on the map's entry, and completes
    /// with what `finish` makes of its result and the map after it.
    pub(crate) fn send<T: 'static, R: Send + 'static>(
        &self,
        apply: impl FnOnce(&mut MapEntry, &mut Outbox) -> Result<T, Error> + Send + 'static,
        finish: impl FnOnce(T, &MapEntry) -> R + Send + 'static,
    ) -> Result<Completion<R>, Error> {
        self.shared.send(move |core, outbox, id, sequence, resolver| {
            run_command(core, id, sequence, apply, finish, resolver, outbox);
        })
    }

    /// A command that changes the map.
    pub(crate) fn command(
        &self,
        apply: impl FnOnce(&mut mln_map::Map) -> Result<(), Error> + Send + 'static,
    ) -> Result<Completion<Applied>, Error> {
        self.send(move |entry, _| apply(&mut entry.core), |(), entry| entry.applied())
    }

    /// A command that reads the map.
    pub(crate) fn query<T: Send + 'static>(
        &self,
        read: impl FnOnce(&mut mln_map::Map) -> Result<T, Error> + Send + 'static,
    ) -> Result<Completion<T>, Error> {
        self.send(move |entry, _| read(&mut entry.core), |value, _| value)
    }

    /// Runs `f` on the map itself, on the runtime's thread, in its place
    /// among the map's commands. It sees the effects of the commands sent
    /// before it. The map handles what `f` changed, and reports its events,
    /// before the completion.
    pub fn with_core<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut mln_map::Map) -> R + Send + 'static,
    ) -> Result<Completion<R>, Error> {
        self.query(move |map| Ok(f(map)))
    }

    /// Releases the map. The completion reports when the runtime has let go
    /// of it. Fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind)
    /// when the map was released already, or while a render session is
    /// attached to it.
    pub fn release(&self) -> Result<Completion<()>, Error> {
        if self.shared.session.lock_unpoisoned().is_some() {
            return Err(Error::invalid_state("the map still has an attached render session"));
        }
        let (completion, resolver) = self.shared.runtime.completion();
        self.shared.release(Some(resolver))?;
        Ok(completion)
    }

    /// Releases the map without a completion, as dropping its last handle
    /// does, and lets the runtime finish in the background. Fails with
    /// [`ErrorKind::InvalidState`](mln_util::ErrorKind) when the map was
    /// released already.
    pub fn dispose(&self) -> Result<(), Error> {
        self.shared.release(None)
    }

    // MARK: - Map

    /// Sets the map's size. A resize to the size the map has changes
    /// nothing, so it renders nothing. Fails with
    /// [`ErrorKind::InvalidState`](mln_util::ErrorKind) while a render
    /// session is attached, since the session sizes the map to its target:
    /// resize the session instead. Fails with
    /// [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for an empty
    /// size.
    pub fn resize(&self, size: Size) -> Result<Completion<Applied>, Error> {
        if self.shared.session.lock_unpoisoned().is_some() {
            return Err(Error::invalid_state("the map has a render session: resize the session instead"));
        }
        if size.is_empty() {
            return Err(Error::invalid_argument("logical size dimensions must be positive"));
        }
        self.command(move |map| if map.get_map_options().size() == size { Ok(()) } else { map.set_size(size) })
    }

    /// Renders a new frame of a continuous map, even when nothing changed.
    /// Fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind) for a
    /// static or tile map, which renders only still images.
    pub fn request_repaint(&self) -> Result<Completion<Applied>, Error> {
        if self.shared.mode != MapMode::Continuous {
            return Err(Error::invalid_state("map is not in continuous mode"));
        }
        self.command(|map| {
            map.trigger_repaint();
            Ok(())
        })
    }

    /// Holds the map's changes back from rendering until the matching
    /// [`end_command_group`](Self::end_command_group), so that they show in
    /// the same frame. Groups nest, and the changes show when the outermost
    /// group ends.
    pub fn begin_command_group(&self) -> Result<Completion<Applied>, Error> {
        self.command(|map| {
            map.begin_command_group();
            Ok(())
        })
    }

    /// Ends the group that [`begin_command_group`](Self::begin_command_group)
    /// began. Completes with [`ErrorKind::InvalidState`](mln_util::ErrorKind)
    /// when no group is open.
    pub fn end_command_group(&self) -> Result<Completion<Applied>, Error> {
        self.command(|map| map.end_command_group())
    }

    /// Begins a command group that ends when the guard drops, or with
    /// [`CommandGroup::end`].
    pub fn command_group(&self) -> Result<CommandGroup, Error> {
        drop(self.begin_command_group()?);
        Ok(CommandGroup { map: Some(self.clone()) })
    }

    /// Selects the events that the map reports.
    pub fn set_event_mask(&self, mask: EventMask) -> Result<Completion<Applied>, Error> {
        self.send(
            move |entry, _| {
                entry.mask = mask;
                entry.published.event_mask.store(mask.bits(), Ordering::SeqCst);
                Ok(())
            },
            |(), entry| entry.applied(),
        )
    }

    // MARK: - Rendering

    /// Attaches a render session, which renders the map into `target` on a
    /// thread of its own, and sizes the map to the target. A map has one
    /// session at a time. The session takes frame demands and other work at
    /// once, and runs them once the completion reports it ready.
    ///
    /// Fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind) when the
    /// map already has a session or was released,
    /// [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for an empty
    /// target, and [`ErrorKind::Unsupported`](mln_util::ErrorKind) for a
    /// driver that the target can't use. The completion fails as
    /// [`OffscreenDevice::new`](crate::OffscreenDevice::new) does when the
    /// session has to create its device and can't.
    pub fn attach(
        &self,
        target: RenderTarget,
        options: AttachOptions,
    ) -> Result<(RenderSession, Completion<()>), Error> {
        crate::render::attach(self, target, options)
    }

    /// Renders a still image of a static or tile map, once every resource
    /// that it needs has loaded. The map's render session draws it, and
    /// [`RenderSession::read_texture`] reads its pixels once the completion
    /// reports success. Without a session, the image waits for the next one.
    ///
    /// Fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind) for a
    /// continuous map. Completes with
    /// [`ErrorKind::InvalidState`](mln_util::ErrorKind) while another image
    /// is on its way, [`ErrorKind::Native`](mln_util::ErrorKind) when the
    /// style or a resource that the image needs fails to load, and
    /// [`ErrorKind::Cancelled`](mln_util::ErrorKind) when the map is released
    /// first.
    pub fn request_still_image(&self) -> Result<Completion<()>, Error> {
        if self.shared.mode == MapMode::Continuous {
            return Err(Error::invalid_state("the map is not in static or tile mode"));
        }
        self.shared.send(move |core, outbox, id, sequence, resolver| {
            if !core.maps.contains_key(&id) {
                return outbox.resolve(resolver, Err(missing_map()));
            }
            apply_command(
                core,
                id,
                sequence,
                outbox,
                move |entry, outbox| {
                    if entry.pending_still.is_some() {
                        let error = Error::invalid_state("the map is rendering a still image already");
                        return outbox.resolve(resolver, Err(error));
                    }
                    let outcome = Rc::new(RefCell::new(None));
                    let done = outcome.clone();
                    entry.core.render_still(Box::new(move |error| {
                        *done.borrow_mut() = Some(error.map_or(Ok(()), |error| Err(still_image_error(error))));
                    }));
                    entry.pending_still = Some(PendingStill { resolver, outcome });
                },
                |_, (), _, _| {},
            );
        })
    }

    pub(crate) fn pixel_ratio(&self) -> f32 {
        self.shared.pixel_ratio
    }

    pub(crate) fn glyph_cache(&self) -> Arc<GlyphCache> {
        self.shared.runtime.glyph_cache.clone()
    }

    /// Reserves the map for the session `id`.
    pub(crate) fn claim_session(&self, id: SessionId) -> Result<(), Error> {
        let mut session = self.shared.session.lock_unpoisoned();
        if self.shared.released.load(Ordering::SeqCst) {
            return Err(released());
        }
        if session.is_some() {
            return Err(Error::invalid_state("the map already has an attached render session"));
        }
        *session = Some(id);
        Ok(())
    }

    /// Lets another session attach once the session `id` is gone.
    pub(crate) fn release_session_claim(&self, id: SessionId) {
        let mut session = self.shared.session.lock_unpoisoned();
        if *session == Some(id) {
            *session = None;
        }
    }

    /// Sends the command that binds the session `id`, whose thread runs
    /// `run_loop`, to the map: it sizes the map to `size`, and hands the
    /// session the map's updates.
    pub(crate) fn bind_session(&self, id: SessionId, run_loop: RunLoopHandle, size: Size) -> Result<(), Error> {
        let sent = self.shared.send(move |core, outbox, map_id, sequence, resolver: Resolver<()>| {
            drop(resolver);
            if !core.maps.contains_key(&map_id) {
                return worker_post(&run_loop, |worker, outbox| worker.bind_failed(missing_map(), outbox));
            }
            let link = SessionLink { id, run_loop: run_loop.clone() };
            apply_command(
                core,
                map_id,
                sequence,
                outbox,
                move |entry, _| {
                    entry.session = Some(link);
                    let resized = entry.core.set_size(size);
                    (resized, entry.core.update_reader(), entry.core.renderer_events())
                },
                move |_, (resized, reader, events), _, _| match resized {
                    Ok(()) => {
                        worker_post(&run_loop, move |worker, outbox| worker.bound(sequence, reader, events, outbox));
                    }
                    Err(error) => worker_post(&run_loop, move |worker, outbox| worker.bind_failed(error, outbox)),
                },
            );
        });
        sent.map(drop)
    }

    /// Unbinds the session `id` from the map on the runtime's thread, after
    /// the commands sent before, then runs `then` on the session's thread.
    pub(crate) fn unbind_session(
        &self,
        id: SessionId,
        run_loop: RunLoopHandle,
        then: impl FnOnce(&mut Worker, &mut Outbox) + Send + 'static,
    ) {
        let map_id = self.shared.id;
        self.shared.runtime.post(move |core, _| {
            if let Some(entry) = core.maps.get_mut(&map_id)
                && entry.session.as_ref().is_some_and(|link| link.id == id)
            {
                entry.session = None;
            }
            worker_post(&run_loop, then);
        });
    }

    /// Sends a command that resizes the map to `size`, then runs `resize`
    /// with its outcome on the thread of the session that `run_loop` runs,
    /// before the session learns of any later command.
    pub(crate) fn resize_for_session(
        &self,
        size: Size,
        run_loop: RunLoopHandle,
        resize: impl FnOnce(&mut Worker, Result<Applied, Error>, &mut Outbox) + Send + 'static,
    ) -> Result<(), Error> {
        let sent = self.shared.send(move |core, outbox, id, sequence, resolver: Resolver<()>| {
            drop(resolver);
            if !core.maps.contains_key(&id) {
                return worker_post(&run_loop, move |worker, outbox| resize(worker, Err(missing_map()), outbox));
            }
            apply_command(
                core,
                id,
                sequence,
                outbox,
                move |entry, _| entry.core.set_size(size),
                move |_, resized, applied, _| {
                    let result = resized.map(|()| applied);
                    worker_post(&run_loop, move |worker, outbox| resize(worker, result, outbox));
                },
            );
        });
        sent.map(drop)
    }

    // MARK: - Feature state

    /// Merges `state` into the state of the feature that `selector` names:
    /// its keys replace the same keys of the feature's state, and the other
    /// keys stay.
    ///
    /// The map keeps the state until it is removed, its source leaves the
    /// style, or a style loads: [`remove_style_source`](Self::remove_style_source)
    /// removes the state of the source's features, and a style that loads
    /// removes the state of every source. To set state for a new style's
    /// sources, wait until [`set_style_url`](Self::set_style_url) or
    /// [`set_style_json`](Self::set_style_json) completes.
    ///
    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for a
    /// selector without a feature, or an empty state.
    pub fn set_feature_state(
        &self,
        selector: FeatureStateSelector,
        state: FeatureState,
    ) -> Result<Completion<Applied>, Error> {
        let key = selector.key(true)?;
        if state.is_empty() {
            return Err(Error::invalid_argument("feature state must not be empty"));
        }
        self.send(
            move |entry, _| {
                entry.feature_state.set(key, state);
                Ok(())
            },
            |(), entry| entry.applied(),
        )
    }

    /// The state of the feature that `selector` names, empty when it has
    /// none. Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind)
    /// for a selector without a feature.
    pub fn feature_state(&self, selector: FeatureStateSelector) -> Result<Completion<FeatureState>, Error> {
        let key = selector.key(true)?;
        self.send(move |entry, _| Ok(entry.feature_state.get(&key)), |state, _| state)
    }

    /// Removes the state that `selector` selects: one key of a feature, all
    /// of a feature's state, or that of every feature in a source layer.
    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for a
    /// state key without a feature.
    pub fn remove_feature_state(&self, selector: FeatureStateSelector) -> Result<Completion<Applied>, Error> {
        let key = selector.key(false)?;
        self.send(
            move |entry, _| {
                entry.feature_state.remove(&key);
                Ok(())
            },
            |(), entry| entry.applied(),
        )
    }

    // MARK: - Style

    /// Loads the map's style from `url`.
    ///
    /// Completes once the style document has loaded and parsed, with
    /// [`ErrorKind::Native`](mln_util::ErrorKind) when it fails to, and with
    /// [`ErrorKind::Cancelled`](mln_util::ErrorKind) when another style load
    /// starts or the map is released first. The style's sources, sprites and
    /// glyphs then load in the background. Commands that change the style
    /// fail with [`ErrorKind::InvalidState`](mln_util::ErrorKind) until the
    /// document has parsed.
    pub fn set_style_url(&self, url: impl Into<String>) -> Result<Completion<Applied>, Error> {
        let url = url.into();
        if url.is_empty() {
            return Err(Error::invalid_argument("style URL must not be empty"));
        }
        self.load_style(StyleLoad::Url(url))
    }

    /// Loads the map's style from JSON text. Completes as
    /// [`set_style_url`](Self::set_style_url) does, once the style has
    /// parsed. A style that fails to parse leaves the map's style as it was.
    pub fn set_style_json(&self, json: impl Into<String>) -> Result<Completion<Applied>, Error> {
        let json = json.into();
        if json.is_empty() {
            return Err(Error::invalid_argument("style JSON must not be empty"));
        }
        self.load_style(StyleLoad::Json(json))
    }

    fn load_style(&self, load: StyleLoad) -> Result<Completion<Applied>, Error> {
        self.shared.send(move |core, outbox, id, sequence, resolver| {
            load_style(core, id, sequence, load, resolver, outbox);
        })
    }

    /// The JSON of the style that the map loaded, without the changes made
    /// since.
    pub fn loaded_style_json(&self) -> Result<Completion<String>, Error> {
        self.query(|map| Ok(map.get_style().get_json()))
    }

    /// The URL of the map's style, or an empty string for a style loaded
    /// from JSON.
    pub fn style_url(&self) -> Result<Completion<String>, Error> {
        self.query(|map| Ok(map.get_style().get_url()))
    }

    /// Adds a source to the style from its JSON. See
    /// `Style::add_source_json` for the errors.
    pub fn add_style_source_json(
        &self,
        source_id: impl Into<String>,
        json: impl Into<String>,
    ) -> Result<Completion<Applied>, Error> {
        let (source_id, json) = (source_id.into(), json.into());
        self.command(move |map| map.get_style_mut().add_source_json(&source_id, &json))
    }

    /// Removes a source from the style. Completes with
    /// [`ErrorKind::NotFound`](mln_util::ErrorKind) when the style has no
    /// such source, and [`ErrorKind::InvalidState`](mln_util::ErrorKind)
    /// when a layer uses it or the style isn't loaded. The source's features
    /// lose their state, and a custom source's callbacks drop before the
    /// completion.
    pub fn remove_style_source(&self, source_id: impl Into<String>) -> Result<Completion<Applied>, Error> {
        let source_id = source_id.into();
        self.send(
            move |entry, _| {
                let style = entry.core.get_style_mut();
                if !style.is_loaded() {
                    return Err(style_not_loaded());
                }
                if style.get_source(&source_id).is_none() {
                    return Err(Error::not_found(format!("source \"{source_id}\" does not exist")));
                }
                if let Some(layer) = style.get_layers().into_iter().find(|layer| layer.get_source_id() == source_id) {
                    let message = format!("source \"{source_id}\" is used by layer \"{}\"", layer.get_id());
                    return Err(Error::invalid_state(message));
                }
                style.remove_source(&source_id);
                entry.feature_state.remove_source(&source_id);
                Ok(())
            },
            |(), entry| entry.applied(),
        )
    }

    /// Adds a layer to the style from its JSON, below `before_layer_id` or
    /// above every layer. See `Style::add_layer_json` for the errors.
    pub fn add_style_layer_json(
        &self,
        json: impl Into<String>,
        before_layer_id: Option<String>,
    ) -> Result<Completion<Applied>, Error> {
        let json = json.into();
        self.command(move |map| map.get_style_mut().add_layer_json(&json, before_layer_id.as_deref()))
    }

    /// Removes a layer from the style. Completes with
    /// [`ErrorKind::NotFound`](mln_util::ErrorKind) when the style has no
    /// such layer, and [`ErrorKind::InvalidState`](mln_util::ErrorKind)
    /// while the style isn't loaded.
    pub fn remove_style_layer(&self, layer_id: impl Into<String>) -> Result<Completion<Applied>, Error> {
        let layer_id = layer_id.into();
        self.command(move |map| {
            let style = map.get_style_mut();
            if !style.is_loaded() {
                return Err(style_not_loaded());
            }
            style
                .remove_layer(&layer_id)
                .map(drop)
                .ok_or_else(|| Error::not_found(format!("layer \"{layer_id}\" does not exist")))
        })
    }

    /// A layer of the style as JSON, or `None` when the style has no such
    /// layer.
    pub fn style_layer_json(&self, layer_id: impl Into<String>) -> Result<Completion<Option<String>>, Error> {
        let layer_id = layer_id.into();
        self.query(move |map| Ok(map.get_style().layer_json(&layer_id)))
    }

    /// Sets a property of a layer to a JSON value. See
    /// `Style::set_layer_property_json` for the errors.
    pub fn set_style_layer_property(
        &self,
        layer_id: impl Into<String>,
        name: impl Into<String>,
        json: impl Into<String>,
    ) -> Result<Completion<Applied>, Error> {
        let (layer_id, name, json) = (layer_id.into(), name.into(), json.into());
        self.command(move |map| map.get_style_mut().set_layer_property_json(&layer_id, &name, &json))
    }

    /// A property of a layer as JSON, or `None` when the layer doesn't set
    /// it. See `Style::layer_property_json` for the errors.
    pub fn style_layer_property(
        &self,
        layer_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<Completion<Option<String>>, Error> {
        let (layer_id, name) = (layer_id.into(), name.into());
        self.query(move |map| map.get_style().layer_property_json(&layer_id, &name))
    }

    /// Replaces the style's light with one from JSON.
    pub fn set_style_light_json(&self, json: impl Into<String>) -> Result<Completion<Applied>, Error> {
        let json = json.into();
        self.command(move |map| map.get_style_mut().set_light_json(&json))
    }

    /// Sets a property of the style's light to a JSON value.
    pub fn set_style_light_property(
        &self,
        name: impl Into<String>,
        json: impl Into<String>,
    ) -> Result<Completion<Applied>, Error> {
        let (name, json) = (name.into(), json.into());
        self.command(move |map| map.get_style_mut().set_light_property_json(&name, &json))
    }

    /// A property of the style's light as JSON, or `None` when the light
    /// doesn't set it.
    pub fn style_light_property(&self, name: impl Into<String>) -> Result<Completion<Option<String>>, Error> {
        let name = name.into();
        self.query(move |map| Ok(map.get_style().light_property_json(&name)))
    }

    // MARK: - Camera

    /// Moves the camera to absolute values, by a jump, an ease or a flight.
    pub fn update_camera(&self, update: CameraUpdate) -> Result<Completion<Applied>, Error> {
        self.command(move |map| {
            map.update_camera(&update);
            Ok(())
        })
    }

    /// Moves the camera relative to where it is, as a gesture does.
    /// Completes with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for
    /// a delta that the camera can't apply.
    pub fn apply_camera_delta(&self, delta: CameraDelta) -> Result<Completion<Applied>, Error> {
        self.command(move |map| {
            map.apply_camera_delta(&delta).map_err(|error| Error::invalid_argument(error.to_string()))
        })
    }

    /// Stops every camera animation.
    pub fn cancel_camera_transitions(&self) -> Result<Completion<Applied>, Error> {
        self.command(|map| {
            map.cancel_transitions();
            Ok(())
        })
    }

    /// Stops the camera animations whose transition ID is `transition_id`.
    pub fn cancel_camera_transitions_with_id(&self, transition_id: u64) -> Result<Completion<Applied>, Error> {
        self.command(move |map| {
            map.cancel_transitions_with_id(transition_id);
            Ok(())
        })
    }

    /// The camera after the commands sent before.
    pub fn camera(&self) -> Result<Completion<CameraQuery>, Error> {
        self.send(
            |_, _| Ok(()),
            |(), entry| CameraQuery { generation: entry.core.generation(), camera: entry.core.snapshot().view.camera },
        )
    }

    /// The area that `camera` would show.
    pub fn lat_lng_bounds_for_camera(&self, camera: CameraOptions) -> Result<Completion<LatLngBounds>, Error> {
        self.query(move |map| map.lat_lng_bounds_for_camera(&camera))
    }

    /// Sets the limits on the camera.
    pub fn set_bounds(&self, bounds: BoundOptions) -> Result<Completion<Applied>, Error> {
        self.command(move |map| map.set_bounds(&bounds))
    }

    /// Sets the camera's position and orientation in three dimensions.
    pub fn set_free_camera_options(&self, camera: FreeCameraOptions) -> Result<Completion<Applied>, Error> {
        self.command(move |map| {
            map.set_free_camera_options(&camera);
            Ok(())
        })
    }

    pub fn set_projection_mode(&self, mode: ProjectionMode) -> Result<Completion<Applied>, Error> {
        self.command(move |map| {
            map.set_projection_mode(&mode);
            Ok(())
        })
    }

    /// Selects the debug overlays that the map draws.
    pub fn set_debug_flags(&self, flags: MapDebugOptions) -> Result<Completion<Applied>, Error> {
        self.command(move |map| {
            map.set_debug(flags);
            Ok(())
        })
    }

    /// Shows or hides the rendering statistics over the map.
    pub fn set_rendering_stats_view_enabled(&self, enabled: bool) -> Result<Completion<Applied>, Error> {
        self.command(move |map| {
            map.enable_rendering_stats_view(enabled);
            Ok(())
        })
    }

    /// Logs the style's sources and their state.
    pub fn dump_debug_logs(&self) -> Result<Completion<Applied>, Error> {
        self.command(|map| {
            map.dump_debug_logs();
            Ok(())
        })
    }
}

/// A command group of a map that ends when it drops.
#[must_use = "the group ends when it drops"]
pub struct CommandGroup {
    map: Option<Map>,
}

impl CommandGroup {
    /// Ends the group, as [`Map::end_command_group`] does.
    pub fn end(mut self) -> Result<Completion<Applied>, Error> {
        match self.map.take() {
            Some(map) => map.end_command_group(),
            None => Err(Error::invalid_state("the group ended already")),
        }
    }
}

impl Drop for CommandGroup {
    fn drop(&mut self) {
        if let Some(map) = self.map.take() {
            let _ = map.end_command_group();
        }
    }
}

enum StyleLoad {
    Url(String),
    Json(String),
}

/// The render session bound to a map, as the runtime's thread knows it.
pub(crate) struct SessionLink {
    id: SessionId,
    run_loop: RunLoopHandle,
}

/// A still image on its way: the map's callback leaves its outcome for the
/// next flush.
struct PendingStill {
    resolver: Resolver<()>,
    outcome: Rc<RefCell<Option<Result<(), Error>>>>,
}

/// A still image's error as the handle reports it: a load failure is the
/// library's, whatever the core called it.
fn still_image_error(error: Error) -> Error {
    match error.kind {
        ErrorKind::InvalidState => error,
        _ => Error::native(error.message),
    }
}

/// A map on the runtime's thread.
pub(crate) struct MapEntry {
    pub(crate) core: mln_map::Map,
    published: Arc<Published>,
    mask: EventMask,
    /// The sequence number of the last command that ran.
    applied: u64,
    /// The completion of the style load in progress.
    pending_style: Option<Resolver<Applied>>,
    pending_still: Option<PendingStill>,
    /// The render session that draws the map.
    session: Option<SessionLink>,
    /// Whether a flush of the map's events is on its way.
    flush_scheduled: Arc<AtomicBool>,
    pub(crate) feature_state: FeatureStateStore,
    /// The callbacks of the custom sources that commands added.
    pub(crate) callback_sources: CallbackSources,
    /// The GeoJSON data that commands set and that is still being built.
    pub(crate) geojson_updates: GeoJsonUpdates,
    #[expect(dead_code, reason = "the renderers of the map's render sessions read it")]
    pub(crate) tile_cache_enabled: bool,
}

impl MapEntry {
    pub(crate) fn applied(&self) -> Applied {
        Applied {
            sequence: self.applied,
            generation: self.core.generation(),
            render_update_generation: self.core.pending_update_generation(),
        }
    }
}

/// Builds the map `id` on the runtime's thread.
pub(crate) fn create(
    core: &mut RuntimeCore,
    id: MapId,
    options: MapOptions,
    published: Arc<Published>,
    resolver: Resolver<Applied>,
    outbox: &mut Outbox,
) {
    let (file_sources, resource_options) = core.file_sources();
    let mut map = match mln_map::Map::with_resource_options(options.map, file_sources, resource_options) {
        Ok(map) => map,
        Err(error) => return outbox.resolve(resolver, Err(error)),
    };
    if let Some(json) = &options.fallback_font_faces
        && let Err(error) = map.get_style_mut().set_fallback_font_faces(json)
    {
        return outbox.resolve(resolver, Err(error));
    }
    let flush_scheduled = Arc::new(AtomicBool::new(false));
    let scheduled = flush_scheduled.clone();
    let run_loop = core.run_loop.clone();
    map.set_waker(Some(Arc::new(move || {
        if !scheduled.swap(true, Ordering::SeqCst) {
            post(&run_loop, move |core, outbox| flush(core, id, outbox));
        }
    })));
    published.applied.store(1, Ordering::SeqCst);
    core.maps.insert(
        id,
        MapEntry {
            core: map,
            published,
            mask: options.event_mask,
            applied: 1,
            pending_style: None,
            pending_still: None,
            session: None,
            flush_scheduled,
            feature_state: FeatureStateStore::default(),
            callback_sources: CallbackSources::default(),
            geojson_updates: GeoJsonUpdates::default(),
            tile_cache_enabled: options.tile_cache_enabled,
        },
    );
    flush(core, id, outbox);
    if let Some(entry) = core.maps.get(&id) {
        outbox.resolve(resolver, Ok(entry.applied()));
    }
}

/// Runs a command on the map `id`, which must be there: handles what came
/// before it, applies it, publishes the map's state, sends its events, and
/// runs `after` with what `apply` returned. Then it tells the map's render
/// session that the command ran, so `after` can send the session what the
/// session must see first.
pub(crate) fn apply_command<T>(
    core: &mut RuntimeCore,
    id: MapId,
    sequence: u64,
    outbox: &mut Outbox,
    apply: impl FnOnce(&mut MapEntry, &mut Outbox) -> T,
    after: impl FnOnce(&MapEntry, T, Applied, &mut Outbox),
) {
    flush(core, id, outbox);
    let Some(entry) = core.maps.get_mut(&id) else { return };
    let value = apply(entry, outbox);
    entry.applied = sequence;
    flush(core, id, outbox);
    let Some(entry) = core.maps.get(&id) else { return };
    after(entry, value, entry.applied(), outbox);
    entry.published.applied.store(sequence, Ordering::SeqCst);
    if let Some(link) = &entry.session {
        worker_post(&link.run_loop, move |worker, outbox| worker.map_applied(sequence, outbox));
    }
}

/// Runs a command on the map `id` and completes with what `finish` makes of
/// its result and the map after it.
pub(crate) fn run_command<T, R: 'static>(
    core: &mut RuntimeCore,
    id: MapId,
    sequence: u64,
    apply: impl FnOnce(&mut MapEntry, &mut Outbox) -> Result<T, Error>,
    finish: impl FnOnce(T, &MapEntry) -> R,
    resolver: Resolver<R>,
    outbox: &mut Outbox,
) {
    if !core.maps.contains_key(&id) {
        return outbox.resolve(resolver, Err(missing_map()));
    }
    apply_command(core, id, sequence, outbox, apply, move |entry, result, _, outbox| {
        outbox.resolve(resolver, result.map(|value| finish(value, entry)));
    });
}

/// Starts a style load on the map `id`. Its completion waits for the load's
/// outcome, which a flush finds among the map's events.
fn load_style(
    core: &mut RuntimeCore,
    id: MapId,
    sequence: u64,
    load: StyleLoad,
    resolver: Resolver<Applied>,
    outbox: &mut Outbox,
) {
    if !core.maps.contains_key(&id) {
        return outbox.resolve(resolver, Err(missing_map()));
    }
    // The flush before the load sends the outcomes of earlier loads to their
    // own completions.
    apply_command(
        core,
        id,
        sequence,
        outbox,
        move |entry, outbox| {
            if let Some(previous) = entry.pending_style.replace(resolver) {
                outbox.resolve(previous, Err(Error::cancelled("another style load started")));
            }
            let style = entry.core.get_style_mut();
            match load {
                StyleLoad::Url(url) => style.load_url(&url),
                StyleLoad::Json(json) => style.load_json(&json),
            }
        },
        |_, (), _, _| {},
    );
}

/// Handles what the map's style and renderer reported, publishes its state,
/// and sends its events.
pub(crate) fn flush(core: &mut RuntimeCore, id: MapId, outbox: &mut Outbox) {
    let Some(entry) = core.maps.get_mut(&id) else { return };
    entry.flush_scheduled.store(false, Ordering::SeqCst);
    let events = entry.core.drain_events();
    // The sources that left the style, by a command or a style load, let go
    // of what they held before anything reports the change.
    crate::style::reconcile(entry, outbox);
    let generation = entry.core.generation();
    *entry.published.snapshot.lock_unpoisoned() = Some(entry.core.snapshot());

    let mut style_outcome = None;
    for event in &events {
        match event {
            MapEvent::StyleLoaded => {
                // A new style's sources start without state.
                entry.feature_state.clear();
                style_outcome = Some(Ok(()));
            }
            MapEvent::LoadingFailed(error) => style_outcome = Some(Err(Error::native(error.message()))),
            _ => {}
        }
    }
    let style_outcome = style_outcome.map(|outcome| (outcome, entry.pending_style.take(), entry.applied()));
    let still_outcome = entry.pending_still.as_ref().and_then(|pending| pending.outcome.borrow_mut().take());
    let still = still_outcome.and_then(|outcome| Some((outcome, entry.pending_still.take()?.resolver)));

    let mask = entry.mask;
    let events: Vec<RuntimeEvent> = events
        .into_iter()
        .map(RuntimeEventKind::Map)
        .filter(|kind| mask.contains(kind.event_type()))
        .map(|kind| RuntimeEvent { source: EventSource::Map(id), generation, kind })
        .collect();
    core.send_events(events, outbox);

    if let Some((outcome, Some(pending), applied)) = style_outcome {
        outbox.resolve(pending, outcome.map(|()| applied));
    }
    if let Some((outcome, resolver)) = still {
        outbox.resolve(resolver, outcome);
    }
}

/// Lets go of the map `id`, after sending the events it still had.
fn remove(core: &mut RuntimeCore, id: MapId, done: Option<Resolver<()>>, outbox: &mut Outbox) {
    flush(core, id, outbox);
    if let Some(mut entry) = core.maps.remove(&id) {
        entry.core.set_waker(None);
        if let Some(pending) = entry.pending_style.take() {
            outbox.resolve(pending, Err(Error::cancelled("the map was released")));
        }
        if let Some(pending) = entry.pending_still.take() {
            outbox.resolve(pending.resolver, Err(Error::cancelled("the map was released")));
        }
        crate::style::release(&mut entry, outbox);
        drop(entry);
    }
    if let Some(done) = done {
        outbox.resolve(done, Ok(()));
    }
}
