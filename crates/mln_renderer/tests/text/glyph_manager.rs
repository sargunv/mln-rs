// Ports test/text/glyph_manager.test.cpp.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mln_actor::RunLoop;
use mln_renderer::text::font_face_engine::{FontFaceEngine, FontFaceShaper};
use mln_renderer::text::glyph_manager::{GlyphManager, GlyphManagerObserver, GlyphRequestor};
use mln_storage::response::{Error, Reason};
use mln_storage::stub_file_source::{ResponseType, StubFileSource};
use mln_storage::{FileSource, Kind, Resource, Response};
use mln_text::HBShapeAdjust;
use mln_text::glyph::{
    FontFace, GLYPH_RANGES_PER_FONT_STACK, GLYPHS_PER_GLYPH_RANGE, Glyph, GlyphDependencies, GlyphID, GlyphIDType,
    GlyphIDs, GlyphMap, GlyphMetrics, GlyphRange, HBShapeRequests, NON_IDEOGRAPH_GLYPH_RANGES_PER_FONT_STACK,
};
use mln_text::glyph_cache::GlyphCache;
use mln_text::local_glyph_rasterizer::LocalGlyphRasterizer;
use mln_text::tagged_string::font_stack_hash;
use mln_util::exception::ExceptionPtr;
use mln_util::font_stack::FontStack;
use mln_util::i18n;
use mln_util::image::AlphaImage;
use mln_util::size::Size;
use mln_vec_map::{VecMap, VecSet};

use super::glyph_manager_bitmaps::{SDF_BITMAP, STUB_BITMAP};
use super::{SquelchLog, read_file};

fn stub_glyph(glyph_id: GlyphID) -> Glyph {
    Glyph {
        id: glyph_id,
        metrics: GlyphMetrics { width: 24, height: 24, left: 0.0, top: -8.0, advance: 24, ..Default::default() },
        bitmap: AlphaImage::from_slice(Size::new(30, 30), &STUB_BITMAP).unwrap(),
    }
}

/// `StubLocalGlyphRasterizer`.
struct StubLocalGlyphRasterizer;

impl LocalGlyphRasterizer for StubLocalGlyphRasterizer {
    fn can_rasterize_glyph(&self, _font_stack: &FontStack, glyph_id: GlyphID) -> bool {
        i18n::allows_fixed_width_glyph_generation(glyph_id.code)
    }

    fn rasterize_glyph(&self, _font_stack: &FontStack, glyph_id: GlyphID) -> Glyph {
        stub_glyph(glyph_id)
    }
}

/// `KatakanaOnlyLocalGlyphRasterizer`.
struct KatakanaOnlyLocalGlyphRasterizer;

impl LocalGlyphRasterizer for KatakanaOnlyLocalGlyphRasterizer {
    fn can_rasterize_glyph(&self, _font_stack: &FontStack, glyph_id: GlyphID) -> bool {
        glyph_id.code == 'テ' as u16
    }

    fn rasterize_glyph(&self, _font_stack: &FontStack, glyph_id: GlyphID) -> Glyph {
        stub_glyph(glyph_id)
    }
}

/// `LatinALocalGlyphRasterizer`.
struct LatinALocalGlyphRasterizer;

impl LocalGlyphRasterizer for LatinALocalGlyphRasterizer {
    fn can_rasterize_glyph(&self, _font_stack: &FontStack, glyph_id: GlyphID) -> bool {
        glyph_id == gid('a')
    }

    fn rasterize_glyph(&self, _font_stack: &FontStack, glyph_id: GlyphID) -> Glyph {
        stub_glyph(glyph_id)
    }
}

type LoadedFn = Box<dyn FnMut(&FontStack, &GlyphRange)>;
type ErrorFn = Box<dyn FnMut(&FontStack, &GlyphRange, ExceptionPtr)>;

/// `StubGlyphManagerObserver`.
#[derive(Default)]
struct StubGlyphManagerObserver {
    glyphs_loaded: RefCell<Option<LoadedFn>>,
    glyphs_error: RefCell<Option<ErrorFn>>,
}

