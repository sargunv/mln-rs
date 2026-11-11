// Port-only: system fonts as the map's fallback font faces, for the scripts
// that glyph PBFs can't shape (i18n::char_in_supported_script: the Indic
// scripts, Sinhala, Tibetan, Myanmar and Khmer). For each script it picks
// one installed face, by family name, that has the script's glyphs, and
// gives the map that face for the script's code points only.
//
// fontdb finds the fonts and reads their names and coverage through
// short-lived memory maps. A face's bytes load the first time the map needs
// it (`LazyFace`), mapped by fontdb again and copied out: fontdb's lasting
// map, `make_shared_face_data`, is an `unsafe fn`, and this crate has no
// `unsafe` outside FFI.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use mln_text::glyph::{FontData, FontFace, FontFaceRange, FontFaces};
use skrifa::MetadataProvider;

/// A script that needs a shaped font: the families that draw it on Linux,
/// macOS and Windows, in order of preference, the words of other families
/// that do, a letter of it, and its Unicode blocks.
struct Script {
    families: &'static [&'static str],
    keywords: &'static [&'static str],
    probe: char,
    ranges: &'static [FontFaceRange],
}

const SCRIPTS: &[Script] = &[
    Script {
        families: &["Noto Sans Devanagari", "Kohinoor Devanagari", "Devanagari Sangam MN", "Nirmala UI", "Mangal"],
        keywords: &["Devanagari"],
        probe: '\u{0915}',
        ranges: &[(0x0900, 0x097F), (0xA8E0, 0xA8FF)],
    },
    Script {
        families: &["Noto Sans Bengali", "Kohinoor Bangla", "Bangla Sangam MN", "Nirmala UI", "Vrinda"],
        keywords: &["Bengali", "Bangla"],
        probe: '\u{0995}',
        ranges: &[(0x0980, 0x09FF)],
    },
    Script {
        families: &["Noto Sans Gurmukhi", "Gurmukhi Sangam MN", "Gurmukhi MN", "Nirmala UI", "Raavi"],
        keywords: &["Gurmukhi"],
        probe: '\u{0A15}',
        ranges: &[(0x0A00, 0x0A7F)],
    },
    Script {
        families: &["Noto Sans Gujarati", "Kohinoor Gujarati", "Gujarati Sangam MN", "Nirmala UI", "Shruti"],
        keywords: &["Gujarati"],
        probe: '\u{0A95}',
        ranges: &[(0x0A80, 0x0AFF)],
    },
    Script {
        families: &["Noto Sans Oriya", "Oriya Sangam MN", "Nirmala UI", "Kalinga"],
        keywords: &["Oriya", "Odia"],
        probe: '\u{0B15}',
        ranges: &[(0x0B00, 0x0B7F)],
    },
    Script {
        families: &["Noto Sans Tamil", "Tamil Sangam MN", "Nirmala UI", "Latha"],
        keywords: &["Tamil"],
        probe: '\u{0B95}',
        ranges: &[(0x0B80, 0x0BFF)],
    },
    Script {
        families: &["Noto Sans Telugu", "Kohinoor Telugu", "Telugu Sangam MN", "Nirmala UI", "Gautami"],
        keywords: &["Telugu"],
        probe: '\u{0C15}',
        ranges: &[(0x0C00, 0x0C7F)],
    },
    Script {
        families: &["Noto Sans Kannada", "Kannada Sangam MN", "Nirmala UI", "Tunga"],
        keywords: &["Kannada"],
        probe: '\u{0C95}',
        ranges: &[(0x0C80, 0x0CFF)],
    },
    Script {
        families: &["Noto Sans Malayalam", "Malayalam Sangam MN", "Nirmala UI", "Kartika"],
        keywords: &["Malayalam"],
        probe: '\u{0D15}',
        ranges: &[(0x0D00, 0x0D7F)],
    },
    Script {
        families: &["Noto Sans Sinhala", "Sinhala Sangam MN", "Nirmala UI", "Iskoola Pota"],
        keywords: &["Sinhala"],
        probe: '\u{0D9A}',
        ranges: &[(0x0D80, 0x0DFF)],
    },
    Script {
        families: &["Noto Serif Tibetan", "Kailasa", "Microsoft Himalaya"],
        keywords: &["Tibetan"],
        probe: '\u{0F40}',
        ranges: &[(0x0F00, 0x0FFF)],
    },
    Script {
        families: &["Noto Sans Myanmar", "Myanmar Sangam MN", "Myanmar MN", "Myanmar Text", "Padauk"],
        keywords: &["Myanmar"],
        probe: '\u{1000}',
        ranges: &[(0x1000, 0x109F), (0xA9E0, 0xA9FF), (0xAA60, 0xAA7F)],
    },
    Script {
        families: &["Noto Sans Khmer", "Khmer Sangam MN", "Khmer MN", "Khmer UI", "Leelawadee UI"],
        keywords: &["Khmer"],
        probe: '\u{1780}',
        ranges: &[(0x1780, 0x17FF), (0x19E0, 0x19FF)],
    },
];

