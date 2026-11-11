// Ports src/mln/text/glyph_manager.hpp, src/mln/text/glyph_manager.cpp and
// src/mln/text/glyph_manager_observer.hpp.
//
// In mln_renderer rather than mln_text: it requests through mln_storage
// (mln_text stays free of it).
//
// The manager lives on the render thread (orchestrator-owned, shared with
// the tiles as `Rc`); its state is in a `RefCell` instead of behind the C++
// recursive mutex, and requestors are notified after the borrow is
// released. The C++ shares one `std::shared_ptr<GlyphDependencies>` between
// the ranges a request waits for and notifies when it becomes unique; the
// port does the same with `Rc::strong_count`.
//
// Glyphs parsed from PBF ranges go through the `GlyphCache` the manager is
// given (thread-safe, so renderers on other threads can share it), and each
// range keeps its glyphs in one shared map; `Entry::glyphs` holds only the
// local and HarfBuzz glyphs.
//
// Font faces (MLN_TEXT_SHAPING_HARFBUZZ): the style's "font-faces" are
// fetched as font files (`Resource::fontFace`) and shaped with the
// `FontFaceEngine` (font_face_engine.rs): mln_text's HBShaper (HarfRust)
// natively, ready at once. An engine that needs time to get ready (the
// browser build loads its shaper as a second wasm module) is asked to as soon
// as the style has font faces; a font file that arrives before it is ready
// waits, parsed when the engine reports back on this thread's RunLoop, and
// the tiles that need it wait through their GlyphDependencies as for any
// range still in flight.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

use mln_actor::{AsyncTask, AsyncTaskSender};
use mln_storage::{AsyncRequest, FileSource, Resource, Response};
use mln_text::HBShapeAdjust;
use mln_text::glyph::{
    FontFaces, Glyph, GlyphDependencies, GlyphID, GlyphIDType, GlyphIDs, GlyphMap, GlyphRange, Glyphs, HBShapeRequests,
    get_glyph_range,
};
use mln_text::glyph_cache::{GlyphCache, GlyphCacheRange};
use mln_text::glyph_pbf::parse_glyph_pbf;
use mln_text::local_glyph_rasterizer::{DefaultLocalGlyphRasterizer, LocalGlyphRasterizer};
use mln_text::tagged_string::font_stack_hash;
use mln_util::exception::{ExceptionPtr, RuntimeError};
use mln_util::font_stack::FontStack;
use mln_util::immutable::Immutable;
use mln_util::logging::{Event, Log};
use mln_util::sync::MutexExt;
use mln_util::tiny_sdf::transform_raster_to_sdf;
use mln_vec_map::{VecMap, VecSet};
use serde::{Deserialize, Serialize};

use crate::text::font_face_engine::{self, FontFaceEngine, FontFaceShaper};

/// `HBShapeResult`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HBShapeResult {
    pub str_: Vec<u16>,
    pub adjusts: Arc<Vec<mln_text::HBShapeAdjust>>,
}

/// `HBShapeResults = std::map<FontStack, std::map<GlyphIDType,
/// std::map<std::u16string, HBShapeResult>>>`.
pub type HBShapeResults = VecMap<FontStack, VecMap<GlyphIDType, BTreeMap<Vec<u16>, HBShapeResult>>>;

/// `GlyphManagerObserver`.
pub trait GlyphManagerObserver {
    fn on_glyphs_loaded(&self, _font_stack: &FontStack, _range: &GlyphRange) {}
    fn on_glyphs_error(&self, _font_stack: &FontStack, _range: &GlyphRange, _error: ExceptionPtr) {}
    fn on_glyphs_requested(&self, _font_stack: &FontStack, _range: &GlyphRange) {}
}

struct NullObserver;
impl GlyphManagerObserver for NullObserver {}

/// `GlyphRequestor`.
pub trait GlyphRequestor {
    fn on_glyphs_available(&self, glyphs: GlyphMap, requests: HBShapeRequests);
}

/// A requestor's identity (`GlyphRequestor*`).
pub type GlyphRequestorKey = usize;

pub fn glyph_requestor_key<T: ?Sized>(requestor: &Rc<T>) -> GlyphRequestorKey {
    Rc::as_ptr(requestor) as *const () as usize
}

