// Ports test/style/style_parser.test.cpp.

use std::path::PathBuf;

use mln_util::enum_::Enum;
use mln_util::logging::{Event, EventSeverity, Log};
use mln_vec_map::VecSet;

use super::util::FixtureLog;
use crate::layer::FontStack;
use crate::parser::Parser;

fn fixtures_dir() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/test/fixtures/style_parser")
}

/// `populateNames`: every `<name>.info.json` in the fixture directory.
fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(fixtures_dir())
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".info.json").map(str::to_owned)
        })
        .collect();
    names.sort();
    names
}

/// One `StyleParserTest.ParseStyle` instance. Returns the failure, if any.
fn parse_style_fixture(name: &str) -> Result<(), String> {
    let base = fixtures_dir().join(name);
    let log = FixtureLog::new();

    let info_doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(base.with_extension("info.json")).unwrap()).unwrap();
    let style_json = std::fs::read_to_string(base.with_extension("style.json")).unwrap();

    let mut parser = Parser::default();
    if let Err(error) = parser.parse(&style_json) {
        Log::error(Event::ParseStyle, format!("Failed to parse style: {error}"));
    }

    for (property_name, value) in info_doc.as_object().unwrap() {
        if let Some(js_log) = value.get("log") {
            for js_entry in js_log.as_array().unwrap() {
                let js_entry = js_entry.as_array().unwrap();
                let count = js_entry[0].as_u64().unwrap() as usize;
                let severity = EventSeverity::to_enum(js_entry[1].as_str().unwrap()).unwrap();
                let event = Event::to_enum(js_entry[2].as_str().unwrap()).unwrap();
                let msg = js_entry[3].as_str().unwrap();
                let observed = log.count(severity, event, msg);
                if count != observed {
                    return Err(format!("{name}/{property_name}: Message: {msg}: expected {count}, got {observed}"));
                }
            }
        }
        let unchecked = log.unchecked();
        if !unchecked.is_empty() {
            return Err(format!("Unchecked Log Messages ({name}/{property_name}): {unchecked:?}"));
        }
    }
    Ok(())
}

/// TEST_P(StyleParserTest, ParseStyle), instantiated over every fixture.
// gtest: StyleParserTest.ParseStyle
#[test]
fn parse_style() {
    let names = fixture_names();
    assert!(!names.is_empty());
    let mut failures = Vec::new();
    for name in &names {
        if let Err(failure) = parse_style_fixture(name) {
            failures.push(failure);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// gtest: StyleParser.SpriteAsString
#[test]
fn sprite_as_string() {
    let mut parser = Parser::default();
    parser
        .parse(
            r#"{
        "version": 8,
        "sprite": "https://example.com/default/markers"
    }"#,
        )
        .unwrap();
    let result = &parser.sprites;
    assert_eq!(1, result.len());
    assert_eq!("https://example.com/default/markers", result[0].sprite_url);
    assert_eq!("default", result[0].id);
}

// gtest: StyleParser.SpriteAsArrayEmpty
#[test]
fn sprite_as_array_empty() {
    let mut parser = Parser::default();
    parser
        .parse(
            r#"{
        "version": 8,
        "sprite": []
    }"#,
        )
        .unwrap();
    assert_eq!(0, parser.sprites.len());
}

// gtest: StyleParser.SpriteAsArraySingle
#[test]
fn sprite_as_array_single() {
    let mut parser = Parser::default();
    parser
        .parse(
            r#"{
        "version": 8,
        "sprite": [{
            "id": "default",
            "url": "https://example.com/default/markers"
        }]
    }"#,
        )
        .unwrap();
    let result = &parser.sprites;
    assert_eq!(1, result.len());
    assert_eq!("https://example.com/default/markers", result[0].sprite_url);
    assert_eq!("default", result[0].id);
}

// gtest: StyleParser.SpriteAsArrayMultiple
#[test]
fn sprite_as_array_multiple() {
    let mut parser = Parser::default();
    parser
        .parse(
            r#"{
        "version": 8,
        "sprite": [{
            "id": "default",
            "url": "https://example.com/default/markers"
        },{
            "id": "hiking",
            "url": "https://example.com/hiking/markers"
        }]
    }"#,
        )
        .unwrap();
    let result = &parser.sprites;
    assert_eq!(2, result.len());
    assert_eq!("https://example.com/default/markers", result[0].sprite_url);
    assert_eq!("default", result[0].id);
    assert_eq!("https://example.com/hiking/markers", result[1].sprite_url);
    assert_eq!("hiking", result[1].id);
}

// gtest: StyleParser.FontStacks
#[test]
fn font_stacks() {
    let mut parser = Parser::default();
    let _ = parser.parse(&std::fs::read_to_string(fixtures_dir().join("font_stacks.json")).unwrap());
    let expected: VecSet<FontStack> = [vec!["a"], vec!["a", "b"], vec!["a", "b", "c"]]
        .into_iter()
        .map(|v| v.into_iter().map(str::to_owned).collect())
        .collect();
    let result = parser.font_stacks();
    assert_eq!(expected, result);
}