impl GlyphManagerObserver for StubGlyphManagerObserver {
    fn on_glyphs_loaded(&self, font_stack: &FontStack, range: &GlyphRange) {
        if let Some(f) = self.glyphs_loaded.borrow_mut().as_mut() {
            f(font_stack, range);
        }
    }

    fn on_glyphs_error(&self, font_stack: &FontStack, range: &GlyphRange, error: ExceptionPtr) {
        if let Some(f) = self.glyphs_error.borrow_mut().as_mut() {
            f(font_stack, range, error);
        }
    }
}

/// `StubGlyphRequestor`. The callback is an `Rc<dyn Fn>` because C++'s
/// LocalCJKGlyphPrefersLocalRasterizationOverRange calls `getGlyphs` from
/// inside it, which notifies the requestor again.
#[derive(Default)]
struct StubGlyphRequestor {
    glyphs_available: RefCell<Option<Rc<dyn Fn(GlyphMap)>>>,
}

impl GlyphRequestor for StubGlyphRequestor {
    fn on_glyphs_available(&self, glyphs: GlyphMap, _requests: HBShapeRequests) {
        let f = self.glyphs_available.borrow().clone();
        if let Some(f) = f {
            f(glyphs);
        }
    }
}

struct GlyphManagerTest {
    run_loop: RunLoop,
    file_source: Arc<StubFileSource>,
    observer: Rc<StubGlyphManagerObserver>,
    requestor: Rc<StubGlyphRequestor>,
    glyph_manager: Rc<GlyphManager>,
    _log: Option<SquelchLog>,
}

impl GlyphManagerTest {
    fn new() -> Self {
        Self::with(Box::new(StubLocalGlyphRasterizer), ResponseType::Asynchronous)
    }

    fn with(local_glyph_rasterizer: Box<dyn LocalGlyphRasterizer>, response_type: ResponseType) -> Self {
        let run_loop = RunLoop::new();
        GlyphManagerTest {
            run_loop,
            file_source: Arc::new(StubFileSource::new(response_type)),
            observer: Rc::new(StubGlyphManagerObserver::default()),
            requestor: Rc::new(StubGlyphRequestor::default()),
            glyph_manager: GlyphManager::new(Some(local_glyph_rasterizer)),
            _log: None,
        }
    }

    /// `GlyphManagerTest(localGlyphRasterizer, glyphCache)`.
    fn with_glyph_cache(local_glyph_rasterizer: Box<dyn LocalGlyphRasterizer>, glyph_cache: Arc<GlyphCache>) -> Self {
        let mut test = Self::new();
        test.glyph_manager = GlyphManager::with_glyph_cache(Some(local_glyph_rasterizer), glyph_cache);
        test
    }

    fn file_source(&self) -> Arc<dyn FileSource> {
        self.file_source.clone()
    }

    fn requestor(&self) -> Rc<dyn GlyphRequestor> {
        self.requestor.clone()
    }

    fn run(&mut self, url: &str, dependencies: GlyphDependencies) {
        // Squelch logging.
        self._log = Some(SquelchLog::new());

        self.glyph_manager.set_url(url);
        self.glyph_manager.set_observer(Some(self.observer.clone()));
        self.glyph_manager.get_glyphs(&self.requestor(), dependencies, Some(&self.file_source()));

        self.run_loop.run();
    }

    fn end(&self) {
        self.run_loop.stop();
    }

    /// Loads `chars` of `font_stack` from the glyphs.pbf fixture.
    fn load(&mut self, font_stack: FontStack, chars: &[char]) {
        self.file_source.set_response(Kind::Glyphs, glyphs_response("test/fixtures/resources/glyphs.pbf"));
        let run_loop = self.run_loop.clone();
        *self.requestor.glyphs_available.borrow_mut() = Some(Rc::new(move |_: GlyphMap| run_loop.stop()));
        let ids: GlyphIDs = chars.iter().map(|c| gid(*c)).collect();
        let dependencies = GlyphDependencies { glyphs: VecMap::from([(font_stack, ids)]), shapes: Default::default() };
        self.run("test/fixtures/resources/glyphs.pbf", dependencies);
    }
}