#[derive(Default)]
struct GlyphRequest {
    parsed: bool,
    req: Option<Box<dyn AsyncRequest>>,
    requestors: HashMap<GlyphRequestorKey, (Weak<dyn GlyphRequestor>, Rc<GlyphDependencies>)>,
    glyphs: Option<Arc<GlyphCacheRange>>,
}

#[derive(Default)]
struct Entry {
    ranges: VecMap<GlyphRange, GlyphRequest>,
    /// Local and HarfBuzz glyphs. PBF glyphs are in the ranges.
    glyphs: BTreeMap<GlyphID, Immutable<Glyph>>,
}

impl Entry {
    /// `findGlyph(entry, glyphID)`.
    fn find_glyph(&self, glyph_id: GlyphID) -> Option<Immutable<Glyph>> {
        if let Some(glyph) = self.glyphs.get(&glyph_id) {
            return Some(glyph.clone());
        }
        if glyph_id.type_ != GlyphIDType::FontPBF {
            return None;
        }
        self.ranges.get(&get_glyph_range(glyph_id))?.glyphs.as_ref()?.get(&glyph_id).cloned()
    }
}

type Notification = (Weak<dyn GlyphRequestor>, GlyphMap, HBShapeRequests);

/// What the font-face engine's `prepare` reported, and how to wake the
/// manager when it does; shared with its `ready` callback, which may run on
/// another thread.
#[derive(Default)]
struct EngineReadiness {
    result: Mutex<Option<Result<(), String>>>,
    waker: Mutex<Option<AsyncTaskSender>>,
}

impl EngineReadiness {
    fn is_reported(&self) -> bool {
        self.result.lock_unpoisoned().is_some()
    }

    fn is_ready(&self) -> bool {
        matches!(*self.result.lock_unpoisoned(), Some(Ok(())))
    }
}

/// `GlyphManager`.
pub struct GlyphManager {
    self_weak: Weak<GlyphManager>,
    glyph_url: RefCell<String>,
    entries: RefCell<HashMap<FontStack, Entry>>,
    observer: RefCell<Rc<dyn GlyphManagerObserver>>,
    local_glyph_rasterizer: Box<dyn LocalGlyphRasterizer>,
    glyph_cache: Arc<GlyphCache>,

    // Shaping objects
    font_faces: RefCell<Option<Arc<FontFaces>>>,
    engine: Arc<dyn FontFaceEngine>,
    /// Whether `engine.prepare` was called.
    engine_prepared: Cell<bool>,
    engine_readiness: Arc<EngineReadiness>,
    /// Font-face responses that arrived before the engine was ready.
    deferred_responses: RefCell<Vec<(Response, FontStack, GlyphRange)>>,
    /// Parses `deferred_responses` on this thread once the engine is ready.
    deferred_task: RefCell<Option<AsyncTask>>,
    hb_shapers: RefCell<VecMap<FontStack, VecMap<GlyphIDType, Rc<dyn FontFaceShaper>>>>,
}

impl GlyphManager {
    /// `GlyphManager(std::unique_ptr<LocalGlyphRasterizer>)`: a manager
    /// with a glyph cache of its own.
    pub fn new(local_glyph_rasterizer: Option<Box<dyn LocalGlyphRasterizer>>) -> Rc<GlyphManager> {
        Self::with_glyph_cache(local_glyph_rasterizer, Arc::default())
    }

    /// `GlyphManager(std::unique_ptr<LocalGlyphRasterizer>,
    /// std::shared_ptr<GlyphCache>)`: managers given the same cache share
    /// the glyphs they parse from glyph PBFs.
    pub fn with_glyph_cache(
        local_glyph_rasterizer: Option<Box<dyn LocalGlyphRasterizer>>,
        glyph_cache: Arc<GlyphCache>,
    ) -> Rc<GlyphManager> {
        Self::with_cache_and_engine(local_glyph_rasterizer, glyph_cache, font_face_engine::font_face_engine())
    }

