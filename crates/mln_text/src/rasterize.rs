// Ports FreeTypeFace::rasterizeGlyph (src/mln/text/freetype.cpp) and the
// FreeType path it takes:
// - `FT_Load_Glyph(face, glyph, FT_LOAD_DEFAULT)` at 24 px: hinted with the
//   font's TrueType instructions or PostScript hints, or autohinted when a
//   TrueType font has no hinting programs. The hinted outline comes from
//   skrifa, whose hinting engines follow FreeType's (`HintOptions::
//   auto_fallback`, grayscale target); its paths carry FreeType's 26.6
//   coordinates and implied midpoints. The advance is FreeType's
//   `ft_glyphslot_grid_fit_metrics` rounding of the hinted advance.
// - `FT_Render_Glyph(slot, FT_RENDER_MODE_NORMAL)`: the bitmap box of
//   `ft_glyphslot_preset_bitmap` and the smooth rasterizer (`ft::grays`, a
//   port of ftgrays.c), oversampled for glyphs flagged as overlapping.
//
// TODO(port): embedded bitmap glyphs (CBDT/sbix/EBDT, e.g. color emoji
// fonts without outlines) give an empty glyph here.
// TODO(port): FT_Load_Glyph failures leave the previous glyph in FreeType's
// glyph slot, which the C++ then renders; here a glyph that cannot be loaded
// is empty.

use std::sync::Arc;

use mln_util::image::AlphaImage;
use mln_util::size::Size;
use skrifa::raw::model::glyph::outline::{DrawMemory, PathSink};
use skrifa::{GlyphId, HintOptions, ScalerFont};

use crate::freetype::{FreeTypeFace, SDF_FONT_SIZE};
use crate::ft::grays::{self, Segment};
use crate::glyph::{Glyph, GlyphID};

/// Collects a drawn path as FreeType outline segments in 26.6, y up, with
/// `FT_Outline_Decompose`'s closing line to each contour's start.
#[derive(Default)]
struct Collector {
    segments: Vec<Segment>,
    start: Option<(i64, i64)>,
    /// All points, control points included (for FT_Outline_Get_CBox).
    points: Vec<(i64, i64)>,
}

/// 26.6 from a skrifa coordinate (exact: skrifa keeps FreeType's 26.6).
fn f26dot6(v: f32) -> i64 {
    (v as f64 * 64.0).round() as i64
}

impl Collector {
    fn point(&mut self, x: f32, y: f32) -> (i64, i64) {
        let p = (f26dot6(x), f26dot6(y));
        self.points.push(p);
        p
    }
    fn close_contour(&mut self) {
        if let Some((x, y)) = self.start.take() {
            self.segments.push(Segment::LineTo(x, y));
        }
    }
}

impl PathSink for Collector {
    fn move_to(&mut self, x: f32, y: f32) {
        self.close_contour();
        let (x, y) = self.point(x, y);
        self.start = Some((x, y));
        self.segments.push(Segment::MoveTo(x, y));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.point(x, y);
        self.segments.push(Segment::LineTo(x, y));
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let (cx, cy) = self.point(cx0, cy0);
        let (x, y) = self.point(x, y);
        self.segments.push(Segment::ConicTo(cx, cy, x, y));
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let (ax, ay) = self.point(cx0, cy0);
        let (bx, by) = self.point(cx1, cy1);
        let (x, y) = self.point(x, y);
        self.segments.push(Segment::CubicTo(ax, ay, bx, by, x, y));
    }
    fn close(&mut self) {
        self.close_contour();
    }
}

/// The hinting scaler of a face at `SDF_FONT_SIZE` (FT_LOAD_DEFAULT), on
/// the face's own font data.
pub(crate) fn new_scaler(data: &Arc<Vec<u8>>) -> Option<ScalerFont> {
    let data: Arc<dyn AsRef<[u8]> + Send + Sync> = data.clone();
    let font = skrifa::Font::new(data, 0)?;
    Some(ScalerFont::hinted(font, SDF_FONT_SIZE as f32, HintOptions::auto_fallback()))
}

/// `FT_PIX_ROUND`.
fn pix_round(x: i64) -> i64 {
    (x + 32) & !63
}