fn test_stack() -> FontStack {
    vec!["Test Stack".to_owned()]
}

/// `GlyphDependencies{.glyphs = {{{{"Test Stack"}}, {chars...}}}, .shapes = {}}`.
fn dependencies(chars: &[char]) -> GlyphDependencies {
    let ids: GlyphIDs = chars.iter().map(|c| GlyphID::new(*c as u32)).collect();
    GlyphDependencies { glyphs: VecMap::from([(test_stack(), ids)]), shapes: Default::default() }
}

fn gid(c: char) -> GlyphID {
    GlyphID::new(c as u32)
}

fn glyphs_response(path: &'static str) -> impl Fn(&Resource) -> Option<Response> {
    move |_resource: &Resource| {
        let mut response = Response::new();
        response.data = Some(Arc::new(read_file(path)));
        Some(response)
    }
}

// gtest: GlyphManager.LoadingSuccess
#[test]
fn loading_success() {
    let mut test = GlyphManagerTest::new();

    test.file_source.set_response(Kind::Glyphs, |resource: &Resource| {
        assert_eq!(Kind::Glyphs, resource.kind);
        let mut response = Response::new();
        response.data = Some(Arc::new(read_file("test/fixtures/resources/glyphs.pbf")));
        Some(response)
    });

    *test.observer.glyphs_error.borrow_mut() = Some(Box::new(|_, _, _| panic!("glyphsError")));

    *test.observer.glyphs_loaded.borrow_mut() = Some(Box::new(|font_stack, range| {
        assert_eq!(*font_stack, test_stack());
        assert_eq!(*range, GlyphRange::new(0, 255, GlyphIDType::FontPBF));
    }));

    let available = Rc::new(Cell::new(false));
    let (a, run_loop) = (available.clone(), test.run_loop.clone());
    *test.requestor.glyphs_available.borrow_mut() = Some(Rc::new(move |glyphs: GlyphMap| {
        let test_positions = &glyphs[&font_stack_hash(&test_stack())];

        assert_eq!(test_positions.len(), 3);
        assert!(test_positions.contains_key(&gid('a')));
        assert!(test_positions.contains_key(&gid('å')));
        assert!(test_positions.contains_key(&gid(' ')));
        assert!(test_positions[&gid(' ')].is_some());

        a.set(true);
        run_loop.stop();
    }));

    test.run("test/fixtures/resources/glyphs.pbf", dependencies(&['a', 'å', ' ']));
    assert!(available.get());
}

/// The observer's `glyphsError` for LoadingFail / LoadingCorrupted.
fn expect_error(test: &GlyphManagerTest, message: &'static str) -> Rc<Cell<bool>> {
    let called = Rc::new(Cell::new(false));
    let (c, run_loop) = (called.clone(), test.run_loop.clone());
    *test.observer.glyphs_error.borrow_mut() = Some(Box::new(move |font_stack, glyph_range, error| {
        assert_eq!(*font_stack, test_stack());
        assert_eq!(*glyph_range, GlyphRange::new(0, 255, GlyphIDType::FontPBF));

        assert_eq!(error.to_string(), message);

        c.set(true);
        run_loop.stop();
    }));

    *test.requestor.glyphs_available.borrow_mut() = Some(Rc::new(|_| panic!("glyphsAvailable")));
    called
}

// gtest: GlyphManager.LoadingFail
#[test]
fn loading_fail() {
    let mut test = GlyphManagerTest::new();

    test.file_source.set_response(Kind::Glyphs, |_: &Resource| {
        let mut response = Response::new();
        response.error = Some(Box::new(Error::new(Reason::Other, "Failed by the test case")));
        Some(response)
    });

    let called = expect_error(&test, "Failed by the test case");

    test.run("test/fixtures/resources/glyphs.pbf", dependencies(&['a', 'å']));
    assert!(called.get());
}

