// Ports test/text/glyph_cache.test.cpp.

use std::sync::Arc;

use mln_text::glyph::{Glyph, GlyphID, GlyphIDType, GlyphMetrics, GlyphRange};
use mln_text::glyph_cache::{GlyphCache, GlyphCacheRange};
use mln_text::glyph_pbf::parse_glyph_pbf;
use mln_util::image::AlphaImage;
use mln_util::immutable::Immutable;
use mln_util::size::Size;

use super::read_file;

fn glyph(id: char, value: u8) -> Glyph {
    let mut bitmap = AlphaImage::new(Size::new(4, 4));
    bitmap.fill(value);
    Glyph { id: GlyphID::new(id as u32), metrics: GlyphMetrics { width: 4, height: 4, ..Default::default() }, bitmap }
}

/// The range in test/fixtures/resources/glyphs.pbf.
fn fixture_range() -> GlyphRange {
    GlyphRange::new(0, 255, GlyphIDType::FontPBF)
}

fn intern_range(cache: &GlyphCache, data: &[u8]) -> Vec<Immutable<Glyph>> {
    parse_glyph_pbf(&fixture_range(), data).unwrap().into_iter().map(|parsed| cache.intern_glyph(parsed)).collect()
}

fn range(id: char, glyph: &Immutable<Glyph>) -> GlyphCacheRange {
    GlyphCacheRange::from([(GlyphID::new(id as u32), glyph.clone())])
}

// gtest: GlyphCache.InternsEqualGlyphsOnce
#[test]
fn interns_equal_glyphs_once() {
    let cache = GlyphCache::new();

    assert!(Arc::ptr_eq(&cache.intern_glyph(glyph('a', 1)), &cache.intern_glyph(glyph('a', 1))));
}

// gtest: GlyphCache.KeepsGlyphsWithTheSameIdAndDifferentBitmapsApart
#[test]
fn keeps_glyphs_with_the_same_id_and_different_bitmaps_apart() {
    let cache = GlyphCache::new();

    assert!(!Arc::ptr_eq(&cache.intern_glyph(glyph('a', 1)), &cache.intern_glyph(glyph('a', 2))));
}

// gtest: GlyphCache.KeepsGlyphsWithDifferentIdsOrMetricsApart
#[test]
fn keeps_glyphs_with_different_ids_or_metrics_apart() {
    let cache = GlyphCache::new();
    let a = cache.intern_glyph(glyph('a', 1));
    let mut shifted = glyph('a', 1);
    shifted.metrics.left = 1.0;

    assert!(!Arc::ptr_eq(&a, &cache.intern_glyph(glyph('b', 1))));
    assert!(!Arc::ptr_eq(&a, &cache.intern_glyph(shifted)));
}

// gtest: GlyphCache.ReleasesAGlyphWithItsLastOwner
#[test]
fn releases_a_glyph_with_its_last_owner() {
    let cache = GlyphCache::new();
    let _kept = cache.intern_glyph(glyph('a', 1));
    cache.intern_glyph(glyph('b', 1));

    assert_eq!(1, cache.size());
}

// gtest: GlyphCache.InternsTheGlyphsOfARangeParsedOnTwoThreads
#[test]
fn interns_the_glyphs_of_a_range_parsed_on_two_threads() {
    let cache = GlyphCache::new();
    let data = read_file("test/fixtures/resources/glyphs.pbf");

    let (first, second) = std::thread::scope(|scope| {
        let a = scope.spawn(|| intern_range(&cache, &data));
        let b = scope.spawn(|| intern_range(&cache, &data));
        (a.join().unwrap(), b.join().unwrap())
    });

    assert!(!first.is_empty());
    assert_eq!(first.len(), second.len());
    assert!(first.iter().zip(&second).all(|(a, b)| Arc::ptr_eq(a, b)));
}

// gtest: GlyphCache.InternsRangesOfTheSameGlyphsOnce
#[test]
fn interns_ranges_of_the_same_glyphs_once() {
    let cache = GlyphCache::new();
    let a = cache.intern_glyph(glyph('a', 1));

    assert!(Arc::ptr_eq(&cache.intern_range(range('a', &a)), &cache.intern_range(range('a', &a))));
}

// gtest: GlyphCache.KeepsRangesOfDifferentGlyphsApart
#[test]
fn keeps_ranges_of_different_glyphs_apart() {
    let cache = GlyphCache::new();
    let a = cache.intern_glyph(glyph('a', 1));
    let b = cache.intern_glyph(glyph('a', 2));

    assert!(!Arc::ptr_eq(&cache.intern_range(range('a', &a)), &cache.intern_range(range('a', &b))));
}

// gtest: GlyphCache.ReleasesARangeWithItsLastOwner
#[test]
fn releases_a_range_with_its_last_owner() {
    let cache = GlyphCache::new();
    let a = cache.intern_glyph(glyph('a', 1));
    let range = Arc::downgrade(&cache.intern_range(range('a', &a)));

    assert_eq!(0, range.strong_count());
}