/// `FreeTypeFace::rasterizeGlyph`.
pub fn rasterize_glyph(face: &FreeTypeFace, glyph: &GlyphID) -> Glyph {
    let border = Glyph::BORDER_SIZE as u32;
    let mut fixed_metrics = Glyph { id: *glyph, ..Glyph::default() };

    // FT_Load_Glyph(face, glyph.complex.code, FT_LOAD_DEFAULT)
    let mut collector = Collector::default();
    let mut advance: i64 = 0;
    let mut overlap = false;
    if let Some(outline) = face.scaler().and_then(|s| s.outline(GlyphId::new(glyph.code as u32))) {
        if let Ok(metrics) = outline.draw(DrawMemory::Scratch, &mut collector) {
            collector.close_contour();
            advance = pix_round(metrics.h_advance.map_or(0, f26dot6));
            overlap = metrics.has_overlaps;
        } else {
            collector = Collector::default();
        }
    }

    // FT_Render_Glyph(slot, FT_RENDER_MODE_NORMAL): ft_glyphslot_preset_bitmap
    let (mut x_min, mut y_min, mut x_max, mut y_max) = (0i64, 0i64, 0i64, 0i64);
    if let Some(&(x, y)) = collector.points.first() {
        (x_min, y_min, x_max, y_max) = (x, y, x, y);
        for &(x, y) in &collector.points {
            x_min = x_min.min(x);
            y_min = y_min.min(y);
            x_max = x_max.max(x);
            y_max = y_max.max(y);
        }
    }
    let pbox_x_min = (x_min >> 6) + ((x_min & 63) >> 6);
    let pbox_y_min = (y_min >> 6) + ((y_min & 63) >> 6);
    let pbox_x_max = (x_max >> 6) + (((x_max & 63) + 63) >> 6);
    let pbox_y_max = (y_max >> 6) + (((y_max & 63) + 63) >> 6);
    let bitmap_left = pbox_x_min;
    let bitmap_top = pbox_y_max;
    let mut width = (pbox_x_max - pbox_x_min) as u32;
    let mut rows = (pbox_y_max - pbox_y_min) as u32;
    let overflow = pbox_x_min < -0x8000 || pbox_x_max > 0x7FFF || pbox_y_min < -0x8000 || pbox_y_max > 0x7FFF;

    let buffer = if overflow || collector.segments.is_empty() {
        if overflow {
            // Raster_Overflow: nothing is rendered.
            // TODO(port): check what the glyph slot holds then.
            width = 0;
            rows = 0;
        }
        vec![0u8; (width * rows) as usize]
    } else {
        // Translate the outline into the bitmap.
        let x_shift = 64 * -bitmap_left;
        let y_shift = 64 * -bitmap_top + 64 * rows as i64;
        let translated: Vec<Segment> = collector
            .segments
            .iter()
            .map(|s| match *s {
                Segment::MoveTo(x, y) => Segment::MoveTo(x + x_shift, y + y_shift),
                Segment::LineTo(x, y) => Segment::LineTo(x + x_shift, y + y_shift),
                Segment::ConicTo(a, b, x, y) => Segment::ConicTo(a + x_shift, b + y_shift, x + x_shift, y + y_shift),
                Segment::CubicTo(a, b, c, d, x, y) => {
                    Segment::CubicTo(a + x_shift, b + y_shift, c + x_shift, d + y_shift, x + x_shift, y + y_shift)
                }
            })
            .collect();
        grays::render(&translated, width, rows, overlap)
    };

    let size = Size::new(width + border * 2, rows + border * 2);
    fixed_metrics.metrics.width = size.width;
    fixed_metrics.metrics.height = size.height;
    fixed_metrics.metrics.left = bitmap_left as i32 as f32;
    fixed_metrics.metrics.top = (bitmap_top as i32 - SDF_FONT_SIZE as i32 - border as i32) as f32;
    fixed_metrics.metrics.advance = (advance / 64) as u32;

    // Copy alpha values from the bitmap into the AlphaImage output.
    let mut bitmap = AlphaImage::new(size);
    for h in 0..rows {
        let dst = (size.width * (h + border) + border) as usize;
        let src = (width * h) as usize;
        bitmap.data[dst..dst + width as usize].copy_from_slice(&buffer[src..src + width as usize]);
    }
    fixed_metrics.bitmap = bitmap;
    fixed_metrics
}