    /// A manager whose font faces load with `engine` instead of the one
    /// `set_font_face_engine` set.
    pub fn with_font_face_engine(
        local_glyph_rasterizer: Option<Box<dyn LocalGlyphRasterizer>>,
        engine: Arc<dyn FontFaceEngine>,
    ) -> Rc<GlyphManager> {
        Self::with_cache_and_engine(local_glyph_rasterizer, Arc::default(), engine)
    }

    fn with_cache_and_engine(
        local_glyph_rasterizer: Option<Box<dyn LocalGlyphRasterizer>>,
        glyph_cache: Arc<GlyphCache>,
        engine: Arc<dyn FontFaceEngine>,
    ) -> Rc<GlyphManager> {
        let local_glyph_rasterizer =
            local_glyph_rasterizer.unwrap_or_else(|| Box::new(DefaultLocalGlyphRasterizer::new(None)));
        Rc::new_cyclic(|self_weak| GlyphManager {
            self_weak: self_weak.clone(),
            glyph_url: RefCell::new(String::new()),
            entries: RefCell::new(HashMap::new()),
            observer: RefCell::new(Rc::new(NullObserver)),
            local_glyph_rasterizer,
            glyph_cache,
            font_faces: RefCell::new(None),
            engine,
            engine_prepared: Cell::new(false),
            engine_readiness: Arc::default(),
            deferred_responses: RefCell::new(Vec::new()),
            deferred_task: RefCell::new(None),
            hb_shapers: RefCell::new(VecMap::new()),
        })
    }

    /// `setURL(url)`.
    pub fn set_url(&self, url: &str) {
        if *self.glyph_url.borrow() != url {
            *self.glyph_url.borrow_mut() = url.to_owned();
        }
    }

    pub fn set_observer(&self, observer: Option<Rc<dyn GlyphManagerObserver>>) {
        *self.observer.borrow_mut() = observer.unwrap_or_else(|| Rc::new(NullObserver));
    }

    /// `getGlyphs(GlyphRequestor&, GlyphDependencies, FileSource&)`.
    ///
    /// Workers send a `getGlyphs` message to the main thread once they have
    /// determined their `GlyphDependencies`. If all glyphs are already
    /// locally available, GlyphManager will provide them to the requestor
    /// immediately. Otherwise, it makes a request on the FileSource is made
    /// for each range needed, and notifies the observer when all are
    /// complete.
    pub fn get_glyphs(
        &self,
        requestor: &Rc<dyn GlyphRequestor>,
        glyph_dependencies: GlyphDependencies,
        file_source: Option<&Arc<dyn FileSource>>,
    ) {
        let key = glyph_requestor_key(requestor);
        let weak = Rc::downgrade(requestor);
        let dependencies = Rc::new(glyph_dependencies);
        let mut to_request: Vec<(FontStack, GlyphRange)> = Vec::new();
        {
            let mut entries = self.entries.borrow_mut();
            // Figure out which glyph ranges need to be fetched. For each range
            // that does need to be fetched, record an entry mapping the
            // requestor to a shared pointer containing the dependencies. When
            // the shared pointer becomes unique, we know that all the
            // dependencies for that requestor have been fetched, and can
            // notify it of completion.
            for (font_stack, glyph_ids) in &dependencies.glyphs {
                let entry = entries.entry(font_stack.clone()).or_default();

                let mut ranges: HashSet<GlyphRange> = HashSet::new();
                for glyph_id in glyph_ids {
                    if self.local_glyph_rasterizer.can_rasterize_glyph(font_stack, *glyph_id) {
                        if !entry.glyphs.contains_key(glyph_id) {
                            let glyph = self.generate_local_sdf(font_stack, *glyph_id);
                            entry.glyphs.insert(*glyph_id, Arc::new(glyph));
                        }
                    } else {
                        ranges.insert(get_glyph_range(*glyph_id));
                    }
                }

                for range in ranges {
                    let needs_request = entry.ranges.get(&range).is_none_or(|request| !request.parsed);
                    if needs_request {
                        let request = entry.ranges.entry(range).or_default();
                        request.requestors.insert(key, (weak.clone(), dependencies.clone()));
                        if request.req.is_none() {
                            to_request.push((font_stack.clone(), range));
                        }
                    }
                }
            }
        }

        for (font_stack, range) in to_request {
            self.request_range(&font_stack, &range, file_source);
        }

        // If the shared dependencies pointer is already unique, then all
        // dependent glyph ranges have already been loaded. Send a
        // notification immediately.
        if Rc::strong_count(&dependencies) == 1 {
            let notification = self.notification(&weak, &dependencies);
            Self::deliver(vec![notification]);
        }
    }