/// A system font face chosen as a fallback.
pub struct SystemFontFace {
    /// Its family name.
    pub family: String,
    pub face: FontFace,
}

/// The fallback font faces for the scripts that glyph PBFs can't shape,
/// from the fonts installed on this system: at most one per script.
pub fn fallback_font_faces() -> Vec<SystemFontFace> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    // The chosen faces, in a database of their own, which the faces' bytes
    // load from.
    let mut chosen = fontdb::Database::new();
    let mut chosen_ids: HashMap<fontdb::ID, fontdb::ID> = HashMap::new();
    let mut picks = Vec::new();
    for script in SCRIPTS {
        let Some(info) = pick_face(&db, script) else {
            continue;
        };
        let id = *chosen_ids.entry(info.id).or_insert_with(|| chosen.push_face_info(info.clone()));
        let family = info.families.first().map(|(name, _)| name.clone()).unwrap_or_default();
        picks.push((script, id, family, face_url(info)));
    }

    let chosen = Arc::new(chosen);
    let mut data: HashMap<fontdb::ID, FontData> = HashMap::new();
    picks
        .into_iter()
        .map(|(script, id, family, url)| {
            let bytes = data
                .entry(id)
                .or_insert_with(|| FontData::new(Arc::new(LazyFace { db: chosen.clone(), id, bytes: OnceLock::new() })))
                .clone();
            let face = FontFace::fallback(&url, script.ranges.to_vec()).with_data(bytes);
            SystemFontFace { family, face }
        })
        .collect()
}

/// The faces only, for `Style::set_fallback_font_face_list`.
pub fn into_font_faces(faces: Vec<SystemFontFace>) -> FontFaces {
    faces.into_iter().map(|face| face.face).collect()
}

/// The face of `db` that draws `script` best: the first of its preferred
/// families, else a family named for it, upright and closest to regular
/// weight, that has a glyph for its letter. Only the first face of a font
/// collection counts, the one the map's font loader reads.
fn pick_face<'a>(db: &'a fontdb::Database, script: &Script) -> Option<&'a fontdb::FaceInfo> {
    let rank = |info: &fontdb::FaceInfo| -> Option<usize> {
        info.families.iter().find_map(|(family, _)| {
            script.families.iter().position(|preferred| preferred.eq_ignore_ascii_case(family)).or_else(|| {
                script.keywords.iter().any(|keyword| family.contains(keyword)).then_some(script.families.len())
            })
        })
    };
    let mut candidates: Vec<(usize, bool, u16, &fontdb::FaceInfo)> = db
        .faces()
        .filter(|info| info.index == 0)
        .filter_map(|info| {
            let rank = rank(info)?;
            let slanted = info.style != fontdb::Style::Normal;
            Some((rank, slanted, info.weight.0.abs_diff(fontdb::Weight::NORMAL.0), info))
        })
        .collect();
    candidates.sort_by_key(|&(rank, slanted, weight, _)| (rank, slanted, weight));
    candidates.into_iter().map(|(.., info)| info).find(|info| covers(db, info.id, script.probe))
}

/// Whether the face has a glyph for `c`.
fn covers(db: &fontdb::Database, id: fontdb::ID, c: char) -> bool {
    db.with_face_data(id, |data, index| {
        skrifa::FontRef::from_index(data, index).is_ok_and(|font| font.charmap().map(c).is_some())
    })
    .unwrap_or(false)
}

/// A url that names the face's font file.
fn face_url(info: &fontdb::FaceInfo) -> String {
    match &info.source {
        fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => format!("file://{}", path.display()),
        fontdb::Source::Binary(_) => format!("font://{}", info.post_script_name),
    }
}

/// A face's font file, read the first time the map needs it.
struct LazyFace {
    db: Arc<fontdb::Database>,
    id: fontdb::ID,
    bytes: OnceLock<Vec<u8>>,
}

impl AsRef<[u8]> for LazyFace {
    fn as_ref(&self) -> &[u8] {
        self.bytes.get_or_init(|| self.db.with_face_data(self.id, |data, _| data.to_vec()).unwrap_or_default())
    }
}