// gtest: GlyphManager.LoadingCorrupted
#[test]
fn loading_corrupted() {
    let mut test = GlyphManagerTest::new();

    test.file_source.set_response(Kind::Glyphs, |_: &Resource| {
        let mut response = Response::new();
        response.data = Some(Arc::new(b"CORRUPTED".to_vec()));
        Some(response)
    });

    let called = expect_error(&test, "unknown pbf field type exception");

    test.run("test/fixtures/resources/glyphs.pbf", dependencies(&['a', 'å']));
    assert!(called.get());
}

// gtest: GlyphManager.SharesParsedGlyphsBetweenManagersAndFontStacks
#[test]
fn shares_parsed_glyphs_between_managers_and_font_stacks() {
    let cache = Arc::new(GlyphCache::new());
    let load = |font_stack: &str| {
        let mut test = GlyphManagerTest::with_glyph_cache(Box::new(StubLocalGlyphRasterizer), cache.clone());
        let font_stack = vec![font_stack.to_owned()];
        test.load(font_stack.clone(), &['a']);
        test.glyph_manager.get_glyph(&font_stack, gid('a'))
    };

    let first = load("Test Stack");

    assert!(Arc::ptr_eq(&first, &load("Other Stack")));
}

// gtest: GlyphManager.KeepsItsOwnGlyphsWithoutACache
#[test]
fn keeps_its_own_glyphs_without_a_cache() {
    let load = || {
        let mut test = GlyphManagerTest::new();
        test.load(test_stack(), &['a']);
        test.glyph_manager.get_glyph(&test_stack(), gid('a'))
    };

    let first = load();

    assert!(!Arc::ptr_eq(&first, &load()));
}

// gtest: GlyphManager.ReleasesTheGlyphsOfEvictedFontStacks
#[test]
fn releases_the_glyphs_of_evicted_font_stacks() {
    let cache = Arc::new(GlyphCache::new());
    let mut test = GlyphManagerTest::with_glyph_cache(Box::new(StubLocalGlyphRasterizer), cache.clone());
    test.load(test_stack(), &['a']);
    assert_ne!(0, cache.size());

    test.glyph_manager.evict(&VecSet::new());

    assert_eq!(0, cache.size());
}

// gtest: GlyphManager.DoesNotKeepTheRangeGlyphsItDrawsLocally
#[test]
fn does_not_keep_the_range_glyphs_it_draws_locally() {
    let loaded_glyphs = |rasterizer: Box<dyn LocalGlyphRasterizer>| {
        let cache = Arc::new(GlyphCache::new());
        let mut test = GlyphManagerTest::with_glyph_cache(rasterizer, cache.clone());
        test.load(test_stack(), &['a', 'å']);
        cache.size()
    };

    let all = loaded_glyphs(Box::new(StubLocalGlyphRasterizer));

    // The fixture has 'a', the only glyph LatinALocalGlyphRasterizer draws.
    assert_eq!(all - 1, loaded_glyphs(Box::new(LatinALocalGlyphRasterizer)));
}

// gtest: GlyphManager.LoadingCancel
#[test]
fn loading_cancel() {
    let mut test = GlyphManagerTest::new();

    let run_loop = test.run_loop.clone();
    test.file_source.set_response(Kind::Glyphs, move |_: &Resource| {
        run_loop.stop();
        None
    });

    *test.observer.glyphs_loaded.borrow_mut() = Some(Box::new(|_, _| panic!("Should never be called")));

    test.run("test/fixtures/resources/glyphs.pbf", dependencies(&['a', 'å']));
}

