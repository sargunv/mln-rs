// Ports src/mln/text/harfbuzz_impl.hpp and src/mln/text/harfbuzz_impl.cpp
// on HarfRust.
//
// The C++ shapes with HarfBuzz 8.2.0 built with HB_TINY on an hb-ft font
// (`hb_ft_font_create`). The port shapes with HarfRust on:
// - `HbFtFont` (crate::hb_ft): hb-ft's glyph mapping and advances on the
//   ported FreeType face, at hb-ft's scale (26.6 at 24 px);
// - a view of the font that hides what HB_TINY compiles out, so HarfRust
//   takes the same code paths: the AAT tables (HB_NO_AAT: morx, mort, kerx,
//   ankr, trak, feat), `kern` (HB_NO_OT_KERN, and HB_NO_AAT for the AAT
//   flavour) and the GSUB/GPOS FeatureVariations (HB_NO_VAR); and user
//   features `frac=0`, `numr=0`, `dnom=0` (HB_NO_OT_SHAPE_FRACTIONS:
//   HB_TINY never adds the automatic fraction features).
// HarfRust has no switch for some HB_TINY options (HB_NO_OT_SHAPE_FALLBACK,
// HB_NO_OT_SHAPER_VOWEL_CONSTRAINTS, HB_NO_UCD_UNASSIGNED), so a few texts
// shape differently: fallback mark positioning, an inserted U+25CC, and
// Unicode 18.0 instead of 15.0 properties.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use harfrust::font::{Blob, Font, TableFunction};
use harfrust::{Buffer, Direction, Feature, Language, Script, ShapeOptions, ShapePlan, ShaperFont, Tag};
use mln_util::sync::MutexExt;

use crate::freetype::{FreeTypeFace, FreeTypeLibrary};
use crate::glyph::{Glyph, GlyphID, GlyphIDType};
use crate::harfbuzz::HBShapeAdjust;
use crate::hb_ft::HbFtFont;
use crate::hb_ucd;

/// Tables HarfBuzz never reads when built with HB_TINY.
const HB_TINY_HIDDEN_TABLES: [&[u8; 4]; 7] = [b"morx", b"mort", b"kerx", b"kern", b"ankr", b"trak", b"feat"];

/// `getDefaultLanguage()`: `hb_language_get_default()`, which with
/// HB_NO_SETLOCALE is `hb_language_from_string ("C")` = "c".
fn get_default_language() -> Language {
    Language::new("c").expect("non-empty")
}

/// `getUnicodeScript(u)`: combining marks inherit the script of their
/// bases, regardless of their own script. Called on UTF-16 code units, as
/// in C++ (surrogates are classified on their own).
pub fn get_unicode_script(u: u32) -> [u8; 4] {
    if hb_ucd::general_category(u) == hb_ucd::NON_SPACING_MARK {
        return *b"Zinh"; // HB_SCRIPT_INHERITED
    }
    hb_ucd::script(u)
}

/// `HBShaper::Impl`.
pub struct Impl {
    type_: GlyphIDType,
    face: FreeTypeFace,
    /// `hb_font_t *font` (its face: the HB_TINY view of the tables).
    font: Option<Font>,
    /// HarfBuzz caches shape plans per segment properties (script and
    /// direction); so do we.
    plans: Mutex<HashMap<([u8; 4], bool), Arc<ShapePlan>>>,
}

/// The user features that stand in for HB_NO_OT_SHAPE_FRACTIONS.
fn hb_tiny_features() -> [Feature; 3] {
    [
        Feature::new(Tag::new(b"frac"), 0, ..),
        Feature::new(Tag::new(b"numr"), 0, ..),
        Feature::new(Tag::new(b"dnom"), 0, ..),
    ]
}

/// A table of a font file, in the file's own bytes.
struct TableBytes {
    data: Arc<Vec<u8>>,
    range: std::ops::Range<usize>,
}

impl AsRef<[u8]> for TableBytes {
    fn as_ref(&self) -> &[u8] {
        &self.data[self.range.clone()]
    }
}

/// The tables of `face` as HarfBuzz (hb-ft's `_hb_ft_reference_table`, i.e.
/// `FT_Load_Sfnt_Table`) sees them with HB_TINY.
fn hb_tiny_font(face: &FreeTypeFace) -> Option<Font> {
    let ft = face.get_face()?;
    let data = face.data().clone();
    // FT_Load_Sfnt_Table: the first non-empty directory entry per tag.
    let tables: HashMap<[u8; 4], (usize, usize)> = ft.table_entries().into_iter().collect();
    let table_fn = move |tag: Tag| -> Option<Blob> {
        let bytes = tag.to_be_bytes();
        if HB_TINY_HIDDEN_TABLES.iter().any(|t| **t == bytes) {
            return None;
        }
        let &(offset, length) = tables.get(&bytes)?;
        let range = offset..offset + length;
        let table = data.get(range.clone())?;
        if (&bytes == b"GSUB" || &bytes == b"GPOS") && table.len() >= 4 {
            // HB_NO_VAR: find_variations_index never finds one. Version 1.0
            // headers have no FeatureVariations offset.
            if table[0..2] == [0, 1] && table[2..4] != [0, 0] {
                let mut table = table.to_vec();
                table[2] = 0;
                table[3] = 0;
                return Some(Blob::from(table));
            }
        }
        let table: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(TableBytes { data: data.clone(), range });
        Some(Blob::from(table))
    };
    Font::new(TableFunction::new(Arc::new(table_fn)), 0)
}