    /// `generateLocalSDF(fontStack, glyphID)`.
    fn generate_local_sdf(&self, font_stack: &FontStack, glyph_id: GlyphID) -> Glyph {
        let mut local = self.local_glyph_rasterizer.rasterize_glyph(font_stack, glyph_id);
        let radius = if local.metrics.is_double_resolution { 16.0 } else { 8.0 };
        local.bitmap = transform_raster_to_sdf(&local.bitmap, radius, 0.25);
        local
    }

    /// `requestRange(request, fontStack, range, fileSource)`.
    fn request_range(&self, font_stack: &FontStack, range: &GlyphRange, file_source: Option<&Arc<dyn FileSource>>) {
        let resource = if range.type_ == GlyphIDType::FontPBF {
            Resource::glyphs(&self.glyph_url.borrow(), font_stack, (range.first, range.second))
        } else {
            let url = self.get_font_face_url(range.type_);
            if url.is_empty() {
                // C++ logs and requests an empty Resource.
                mln_util::logging::Log::error(
                    mln_util::logging::Event::Style,
                    "Try download a glyph doesn't in current faces",
                );
            }
            Resource::font_face(&url)
        };

        let observer = self.observer.borrow().clone();
        observer.on_glyphs_requested(font_stack, range);

        let Some(file_source) = file_source else {
            // C++ always has a file source; without one the request never
            // completes.
            return;
        };

        let weak_self = self.self_weak.clone();
        let callback_font_stack = font_stack.clone();
        let callback_range = *range;
        let req = file_source.request(
            &resource,
            Box::new(move |response: Response| {
                if let Some(this) = weak_self.upgrade() {
                    this.process_response(&response, &callback_font_stack, &callback_range);
                }
            }),
        );

        let mut entries = self.entries.borrow_mut();
        if let Some(request) = entries.get_mut(font_stack).and_then(|entry| entry.ranges.get_mut(range)) {
            if !request.parsed && request.req.is_none() {
                request.req = Some(req);
            }
        }
    }

    /// `processResponse(response, fontStack, range)`.
    fn process_response(&self, res: &Response, font_stack: &FontStack, range: &GlyphRange) {
        let observer = self.observer.borrow().clone();
        if let Some(error) = &res.error {
            observer.on_glyphs_error(font_stack, range, RuntimeError::ptr(error.message.clone()));
            return;
        }

        if res.not_modified {
            return;
        }

        if range.type_ != GlyphIDType::FontPBF
            && !res.no_content
            && self.defer_until_engine_ready(res, font_stack, range)
        {
            return;
        }

        let mut notifications = Vec::new();
        {
            let mut entries = self.entries.borrow_mut();
            let entry = entries.entry(font_stack.clone()).or_default();

            let mut range_glyphs = None;
            if !res.no_content {
                let data: &[u8] = res.data.as_deref().map(|d| d.as_slice()).unwrap_or(&[]);
                if range.type_ == GlyphIDType::FontPBF {
                    let glyphs = match parse_glyph_pbf(range, data) {
                        Ok(glyphs) => glyphs,
                        Err(error) => {
                            drop(entries);
                            observer.on_glyphs_error(font_stack, range, RuntimeError::ptr(error.to_string()));
                            return;
                        }
                    };
                    let mut interned = GlyphCacheRange::new();
                    for glyph in glyphs {
                        let id = glyph.id;
                        if !self.local_glyph_rasterizer.can_rasterize_glyph(font_stack, id) {
                            interned.insert(id, self.glyph_cache.intern_glyph(glyph));
                        }
                    }
                    range_glyphs = Some(self.glyph_cache.intern_range(interned));
                } else if self.load_hb_shaper(font_stack, range.type_, data) {
                    let id = GlyphID::with_type(0, range.type_);
                    if !self.local_glyph_rasterizer.can_rasterize_glyph(font_stack, id) {
                        entry.glyphs.insert(id, Arc::new(Glyph { id, ..Glyph::default() }));
                    }
                }
            }

            let request = entry.ranges.entry(*range).or_default();
            request.parsed = true;
            if range_glyphs.is_some() {
                request.glyphs = range_glyphs;
            }
            let requestors = std::mem::take(&mut request.requestors);
            // Release this range's references first, so a requestor's
            // dependencies are unique once all its ranges are parsed.
            let mut pending: Vec<(Weak<dyn GlyphRequestor>, Rc<GlyphDependencies>)> =
                requestors.into_values().collect();
            let mut i = 0;
            while i < pending.len() {
                if Rc::strong_count(&pending[i].1) == 1 {
                    let (requestor, dependencies) = pending.swap_remove(i);
                    notifications.push(Self::notification_for(&mut entries, &requestor, &dependencies));
                } else {
                    i += 1;
                }
            }
        }
        Self::deliver(notifications);

        observer.on_glyphs_loaded(font_stack, range);
    }

