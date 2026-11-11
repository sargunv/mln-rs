// Ports include/mln/text/glyph.hpp, include/mln/text/glyph_range.hpp and
// src/mln/text/glyph.cpp.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Arc, Mutex};

use mln_style_types::image::RectU16;
use mln_util::font_stack::{FontStack, font_stack_to_string};
use mln_util::image::AlphaImage;
use mln_util::immutable::Immutable;
use mln_util::sync::MutexExt;
use mln_vec_map::VecMap;
use serde::{Deserialize, Serialize};

use crate::tagged_string::FontStackHash;

/// `GlyphIDType` (glyph_range.hpp): `FontPBF` (0) for glyphs from glyph PBFs,
/// other values are generated at runtime for HarfBuzz-shaped font faces
/// (`genNewGlyphIDType`), so this is a newtype, not a Rust enum.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize)]
pub struct GlyphIDType(pub u16);

#[allow(non_upper_case_globals)]
impl GlyphIDType {
    pub const FontPBF: GlyphIDType = GlyphIDType(0x00);
}

/// `GLYPHS_PER_GLYPH_RANGE` etc. (glyph_range.hpp).
pub const GLYPHS_PER_GLYPH_RANGE: u32 = 256;
pub const GLYPH_RANGES_PER_FONT_STACK: u32 = 256;
pub const NON_IDEOGRAPH_GLYPH_RANGES_PER_FONT_STACK: u32 = 97;

/// `GlyphRange` (glyph_range.hpp / glyph.cpp).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GlyphRange {
    pub first: u16,
    pub second: u16,
    pub type_: GlyphIDType,
}

impl GlyphRange {
    /// `GlyphRange(uint32_t, uint32_t, GlyphIDType = FontPBF)`: the bounds
    /// are truncated to `uint16_t` as in C++.
    pub fn new(first: u32, second: u32, type_: GlyphIDType) -> Self {
        GlyphRange { first: first as u16, second: second as u16, type_ }
    }
}

impl PartialOrd for GlyphRange {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GlyphRange {
    /// `GlyphRange::operator<`: first, then second, then type.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.first, self.second, self.type_).cmp(&(other.first, other.second, other.type_))
    }
}

/// `union GlyphID`: `complex.code` (UTF-16 code unit or font glyph index)
/// and `complex.type`; `hash` is the two read as one little-endian
/// `char32_t`, which is what C++ compares.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
pub struct GlyphID {
    pub code: u16,
    pub type_: GlyphIDType,
}

impl GlyphID {
    /// `GlyphID(int)` / `GlyphID(uint32_t)` / `GlyphID(char16_t)`: the
    /// codepoint is truncated to `char16_t`, the type is `FontPBF`.
    pub fn new(codepoint: u32) -> Self {
        GlyphID { code: codepoint as u16, type_: GlyphIDType::FontPBF }
    }

    /// `GlyphID(char16_t index, GlyphIDType t)`.
    pub fn with_type(index: u16, type_: GlyphIDType) -> Self {
        GlyphID { code: index, type_ }
    }

    /// The `hash` member of the union (x86-64 / arm64 are little endian).
    pub fn hash(&self) -> u32 {
        ((self.type_.0 as u32) << 16) | self.code as u32
    }
}

impl PartialOrd for GlyphID {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GlyphID {
    /// `GlyphID::operator<` compares `hash`.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.hash().cmp(&other.hash())
    }
}

impl From<u16> for GlyphID {
    fn from(code: u16) -> Self {
        GlyphID::new(code as u32)
    }
}

/// `GlyphIDs` (`std::set<GlyphID>`).
pub type GlyphIDs = std::collections::BTreeSet<GlyphID>;

/// `getGlyphRange`. Note: this only works for the BMP.
pub fn get_glyph_range(glyph: GlyphID) -> GlyphRange {
    let start = (glyph.code as u32 / 256) * 256;
    let end = start + 255;
    let start = start.min(65280);
    let end = end.min(65535);
    GlyphRange::new(start, end, glyph.type_)
}

/// `GlyphMetrics`.
#[derive(Clone, Copy, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct GlyphMetrics {
    pub width: u32,
    pub height: u32,
    pub left: f32,
    pub top: f32,
    pub advance: u32,
    /// True when the glyph bitmap is rendered at 2x texture resolution
    /// (e.g. local CJK glyphs).
    pub is_double_resolution: bool,
}

/// `Glyph`.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct Glyph {
    pub id: GlyphID,
    /// A signed distance field of the glyph with a border (see `BORDER_SIZE`).
    pub bitmap: AlphaImage,
    pub metrics: GlyphMetrics,
}

impl Glyph {
    /// `Glyph::borderSize`. We're using this value throughout the Mapbox GL
    /// ecosystem. If this is different, the glyphs also need to be reencoded.
    pub const BORDER_SIZE: u8 = 3;
}

/// `Glyphs = std::map<GlyphID, std::optional<Immutable<Glyph>>>`.
pub type Glyphs = BTreeMap<GlyphID, Option<Immutable<Glyph>>>;
/// `GlyphMap = std::map<FontStackHash, Glyphs>`.
pub type GlyphMap = VecMap<FontStackHash, Glyphs>;