impl Impl {
    /// `HBShaper::Impl::Impl`: `hb_ft_font_create` + `hb_buffer_create`.
    pub fn new(type_: GlyphIDType, font_file_data: impl Into<Arc<Vec<u8>>>, lib: &FreeTypeLibrary) -> Self {
        let face = FreeTypeFace::from_memory(font_file_data, lib);
        let font = if face.is_valid() { hb_tiny_font(&face) } else { None };
        Impl { type_, face, font, plans: Mutex::new(HashMap::new()) }
    }

    pub fn valid(&self) -> bool {
        self.face.is_valid()
    }

    pub fn rasterize_glyph(&self, glyph: &GlyphID) -> Glyph {
        self.face.rasterize_glyph(glyph)
    }

    /// The face, for the rasterizer and tests.
    pub fn face(&self) -> &FreeTypeFace {
        &self.face
    }

    fn plan(
        &self,
        font: &Font,
        script: [u8; 4],
        rtl: bool,
        language: &Language,
        features: &[Feature],
    ) -> Arc<ShapePlan> {
        let mut plans = self.plans.lock_unpoisoned();
        plans
            .entry((script, rtl))
            .or_insert_with(|| {
                Arc::new(ShapePlan::new(font, direction(rtl), Some(to_script(script)), Some(language), features))
            })
            .clone()
    }

    /// `HBShaper::Impl::createComplexGlyphIDs`.
    pub fn create_complex_glyph_ids(
        &self,
        text: &[u16],
        rtl: bool,
        glyph_ids: &mut Vec<GlyphID>,
        adjusts: &mut Vec<HBShapeAdjust>,
    ) {
        if text.is_empty() {
            return;
        }
        // The C++ never checks `face.isValid()` here (font and buffer are
        // uninitialized then); callers only shape with valid shapers.
        let Some(font) = &self.font else {
            return;
        };

        struct TextPart {
            text: Vec<u16>,
            script: [u8; 4],
        }

        let is_neutral = |script: &[u8; 4]| matches!(script, b"Zyyy" | b"Zinh" | b"Zzzz");
        // HB_SCRIPT_INVALID, for a part that holds no letters yet.
        const INVALID: [u8; 4] = [0; 4];

        let mut text_parts: Vec<TextPart> = Vec::new();
        if rtl {
            // A right-to-left run may hold more than one script, such as
            // Hebrew and Arabic. Each script is shaped on its own, so the
            // font applies that script's rules, such as Arabic joining.
            // Spaces, punctuation, and marks belong to the letters before
            // them, or to the first letters.
            for &ch in text {
                let script = get_unicode_script(ch as u32);
                let part_script = if is_neutral(&script) { INVALID } else { script };
                match text_parts.last_mut() {
                    Some(last) if part_script == INVALID || last.script == script || last.script == INVALID => {
                        if last.script == INVALID {
                            last.script = part_script;
                        }
                        last.text.push(ch);
                    }
                    _ => text_parts.push(TextPart { text: vec![ch], script: part_script }),
                }
            }
            for text_part in &mut text_parts {
                if text_part.script == INVALID {
                    text_part.script = *b"Arab";
                }
            }
            // HarfBuzz returns each part's glyphs in visual order, and the
            // run's later parts lie to the left of its earlier ones.
            text_parts.reverse();
        } else {
            text_parts.push(TextPart { text: vec![text[0]], script: get_unicode_script(text[0] as u32) });
            for &ch in &text[1..] {
                let script = get_unicode_script(ch as u32);
                let last = text_parts.last_mut().expect("non-empty");
                if last.script == script || script == *b"Zinh" {
                    last.text.push(ch);
                } else {
                    text_parts.push(TextPart { text: vec![ch], script });
                }
            }
        }

        for text_part in &text_parts {
            let Some(buffer) = self.shape_part(font, &text_part.text, text_part.script, rtl) else {
                continue;
            };

            // Get Harfbuzz adjustion
            let glyph_info = buffer.glyph_infos();
            let glyph_pos = buffer.glyph_positions();
            glyph_ids.reserve(glyph_info.len());
            adjusts.reserve(glyph_info.len());
            for (info, pos) in glyph_info.iter().zip(glyph_pos) {
                glyph_ids.push(GlyphID::with_type(info.glyph_id as u16, self.type_));

                let x_advance = pos.x_advance as f32 / 64.0;
                let x_offset = pos.x_offset as f32 / 64.0;
                let y_offset = pos.y_offset.wrapping_neg() as f32 / 64.0;

                adjusts.push(HBShapeAdjust::new(x_offset, y_offset, x_advance));
            }
        }
    }

