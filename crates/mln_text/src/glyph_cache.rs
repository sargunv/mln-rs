// Ports include/mln/text/glyph_cache.hpp, src/mln/text/glyph_cache.cpp and
// src/mln/util/weak_immutable.hpp.
//
// `WeakImmutable<T>` is `std::sync::Weak<T>` here, since `Immutable<T>` is an
// `Arc<T>`. C++'s `std::unordered_multimap` is a map from hash to a bucket of
// weak references.

use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, Weak};

use mln_util::immutable::Immutable;
use mln_util::sync::MutexExt;

use crate::glyph::{Glyph, GlyphID};

/// `GlyphCache::Range`: the glyphs parsed from one glyph range.
pub type GlyphCacheRange = BTreeMap<GlyphID, Immutable<Glyph>>;

/// The table sizes below which expired entries are left in place.
const MIN_SWEEP: usize = 4096;

/// `GlyphCache`: keeps one copy of equal glyphs and glyph ranges. It only
/// holds weak references, so it never keeps a glyph alive. Renderers given
/// the same cache share the glyphs they parse from glyph PBFs, across maps
/// and font stacks. Thread-safe.
#[derive(Default)]
pub struct GlyphCache {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    glyphs: Table<Glyph>,
    ranges: Table<GlyphCacheRange>,
}

struct Table<T> {
    buckets: HashMap<u64, Vec<Weak<T>>>,
    /// Entries in `buckets`, expired or not.
    len: usize,
    next_sweep: usize,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Table { buckets: HashMap::new(), len: 0, next_sweep: MIN_SWEEP }
    }
}

impl<T> Table<T> {
    /// The live entry equal to `value` by `eq`, or `value` added.
    fn intern(&mut self, hash: u64, value: T, eq: impl Fn(&T, &T) -> bool) -> Arc<T> {
        let bucket = self.buckets.entry(hash).or_default();
        for weak in bucket.iter() {
            if let Some(alive) = weak.upgrade()
                && eq(&alive, &value)
            {
                return alive;
            }
        }
        let added = Arc::new(value);
        bucket.push(Arc::downgrade(&added));
        self.len += 1;
        if self.len >= self.next_sweep {
            self.sweep();
        }
        added
    }

    /// Drops the expired entries. The next sweep waits until the table has
    /// doubled, so sweeping costs constant time per insertion.
    fn sweep(&mut self) {
        self.buckets.retain(|_, bucket| {
            bucket.retain(|weak| weak.strong_count() > 0);
            !bucket.is_empty()
        });
        self.len = self.buckets.values().map(Vec::len).sum();
        self.next_sweep = MIN_SWEEP.max(2 * self.len);
    }
}

fn hash_glyph(glyph: &Glyph) -> u64 {
    let mut hasher = DefaultHasher::new();
    let metrics = &glyph.metrics;
    glyph.id.hash().hash(&mut hasher);
    metrics.width.hash(&mut hasher);
    metrics.height.hash(&mut hasher);
    metrics.left.to_bits().hash(&mut hasher);
    metrics.top.to_bits().hash(&mut hasher);
    metrics.advance.hash(&mut hasher);
    metrics.is_double_resolution.hash(&mut hasher);
    glyph.bitmap.size.width.hash(&mut hasher);
    glyph.bitmap.size.height.hash(&mut hasher);
    glyph.bitmap.data.hash(&mut hasher);
    hasher.finish()
}

/// Ranges are the same when they point to the same glyphs.
fn same_range(a: &GlyphCacheRange, b: &GlyphCacheRange) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|((a_id, a), (b_id, b))| a_id == b_id && Arc::ptr_eq(a, b))
}

impl GlyphCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// `intern(Glyph&&)`: the cached glyph equal to `glyph`, or `glyph`
    /// cached.
    pub fn intern_glyph(&self, glyph: Glyph) -> Immutable<Glyph> {
        let hash = hash_glyph(&glyph);
        self.inner.lock_unpoisoned().glyphs.intern(hash, glyph, |a, b| a == b)
    }

    /// `intern(Range&&)`: the cached range that points to the same glyphs as
    /// `range`, or `range` cached. Ranges are matched by the glyphs they
    /// point to: intern the glyphs first.
    pub fn intern_range(&self, range: GlyphCacheRange) -> Arc<GlyphCacheRange> {
        let mut hasher = DefaultHasher::new();
        for (id, glyph) in &range {
            id.hash().hash(&mut hasher);
            Arc::as_ptr(glyph).hash(&mut hasher);
        }
        let hash = hasher.finish();
        self.inner.lock_unpoisoned().ranges.intern(hash, range, same_range)
    }

    /// `size()`: the glyphs alive, not counting ranges. Walks the whole
    /// table.
    pub fn size(&self) -> usize {
        let inner = self.inner.lock_unpoisoned();
        inner.glyphs.buckets.values().flatten().filter(|weak| weak.strong_count() > 0).count()
    }
}