    fn notification(&self, requestor: &Weak<dyn GlyphRequestor>, dependencies: &GlyphDependencies) -> Notification {
        let mut entries = self.entries.borrow_mut();
        Self::notification_for(&mut entries, requestor, dependencies)
    }

    /// `notify(GlyphRequestor&, const GlyphDependencies&)`: the glyph map
    /// for the requestor, delivered by `deliver`.
    fn notification_for(
        entries: &mut HashMap<FontStack, Entry>,
        requestor: &Weak<dyn GlyphRequestor>,
        glyph_dependencies: &GlyphDependencies,
    ) -> Notification {
        let mut response = GlyphMap::new();

        for (font_stack, glyph_ids) in &glyph_dependencies.glyphs {
            let glyphs: &mut Glyphs = response.entry(font_stack_hash(font_stack)).or_default();
            let entry = entries.entry(font_stack.clone()).or_default();

            for glyph_id in glyph_ids {
                glyphs.entry(*glyph_id).or_insert_with(|| entry.find_glyph(*glyph_id));
            }
        }

        (requestor.clone(), response, glyph_dependencies.shapes.clone())
    }

    fn deliver(notifications: Vec<Notification>) {
        for (requestor, glyphs, shapes) in notifications {
            if let Some(requestor) = requestor.upgrade() {
                requestor.on_glyphs_available(glyphs, shapes);
            }
        }
    }

    /// `removeRequestor(GlyphRequestor&)`, by the requestor's identity.
    pub fn remove_requestor_key(&self, key: GlyphRequestorKey) {
        let mut entries = self.entries.borrow_mut();
        for entry in entries.values_mut() {
            for range in entry.ranges.values_mut() {
                range.requestors.remove(&key);
            }
        }
    }

    /// `evict(keep)`: remove glyphs for all but the supplied font stacks.
    pub fn evict(&self, keep: &VecSet<FontStack>) {
        self.entries.borrow_mut().retain(|font_stack, _| keep.contains(font_stack));
    }

    /// `getGlyph(fontStack, glyphID)`.
    pub fn get_glyph(&self, font_stack: &FontStack, glyph_id: GlyphID) -> Immutable<Glyph> {
        let mut entries = self.entries.borrow_mut();
        let entry = entries.entry(font_stack.clone()).or_default();
        if let Some(glyph) = entry.find_glyph(glyph_id) {
            return glyph;
        }

        if glyph_id.type_ != GlyphIDType::FontPBF {
            if let Some(shaper) = self.get_hb_shaper(font_stack, glyph_id.type_) {
                let mut glyph = shaper.rasterize(&glyph_id);
                glyph.bitmap = transform_raster_to_sdf(&glyph.bitmap, 8.0, 0.25);
                let glyph = Arc::new(glyph);
                entry.glyphs.insert(glyph_id, glyph.clone());
                return glyph;
            }
        }

        Arc::new(Glyph::default())
    }

    /// `setFontFaces(faces)`. Once a style has font faces, the engine gets
    /// ready to load them while their files download.
    pub fn set_font_faces(&self, faces: Option<Arc<FontFaces>>) {
        let has_faces = faces.as_ref().is_some_and(|faces| !faces.is_empty());
        *self.font_faces.borrow_mut() = faces;
        if has_faces {
            self.prepare_engine();
        }
    }