    /// One iteration of `createComplexGlyphIDs`' loop: `hb_buffer_reset`,
    /// direction, script, language, `hb_buffer_add_utf16`, `hb_shape`.
    fn shape_part(&self, font: &Font, text: &[u16], script: [u8; 4], rtl: bool) -> Option<Buffer> {
        let funcs = HbFtFont::new(&self.face);
        let (x_scale, y_scale) = HbFtFont::scale(&self.face);
        let shaper_font = ShaperFont::new(font).with_scale_separate(x_scale, y_scale).with_font_funcs(Some(&funcs));
        let language = get_default_language();
        let features = hb_tiny_features();

        // Setup harfbuzz
        let mut buffer = Buffer::new();
        buffer.set_direction(direction(rtl));
        buffer.set_script(Some(to_script(script)));
        buffer.set_language(Some(language.clone()));
        add_utf16(&mut buffer, text);

        // harfbuzz shaping
        let plan = self.plan(font, script, rtl, &language, &features);
        let options = ShapeOptions::new().plan(Some(&plan)).features(&features);
        // hb_shape cannot fail on a valid buffer; HarfRust only fails on
        // plan/buffer mismatches, which cannot happen here.
        harfrust::shape(&shaper_font, &mut buffer, options).ok()?;
        Some(buffer)
    }

    /// `shape_part` left to right in HarfBuzz units, for tests: per glyph
    /// [glyph, cluster, x_advance, y_advance, x_offset, y_offset].
    pub fn shape_part_raw(&self, text: &[u16], script: [u8; 4]) -> Vec<[i32; 6]> {
        let Some(font) = &self.font else { return Vec::new() };
        let Some(buffer) = self.shape_part(font, text, script, false) else { return Vec::new() };
        buffer
            .glyph_infos()
            .iter()
            .zip(buffer.glyph_positions())
            .map(|(i, p)| [i.glyph_id as i32, i.cluster as i32, p.x_advance, p.y_advance, p.x_offset, p.y_offset])
            .collect()
    }
}

/// `rtl ? HB_DIRECTION_RTL : HB_DIRECTION_LTR`.
fn direction(rtl: bool) -> Direction {
    if rtl { Direction::RightToLeft } else { Direction::LeftToRight }
}

/// `hb_script_t` (a tag) as a HarfRust script.
fn to_script(tag: [u8; 4]) -> Script {
    Script::from_iso15924_tag(Tag::new(&tag)).unwrap_or(Script::UNKNOWN)
}

/// `hb_buffer_add_utf16 (buffer, text, length, 0, length)`: clusters are
/// UTF-16 offsets; unpaired surrogates become U+FFFD (the buffer's
/// replacement codepoint); no pre- or post-context.
fn add_utf16(buffer: &mut Buffer, text: &[u16]) {
    let mut i = 0;
    while i < text.len() {
        let cluster = i as u32;
        let c = text[i] as u32;
        i += 1;
        let u = if (0xD800..=0xDBFF).contains(&c) {
            match text.get(i) {
                Some(&l) if (0xDC00..=0xDFFF).contains(&(l as u32)) => {
                    i += 1;
                    (c << 10) + l as u32 - ((0xD800 << 10) - 0x10000 + 0xDC00)
                }
                _ => 0xFFFD,
            }
        } else if (0xDC00..=0xDFFF).contains(&c) {
            0xFFFD
        } else {
            c
        };
        buffer.push(u, cluster);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_decoding() {
        let mut b = Buffer::new();
        add_utf16(&mut b, &[0x41, 0xD83D, 0xDE00, 0xD800, 0x42, 0xDC00]);
        let cps: Vec<(u32, u32)> = b.glyph_infos().iter().map(|i| (i.glyph_id, i.cluster)).collect();
        assert_eq!(cps, vec![(0x41, 0), (0x1F600, 1), (0xFFFD, 3), (0x42, 4), (0xFFFD, 5)]);
    }

    #[test]
    fn script_segmentation_classifies_code_units() {
        assert_eq!(&get_unicode_script('a' as u32), b"Latn");
        assert_eq!(&get_unicode_script(' ' as u32), b"Zyyy");
        assert_eq!(&get_unicode_script(0x0301), b"Zinh");
        assert_eq!(&get_unicode_script(0x0941), b"Zinh"); // Devanagari vowel sign U (Mn)
        assert_eq!(&get_unicode_script(0x0627), b"Arab");
        assert_eq!(&get_unicode_script(0xD83D), b"Zzzz");
    }
}