fn symbol_font_stacks(layout: &str) -> VecSet<FontStack> {
    let mut parser = Parser::default();
    let _ = parser.parse(&format!(
        r#"{{
        "version": 8,
        "layers": [{{
            "id": "symbol",
            "type": "symbol",
            "source": "vector",
            "layout": {layout}
        }}]
    }}"#
    ));
    parser.font_stacks()
}

fn arial_helvetica() -> VecSet<FontStack> {
    [vec!["Arial".to_owned()], vec!["Helvetica".to_owned()]].into_iter().collect()
}

// gtest: StyleParser.FontStacksNoTextField
#[test]
fn font_stacks_no_text_field() {
    let result = symbol_font_stacks(r#"{"text-font": ["a"]}"#);
    assert_eq!(0, result.len());
}

// gtest: StyleParser.FontStacksCaseExpression
#[test]
fn font_stacks_case_expression() {
    let result = symbol_font_stacks(
        r#"{
                "text-field": "a",
                "text-font": ["case", ["==", "a", ["string", ["get", "text-font"]]], ["literal", ["Arial"]], ["literal", ["Helvetica"]]]
            }"#,
    );
    assert_eq!(arial_helvetica(), result);
}

// gtest: StyleParser.FontStacksMatchExpression
#[test]
fn font_stacks_match_expression() {
    let result = symbol_font_stacks(
        r#"{
                "text-field": "a",
                "text-font": ["match", ["get", "text-font"], "a", ["literal", ["Arial"]], ["literal", ["Helvetica"]]]
            }"#,
    );
    assert_eq!(arial_helvetica(), result);
}

// gtest: StyleParser.FontStacksStepExpression
#[test]
fn font_stacks_step_expression() {
    let result = symbol_font_stacks(
        r#"{
                "text-field": "a",
                "text-font": ["array", "string", ["step", ["get", "text-font"], ["literal", ["Arial"]], 0, ["literal", ["Helvetica"]]]]
            }"#,
    );
    assert_eq!(arial_helvetica(), result);
}

// gtest: StyleParser.FontStacksGetExpression
#[test]
fn font_stacks_get_expression() {
    // Invalid style, but not currently validated.
    let result = symbol_font_stacks(
        r#"{
                "text-field": "a",
                "text-font": ["array", "string", ["get", "text-font"]]
            }"#,
    );
    assert_eq!(0, result.len());
}

// gtest: StyleParser.ZoomCurve
#[test]
fn zoom_curve() {
    use mln_expression::value::Value;

    use crate::expression::dsl::{interpolate, linear, literal, zoom};
    use crate::expression::find_zoom_curve_checked;

    let zoom_interp =
        || interpolate(linear(), zoom(), vec![(0.0, literal(Value::Number(0.0))), (0.0, literal(Value::Number(0.0)))]);

    let expr1 = interpolate(linear(), literal(Value::Number(0.0)), vec![(0.0, zoom_interp()), (0.0, zoom_interp())]);
    assert!(find_zoom_curve_checked(expr1.as_ref()).is_null());

    let expr2 = interpolate(linear(), zoom(), vec![(0.0, literal(Value::Number(0.0))), (0.0, zoom_interp())]);
    assert!(find_zoom_curve_checked(expr2.as_ref()).is_null());
}

// Port-only: C++ cuts `unicode-range` strings by bytes (`substr(2)`), also
// inside a multibyte character; the range is then invalid, not a panic.
#[test]
fn font_face_unicode_range_with_multibyte_character() {
    let mut parser = Parser::default();
    parser
        .parse(
            r#"{
        "version": 8,
        "font-faces": {
            "Face": [{ "url": "face.ttf", "unicode-range": ["Ué0-FF", "U+0-FF"] }]
        }
    }"#,
        )
        .unwrap();
    let faces = parser.font_faces.unwrap();
    assert_eq!(faces.len(), 1);
    assert_eq!(faces[0].ranges, vec![(0, 0xFF)]);
}

// gtest: StyleParser.FallbackFontFaces
#[test]
fn fallback_font_faces() {
    use mln_text::glyph::GlyphIDType;

    use crate::parser::parse_fallback_font_faces;

    let faces = parse_fallback_font_faces(
        r#"[{"url": "a.ttf", "unicode-range": ["U+1000-109F", "U+A9E0-A9FF"]}, {"url": "b.ttf"}]"#,
    )
    .unwrap();
    assert_eq!(faces.len(), 2);
    assert_eq!(faces[0].name, "");
    assert_eq!(faces[0].url, "a.ttf");
    assert_eq!(faces[0].ranges, vec![(0x1000, 0x109F), (0xA9E0, 0xA9FF)]);
    assert_eq!(faces[1].ranges, vec![(0, 0x10FFFF)]);
    assert_ne!(faces[0].type_, GlyphIDType::FontPBF);

    assert!(parse_fallback_font_faces("{}").is_err());
    assert!(parse_fallback_font_faces("[1]").is_err());
    assert!(parse_fallback_font_faces(r#"[{"unicode-range": ["U+0-FF"]}]"#).is_err());
    assert!(parse_fallback_font_faces(r#"[{"url": "a.ttf", "unicode-range": ["U+XYZ-FF"]}]"#).is_err());
}