/// `PositionedGlyph`.
#[derive(Clone, Debug, PartialEq)]
pub struct PositionedGlyph {
    pub glyph: GlyphID,
    pub x: f32,
    pub y: f32,
    pub vertical: bool,
    pub font: FontStackHash,
    pub scale: f32,
    pub rect: RectU16,
    pub metrics: GlyphMetrics,
    pub image_id: Option<String>,
    /// Maps positioned glyph to TaggedString section
    pub section_index: usize,
}

impl PositionedGlyph {
    /// `PositionedGlyph(glyph, x, y, vertical, font, scale, rect, metrics,
    /// imageID, sectionIndex = 0)`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        glyph: GlyphID,
        x: f32,
        y: f32,
        vertical: bool,
        font: FontStackHash,
        scale: f32,
        rect: RectU16,
        metrics: GlyphMetrics,
        image_id: Option<String>,
        section_index: usize,
    ) -> Self {
        PositionedGlyph { glyph, x, y, vertical, font, scale, rect, metrics, image_id, section_index }
    }
}

/// `enum class WritingModeType : uint8_t`, a bitmask
/// (util/bitmask_operations.hpp).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
pub struct WritingModeType(pub u8);

#[allow(non_upper_case_globals)]
impl WritingModeType {
    pub const None: WritingModeType = WritingModeType(0);
    pub const Horizontal: WritingModeType = WritingModeType(1 << 0);
    pub const Vertical: WritingModeType = WritingModeType(1 << 1);
}

impl std::ops::BitOr for WritingModeType {
    type Output = WritingModeType;
    fn bitor(self, rhs: Self) -> Self {
        WritingModeType(self.0 | rhs.0)
    }
}

impl std::ops::BitAnd for WritingModeType {
    type Output = WritingModeType;
    fn bitand(self, rhs: Self) -> Self {
        WritingModeType(self.0 & rhs.0)
    }
}

impl std::ops::Not for WritingModeType {
    type Output = WritingModeType;
    fn not(self) -> Self {
        WritingModeType(!self.0)
    }
}

/// `PositionedLine`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PositionedLine {
    pub positioned_glyphs: Vec<PositionedGlyph>,
    pub line_offset: f32,
}

/// `Shaping`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shaping {
    pub positioned_lines: Vec<PositionedLine>,
    pub top: f32,
    pub bottom: f32,
    pub left: f32,
    pub right: f32,
    /// C++ leaves this uninitialized in `Shaping()`; here it is `None`.
    pub writing_mode: WritingModeType,
    pub verticalizable: bool,
    pub icons_in_text: bool,
}

impl Shaping {
    /// The y offset *should* be part of the font metadata.
    pub const Y_OFFSET: i32 = -17;

    /// `Shaping(float x, float y, WritingModeType)`.
    pub fn new(x: f32, y: f32, writing_mode: WritingModeType) -> Self {
        Shaping { top: y, bottom: y, left: x, right: x, writing_mode, ..Default::default() }
    }

    /// `explicit operator bool`: whether any line has a glyph.
    pub fn is_some(&self) -> bool {
        self.positioned_lines.iter().any(|line| !line.positioned_glyphs.is_empty())
    }
}

/// `FontFace::Range`.
pub type FontFaceRange = (u32, u32);

/// The bytes of a font file that an app already holds, such as a
/// memory-mapped file, shared without a copy.
#[derive(Clone)]
pub struct FontData(Arc<dyn AsRef<[u8]> + Send + Sync>);

impl FontData {
    pub fn new(bytes: Arc<dyn AsRef<[u8]> + Send + Sync>) -> Self {
        FontData(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        (*self.0).as_ref()
    }
}

impl From<Arc<Vec<u8>>> for FontData {
    fn from(bytes: Arc<Vec<u8>>) -> Self {
        FontData(bytes)
    }
}

impl std::fmt::Debug for FontData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not the bytes: they may load on first access.
        f.write_str("FontData")
    }
}

/// Equal when both share the same bytes.
impl PartialEq for FontData {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// `FontFace`: style defined faces. A face without a name is a fallback,
/// which every font stack uses after the faces that name one of its fonts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FontFace {
    /// an unique glyph id
    pub type_: GlyphIDType,
    /// font face name
    pub name: String,
    /// font file url
    pub url: String,
    /// unicode ranges
    pub ranges: Vec<FontFaceRange>,
    /// The font file itself, which the map then uses instead of fetching
    /// `url`. Left out of the faces that workers get, which don't read
    /// font files.
    #[serde(skip)]
    pub data: Option<FontData>,
}

impl FontFace {
    pub fn new(name: &str, url: &str, ranges: Vec<FontFaceRange>) -> Self {
        FontFace { type_: GlyphIDType::FontPBF, name: name.to_string(), url: url.to_string(), ranges, data: None }
    }

    /// A fallback face, without a name, for the font file at `url`, which
    /// draws the code points in `ranges`, with its glyph type.
    pub fn fallback(url: &str, ranges: Vec<FontFaceRange>) -> Self {
        let mut face = FontFace::new("", url, ranges);
        face.type_ = gen_new_glyph_id_type(&face.url, &FontStack::new(), &face.ranges);
        face
    }