    /// Asks the engine to get ready, once.
    fn prepare_engine(&self) {
        if self.engine_prepared.replace(true) {
            return;
        }
        let readiness = self.engine_readiness.clone();
        self.engine.prepare(Box::new(move |result| {
            if let Err(error) = &result {
                Log::error(Event::Glyph, format!("font faces can't be loaded: {error}"));
            }
            *readiness.result.lock_unpoisoned() = Some(result);
            let waker = readiness.waker.lock_unpoisoned().take();
            if let Some(waker) = waker {
                waker.send();
            }
        }));
    }

    /// Whether `res`, a font file, has to wait for the engine: if so, keeps
    /// it for `process_deferred_responses`, which runs once the engine
    /// reports back.
    fn defer_until_engine_ready(&self, res: &Response, font_stack: &FontStack, range: &GlyphRange) -> bool {
        self.prepare_engine();
        if self.engine_readiness.is_reported() {
            return false;
        }
        self.deferred_responses.borrow_mut().push((res.clone(), font_stack.clone(), *range));
        let mut task = self.deferred_task.borrow_mut();
        let task = task.get_or_insert_with(|| {
            let weak_self = self.self_weak.clone();
            AsyncTask::new(move || {
                if let Some(this) = weak_self.upgrade() {
                    this.process_deferred_responses();
                }
            })
        });
        *self.engine_readiness.waker.lock_unpoisoned() = Some(task.sender());
        // The engine may have reported back from another thread before the
        // waker was set.
        if self.engine_readiness.is_reported() {
            task.send();
        }
        true
    }

    /// Parses the font files that waited for the engine.
    fn process_deferred_responses(&self) {
        let responses = std::mem::take(&mut *self.deferred_responses.borrow_mut());
        for (res, font_stack, range) in responses {
            self.process_response(&res, &font_stack, &range);
        }
    }

    /// `getFontFaces()`.
    pub fn get_font_faces(&self) -> Option<Arc<FontFaces>> {
        self.font_faces.borrow().clone()
    }

    /// `getHBShaper(fontStack, type)`.
    pub fn get_hb_shaper(&self, font_stack: &FontStack, type_: GlyphIDType) -> Option<Rc<dyn FontFaceShaper>> {
        self.hb_shapers.borrow().get(font_stack).and_then(|shapers| shapers.get(&type_)).cloned()
    }

    /// `loadHBShaper(fontStack, type, data)`: false, as for an invalid
    /// font, when the engine failed to get ready.
    fn load_hb_shaper(&self, font_stack: &FontStack, type_: GlyphIDType, data: &[u8]) -> bool {
        if !self.engine_readiness.is_ready() {
            return false;
        }
        let Some(shaper) = self.engine.load(type_, data) else {
            return false;
        };
        self.hb_shapers.borrow_mut().entry(font_stack.clone()).or_default().insert(type_, Rc::from(shaper));
        true
    }

    /// `hbShaping(text, rtl, font, type, glyphIDs, adjusts)`.
    pub fn hb_shaping(
        &self,
        text: &[u16],
        rtl: bool,
        font: &FontStack,
        type_: GlyphIDType,
        glyph_ids: &mut Vec<GlyphID>,
        adjusts: &mut Vec<HBShapeAdjust>,
    ) {
        if let Some(shaper) = self.get_hb_shaper(font, type_) {
            shaper.shape(text, rtl, glyph_ids, adjusts);
        }
    }

    /// `getFontFaceURL(type)`.
    pub fn get_font_face_url(&self, type_: GlyphIDType) -> String {
        if let Some(faces) = &*self.font_faces.borrow() {
            for face in faces.iter() {
                if face.type_ == type_ {
                    return face.url.clone();
                }
            }
        }
        String::new()
    }

    /// The glyph ids a requestor asked for (helper for tests).
    pub fn has_glyph(&self, font_stack: &FontStack, glyph_id: GlyphID) -> bool {
        self.entries.borrow().get(font_stack).is_some_and(|entry| entry.find_glyph(glyph_id).is_some())
    }
}

/// `GlyphIDs` of a dependency set (re-export for the worker).
pub type GlyphIDSet = GlyphIDs;