// gtest: GlyphManager.LoadLocalCJKGlyph
#[test]
fn load_local_cjk_glyph() {
    let mut test = GlyphManagerTest::new();
    let glyph_responses = Rc::new(Cell::new(0));

    let g = glyph_responses.clone();
    test.file_source.set_response(Kind::Glyphs, move |_: &Resource| {
        g.set(g.get() + 1);
        None
    });

    let g = glyph_responses.clone();
    *test.observer.glyphs_loaded.borrow_mut() = Some(Box::new(move |_, _| g.set(g.get() + 1)));

    let available = Rc::new(Cell::new(false));
    let (a, g, run_loop) = (available.clone(), glyph_responses.clone(), test.run_loop.clone());
    *test.requestor.glyphs_available.borrow_mut() = Some(Rc::new(move |glyphs: GlyphMap| {
        assert_eq!(g.get(), 0); // Local generation should prevent requesting any glyphs

        let test_positions = &glyphs[&font_stack_hash(&test_stack())];

        assert_eq!(test_positions.len(), 1);
        assert!(test_positions.contains_key(&gid('中')));

        let glyph = test_positions[&gid('中')].clone().expect("glyph");
        assert_eq!(glyph.id.code, '中' as u16);
        assert_eq!(glyph.metrics.width, 24);
        assert_eq!(glyph.metrics.height, 24);
        assert_eq!(glyph.metrics.left, 0.0);
        assert_eq!(glyph.metrics.top, -8.0);
        assert_eq!(glyph.metrics.advance, 24);
        assert_eq!(glyph.bitmap.size, Size::new(30, 30));

        let pixel_count = (glyph.bitmap.size.width * glyph.bitmap.size.height) as usize;
        for i in 0..pixel_count {
            assert_eq!(glyph.bitmap.data[i], SDF_BITMAP[i], "pixel {i}");
        }

        a.set(true);
        run_loop.stop();
    }));

    test.run("test/fixtures/resources/glyphs.pbf", dependencies(&['中']));
    assert!(available.get());
}

// gtest: GlyphManager.LoadLocalCJKSymbols
#[test]
fn load_local_cjk_symbols() {
    let mut test = GlyphManagerTest::new();
    let glyph_responses = Rc::new(Cell::new(0));

    let g = glyph_responses.clone();
    test.file_source.set_response(Kind::Glyphs, move |_: &Resource| {
        g.set(g.get() + 1);
        None
    });

    let g = glyph_responses.clone();
    *test.observer.glyphs_loaded.borrow_mut() = Some(Box::new(move |_, _| g.set(g.get() + 1)));

    let available = Rc::new(Cell::new(false));
    let (a, g, run_loop) = (available.clone(), glyph_responses.clone(), test.run_loop.clone());
    *test.requestor.glyphs_available.borrow_mut() = Some(Rc::new(move |glyphs: GlyphMap| {
        assert_eq!(g.get(), 0); // Local generation should prevent requesting any glyphs.

        let test_positions = &glyphs[&font_stack_hash(&test_stack())];

        assert_eq!(test_positions.len(), 2);
        assert!(test_positions.contains_key(&gid('〒')));
        assert!(test_positions.contains_key(&gid('！')));

        a.set(true);
        run_loop.stop();
    }));

    test.run("test/fixtures/resources/glyphs.pbf", dependencies(&['〒', '！']));
    assert!(available.get());
}