    /// The face with its font file in memory, so that `url` only names it.
    pub fn with_data(mut self, data: FontData) -> Self {
        self.data = Some(data);
        self
    }

    pub fn valid(&self) -> bool {
        !self.name.is_empty() && !self.url.is_empty() && !self.ranges.is_empty()
    }
}

/// `FontFaces`.
pub type FontFaces = Vec<FontFace>;

/// `HBShapeRequest`.
#[derive(Clone, Debug, PartialEq)]
pub struct HBShapeRequest {
    pub str_: Vec<u16>,
    pub font_stack: FontStack,
    pub type_: GlyphIDType,
}

/// `HBShapeRequests = std::map<FontStack, std::map<GlyphIDType,
/// std::set<std::u16string>>>`.
pub type HBShapeRequests = VecMap<FontStack, VecMap<GlyphIDType, BTreeSet<Vec<u16>>>>;

/// `GlyphDependencies`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GlyphDependencies {
    pub glyphs: VecMap<FontStack, GlyphIDs>,
    pub shapes: HBShapeRequests,
}

/// `GlyphRangeDependencies = std::map<FontStack, std::unordered_set<GlyphRange>>`.
pub type GlyphRangeDependencies = VecMap<FontStack, HashSet<GlyphRange>>;

/// `GlyphPosition`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GlyphPosition {
    pub rect: RectU16,
    pub metrics: GlyphMetrics,
}

/// `GlyphPositionMap = std::map<GlyphID, GlyphPosition>`.
pub type GlyphPositionMap = BTreeMap<GlyphID, GlyphPosition>;
/// `GlyphPositions = std::map<FontStackHash, GlyphPositionMap>`.
pub type GlyphPositions = VecMap<FontStackHash, GlyphPositionMap>;

/// `genNewGlyphIDType()`: a process-wide counter (a function-local static
/// in C++) that skips `FontPBF` when it wraps.
fn gen_new_glyph_id_type_counter(counter: &mut i16) -> GlyphIDType {
    *counter = counter.wrapping_add(1);
    if *counter == GlyphIDType::FontPBF.0 as i16 {
        *counter = counter.wrapping_add(1);
    }
    GlyphIDType(*counter as u16)
}

struct GlyphTypes {
    counter: i16,
    types: VecMap<String, VecMap<String, VecMap<usize, GlyphIDType>>>,
}

static GLYPH_TYPES: Mutex<GlyphTypes> =
    Mutex::new(GlyphTypes { counter: GlyphIDType::FontPBF.0 as i16, types: VecMap::new() });

/// `util::hash_combine` with libstdc++'s identity `std::hash<uint32_t>`.
fn hash_combine(seed: &mut usize, v: u32) {
    *seed ^= (v as usize).wrapping_add(0x9e3779b9).wrapping_add(*seed << 6).wrapping_add(*seed >> 2);
}

/// `genNewGlyphIDType(url, fontStack, pairs)`: one type per (url, font
/// stack, unicode ranges), stable for the process.
pub fn gen_new_glyph_id_type(url: &str, font_stack: &FontStack, pairs: &[(u32, u32)]) -> GlyphIDType {
    let mut hash = 0usize;
    for pair in pairs {
        hash_combine(&mut hash, pair.0);
        hash_combine(&mut hash, pair.1);
    }
    let family = font_stack_to_string(font_stack);

    let mut state = GLYPH_TYPES.lock_unpoisoned();
    let state = &mut *state;
    let entry = state
        .types
        .entry(url.to_string())
        .or_default()
        .entry(family)
        .or_default()
        .entry(hash)
        .or_insert(GlyphIDType::FontPBF);
    if *entry == GlyphIDType::FontPBF {
        *entry = gen_new_glyph_id_type_counter(&mut state.counter);
    }
    *entry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_id_order_is_the_union_hash() {
        let a = GlyphID::with_type(0xFFFF, GlyphIDType::FontPBF);
        let b = GlyphID::with_type(0, GlyphIDType(1));
        assert!(a < b);
        assert_eq!(b.hash(), 0x10000);
        assert_eq!(GlyphID::new(0x1F600).code, 0xF600);
    }

    #[test]
    fn glyph_range() {
        let r = get_glyph_range(GlyphID::new(0x41));
        assert_eq!((r.first, r.second), (0, 255));
        let r = get_glyph_range(GlyphID::new(0xFFFF));
        assert_eq!((r.first, r.second), (65280, 65535));
    }

    #[test]
    fn new_glyph_id_types_are_stable_per_key() {
        let fs = vec!["Noto Sans Regular".to_string()];
        let a = gen_new_glyph_id_type("local://a.ttf", &fs, &[(0x1780, 0x17FF)]);
        let b = gen_new_glyph_id_type("local://a.ttf", &fs, &[(0x1780, 0x17FF)]);
        let c = gen_new_glyph_id_type("local://b.ttf", &fs, &[(0x1780, 0x17FF)]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, GlyphIDType::FontPBF);
    }
}
