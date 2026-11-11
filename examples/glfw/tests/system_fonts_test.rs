// Port-only: the system fonts that mln-glfw gives the map as fallback font
// faces. Which fonts it finds depends on the machine, so the test checks
// each face it chose, and passes with none; `--nocapture` lists them.

use mln_glfw::system_fonts::fallback_font_faces;
use mln_text::glyph::GlyphIDType;
use skrifa::MetadataProvider;

#[test]
fn chosen_faces_load_and_cover_their_ranges() {
    let started = std::time::Instant::now();
    let faces = fallback_font_faces();
    println!("found {} fallback faces in {:?}", faces.len(), started.elapsed());
    for face in &faces {
        let (family, face) = (&face.family, &face.face);
        let bytes = face.data.as_ref().expect("a system face holds its font file").as_bytes();
        println!("{family}: {} ({} bytes) for {:04X?}", face.url, bytes.len(), face.ranges);
        assert!(face.name.is_empty());
        assert!(face.url.starts_with("file://"));
        assert_ne!(face.type_, GlyphIDType::FontPBF);
        let font = skrifa::FontRef::new(bytes).expect("a font");
        let covered = font.charmap().mappings().any(|(c, _)| face.ranges.iter().any(|&(lo, hi)| c >= lo && c <= hi));
        assert!(covered, "{family} has no glyph in its ranges");
    }
}