// gtest: GlyphManager.LocalCJKGlyphPrefersLocalRasterizationOverRange
#[test]
fn local_cjk_glyph_prefers_local_rasterization_over_range() {
    // Verifies that locally-rasterized glyphs (like テ) are produced by the
    // local rasterizer instead of using a glyph from an already-loaded PBF
    // range.
    let mut test = GlyphManagerTest::with(Box::new(KatakanaOnlyLocalGlyphRasterizer), ResponseType::Asynchronous);
    let first_glyph_response = Rc::new(Cell::new(false));

    let f = first_glyph_response.clone();
    test.file_source.set_response(Kind::Glyphs, move |_: &Resource| {
        f.set(true);
        let mut response = Response::new();
        response.data = Some(Arc::new(read_file("test/fixtures/resources/glyphs-12244-12543.pbf")));
        Some(response)
    });

    let done = Rc::new(Cell::new(false));
    let (f, d, run_loop) = (first_glyph_response.clone(), done.clone(), test.run_loop.clone());
    let glyph_manager = Rc::downgrade(&test.glyph_manager);
    let requestor = Rc::downgrade(&test.requestor);
    let file_source = test.file_source();
    *test.requestor.glyphs_available.borrow_mut() = Some(Rc::new(move |glyphs: GlyphMap| {
        let test_positions = &glyphs[&font_stack_hash(&test_stack())];

        if f.get() {
            f.set(false);
            assert_eq!(test_positions.len(), 1);
            assert!(test_positions.contains_key(&gid('々')));

            // Katakana letter te, should be locally rasterized
            //  instead of using the glyph received from the range
            //  for the ideagraphic mark
            let requestor: Rc<dyn GlyphRequestor> = requestor.upgrade().unwrap();
            glyph_manager.upgrade().unwrap().get_glyphs(
                &requestor,
                dependencies(&['テ']), // 0x30c6
                Some(&file_source),
            );
        } else {
            assert_eq!(test_positions.len(), 1);
            assert!(test_positions.contains_key(&gid('テ')));

            let glyph = test_positions[&gid('テ')].clone().expect("glyph");
            assert_eq!(glyph.id.code, 'テ' as u16);
            assert_eq!(glyph.metrics.width, 24);
            assert_eq!(glyph.metrics.height, 24);
            assert_eq!(glyph.metrics.left, 0.0);
            assert_eq!(glyph.metrics.top, -8.0);
            assert_eq!(glyph.metrics.advance, 24);
            assert_eq!(glyph.bitmap.size, Size::new(30, 30));

            d.set(true);
            run_loop.stop();
        }
    }));

    test.run(
        "test/fixtures/resources/glyphs-12244-12543.pbf",
        dependencies(&['々']), // 0x3005
    );
    assert!(done.get());
}

// gtest: GlyphManager.LoadingInvalid
#[test]
fn loading_invalid() {
    let mut test = GlyphManagerTest::new();

    test.file_source.set_response(Kind::Glyphs, |resource: &Resource| {
        assert_eq!(Kind::Glyphs, resource.kind);
        let mut response = Response::new();
        response.data = Some(Arc::new(read_file("test/fixtures/resources/fake_glyphs-0-255.pbf")));
        Some(response)
    });

    *test.observer.glyphs_error.borrow_mut() = Some(Box::new(|_, _, _| panic!("glyphsError")));

    *test.observer.glyphs_loaded.borrow_mut() = Some(Box::new(|font_stack, range| {
        assert_eq!(*font_stack, test_stack());
        assert_eq!(*range, GlyphRange::new(0, 255, GlyphIDType::FontPBF));
    }));

    let available = Rc::new(Cell::new(false));
    let (a, run_loop) = (available.clone(), test.run_loop.clone());
    *test.requestor.glyphs_available.borrow_mut() = Some(Rc::new(move |glyphs: GlyphMap| {
        let test_positions = &glyphs[&font_stack_hash(&test_stack())];

        assert_eq!(test_positions.len(), 2);
        assert!(test_positions[&gid('A')].is_none());
        assert!(test_positions[&gid('E')].is_some());

        a.set(true);
        run_loop.stop();
    }));

    test.run("test/fixtures/resources/glyphs.pbf", dependencies(&['A', 'E']));
    assert!(available.get());
}

