// Ported gtests of test/text/*.test.cpp that need mln_renderer (GlyphManager,
// SymbolLayout), sit above mln_text (Formatted is mln_expression's) or read
// its fixtures (GlyphCache), one module per C++ file. The others are ported next to their code: bidi,
// tagged_string, glyph_pbf, language_tag and shaping in mln_text;
// quads, get_anchors and cross_tile_symbol_index in mln_renderer::text;
// local_glyph_rasterizer in mln_headless (it renders a map).

mod calculate_tile_distances;
mod formatted;
mod glyph_cache;
mod glyph_manager;
mod glyph_manager_bitmaps;

use std::path::PathBuf;

use mln_util::logging::{Event, EventSeverity, Log, Observer};

/// A path of the C++ repository, in the copy of its files these tests read
/// (testdata/).
pub fn cpp_path(relative: &str) -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(relative)
}

/// `util::read_file` on a C++ test fixture.
pub fn read_file(relative: &str) -> Vec<u8> {
    let path = cpp_path(relative);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

struct NullObserver;

impl Observer for NullObserver {
    fn on_record(&self, _: EventSeverity, _: Event, _: i64, _: &str) -> bool {
        true
    }
}

/// `Log::setObserver(std::make_unique<Log::NullObserver>())` for the
/// calling thread, undone on drop.
pub struct SquelchLog;

impl SquelchLog {
    pub fn new() -> Self {
        Log::set_thread_observer(Some(Box::new(NullObserver)));
        SquelchLog
    }
}

impl Drop for SquelchLog {
    fn drop(&mut self) {
        Log::set_thread_observer(None);
    }
}