// gtest: GlyphManager.ImmediateFileSource
#[test]
fn immediate_file_source() {
    // `GlyphManagerTestSynchronous`: the default GlyphManager (no local
    // rasterizer) and a synchronous StubFileSource.
    let mut test = GlyphManagerTest::with(
        Box::new(mln_text::local_glyph_rasterizer::DefaultLocalGlyphRasterizer::new(None)),
        ResponseType::Synchronous,
    );

    test.file_source.set_response(Kind::Glyphs, glyphs_response("test/fixtures/resources/glyphs.pbf"));

    *test.observer.glyphs_error.borrow_mut() = Some(Box::new(|_, _, _| panic!("glyphsError")));

    let available = Rc::new(Cell::new(false));
    let (a, run_loop) = (available.clone(), test.run_loop.clone());
    *test.requestor.glyphs_available.borrow_mut() = Some(Rc::new(move |_| {
        a.set(true);
        run_loop.stop();
    }));

    test.run("test/fixtures/resources/glyphs.pbf", dependencies(&['a', 'å', ' ']));
    assert!(available.get());
    test.end();
}

// Guards against silent drift between allowsFixedWidthGlyphGeneration's range
// coverage and the precomputed NON_IDEOGRAPH_GLYPH_RANGES_PER_FONT_STACK used
// for offline download progress estimation.
// gtest: i18n.GlyphRangeLocalGenerationCountMatchesConstant
#[test]
fn glyph_range_local_generation_count_matches_constant() {
    let mut locally_generated_ranges: u32 = 0;
    for i in 0..GLYPH_RANGES_PER_FONT_STACK {
        if i18n::glyph_range_is_entirely_locally_generated((i * GLYPHS_PER_GLYPH_RANGE) as u16) {
            locally_generated_ranges += 1;
        }
    }
    assert_eq!(GLYPH_RANGES_PER_FONT_STACK - locally_generated_ranges, NON_IDEOGRAPH_GLYPH_RANGES_PER_FONT_STACK);
}

// Port-only: a font-face engine that gets ready later (the browser build's,
// which loads its shaper as a second wasm module).

/// An engine that is ready once the test calls back the `ready`s it holds.
#[derive(Default)]
struct DeferredEngine {
    ready: Mutex<Vec<Box<dyn FnOnce(Result<(), String>) + Send>>>,
    prepares: AtomicUsize,
    loads: AtomicUsize,
}

impl DeferredEngine {
    fn report(&self, result: Result<(), String>) {
        let ready = std::mem::take(&mut *self.ready.lock().unwrap());
        for ready in ready {
            ready(result.clone());
        }
    }
}

impl FontFaceEngine for DeferredEngine {
    fn prepare(&self, ready: Box<dyn FnOnce(Result<(), String>) + Send>) {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        self.ready.lock().unwrap().push(ready);
    }

    fn load(&self, _type: GlyphIDType, data: &[u8]) -> Option<Box<dyn FontFaceShaper>> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        (data == b"font").then(|| Box::new(FakeShaper) as Box<dyn FontFaceShaper>)
    }
}

/// Shapes each code unit as the glyph of that index, 10 px wide.
struct FakeShaper;

impl FontFaceShaper for FakeShaper {
    fn shape(&self, text: &[u16], _rtl: bool, glyph_ids: &mut Vec<GlyphID>, adjusts: &mut Vec<HBShapeAdjust>) {
        for &unit in text {
            glyph_ids.push(GlyphID::with_type(unit, FONT_FACE));
            adjusts.push(HBShapeAdjust::new(0.0, 0.0, 10.0));
        }
    }

    fn rasterize(&self, glyph: &GlyphID) -> Glyph {
        stub_glyph(*glyph)
    }
}

const FONT_FACE: GlyphIDType = GlyphIDType(7);

/// A manager on `engine` with one font face, and a synchronous file source
/// that answers its file with "font".
fn font_face_test(engine: &Arc<DeferredEngine>) -> GlyphManagerTest {
    let mut test = GlyphManagerTest::with(Box::new(StubLocalGlyphRasterizer), ResponseType::Synchronous);
    test.glyph_manager = GlyphManager::with_font_face_engine(Some(Box::new(StubLocalGlyphRasterizer)), engine.clone());
    test.file_source.set_response(Kind::Glyphs, |resource: &Resource| {
        assert_eq!(resource.url, "test://face.ttf");
        let mut response = Response::new();
        response.data = Some(Arc::new(b"font".to_vec()));
        Some(response)
    });
    let face =
        FontFace { type_: FONT_FACE, name: "Test Stack".to_owned(), url: "test://face.ttf".to_owned(), ranges: vec![] };
    test.glyph_manager.set_font_faces(Some(Arc::new(vec![face])));
    test._log = Some(SquelchLog::new());
    test
}

/// Requests glyph 0 of the font face; returns what the requestor got.
fn request_font_face(test: &GlyphManagerTest) -> Rc<RefCell<Option<GlyphMap>>> {
    let available: Rc<RefCell<Option<GlyphMap>>> = Rc::default();
    let a = available.clone();
    *test.requestor.glyphs_available.borrow_mut() = Some(Rc::new(move |glyphs: GlyphMap| {
        *a.borrow_mut() = Some(glyphs);
    }));
    let ids: GlyphIDs = [GlyphID::with_type(0, FONT_FACE)].into_iter().collect();
    let dependencies = GlyphDependencies { glyphs: VecMap::from([(test_stack(), ids)]), shapes: Default::default() };
    test.glyph_manager.get_glyphs(&test.requestor(), dependencies, Some(&test.file_source()));
    available
}

#[test]
fn font_face_waits_for_the_engine() {
    let engine = Arc::new(DeferredEngine::default());
    let test = font_face_test(&engine);
    // Asked to get ready by the font faces, before any file arrives.
    assert_eq!(engine.prepares.load(Ordering::SeqCst), 1);

    // The file arrived (synchronously), but the engine isn't ready: the
    // requestor waits, and the file isn't parsed.
    let available = request_font_face(&test);
    test.run_loop.run_once();
    assert!(available.borrow().is_none());
    assert_eq!(engine.loads.load(Ordering::SeqCst), 0);
    assert!(test.glyph_manager.get_hb_shaper(&test_stack(), FONT_FACE).is_none());

    // Once it is, the RunLoop parses the file and notifies the requestor.
    engine.report(Ok(()));
    assert!(available.borrow().is_none());
    test.run_loop.run_once();
    assert_eq!(engine.loads.load(Ordering::SeqCst), 1);
    assert_eq!(engine.prepares.load(Ordering::SeqCst), 1);
    let glyphs = available.borrow_mut().take().expect("notified");
    assert!(glyphs[&font_stack_hash(&test_stack())][&GlyphID::with_type(0, FONT_FACE)].is_some());

    // Shaping and rasterizing are synchronous from then on.
    let (mut ids, mut adjusts) = (Vec::new(), Vec::new());
    test.glyph_manager.hb_shaping(&[5, 6], false, &test_stack(), FONT_FACE, &mut ids, &mut adjusts);
    assert_eq!(ids, vec![GlyphID::with_type(5, FONT_FACE), GlyphID::with_type(6, FONT_FACE)]);
    assert_eq!(adjusts.len(), 2);
    let glyph = test.glyph_manager.get_glyph(&test_stack(), GlyphID::with_type(5, FONT_FACE));
    assert_eq!(glyph.id, GlyphID::with_type(5, FONT_FACE));
    assert_eq!(glyph.metrics.width, 24);

    // A later request is answered at once.
    let available = request_font_face(&test);
    assert!(available.borrow().is_some());
}

#[test]
fn font_face_engine_failure_leaves_the_face_out() {
    let engine = Arc::new(DeferredEngine::default());
    let test = font_face_test(&engine);
    let available = request_font_face(&test);
    engine.report(Err("no shaper".to_owned()));
    test.run_loop.run_once();
    // Notified, as for a file that isn't a font: without the glyph.
    assert_eq!(engine.loads.load(Ordering::SeqCst), 0);
    let glyphs = available.borrow_mut().take().expect("notified");
    assert!(glyphs[&font_stack_hash(&test_stack())][&GlyphID::with_type(0, FONT_FACE)].is_none());
    assert!(test.glyph_manager.get_hb_shaper(&test_stack(), FONT_FACE).is_none());
}
