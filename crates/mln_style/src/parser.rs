// Ports src/mln/style/parser.hpp + src/mln/style/parser.cpp.
//
// C++ parses with rapidjson; the port parses with serde_json (with
// `preserve_order`, so members iterate in document order as in rapidjson).
// Deviation: the message of a JSON syntax error is serde_json's, not
// rapidjson's `GetParseError_En(code) + " at offset " + offset`.

use std::collections::{HashMap, HashSet};

use mln_conversion::{Convertible, Error};
use mln_util::LatLng;
use mln_util::constants::DEFAULT_TRANSITION_DURATION;
use mln_util::logging::{Event, Log};
use mln_vec_map::VecSet;

use crate::conversion::layer::{convert_layer, set_paint_properties};
use crate::conversion::{Converter, convert, convert_source};
use crate::expression::GlobalStateMap;
use crate::layer::{FontStack, Layer};
use crate::light::Light;
use crate::source::Source;
use crate::sprite::Sprite;
use crate::transition_options::TransitionOptions;

/// What `Parser::parse` returns: a style that isn't one fails with
/// [`ErrorKind::InvalidArgument`](mln_util::ErrorKind).
pub type StyleParseResult = Result<(), mln_util::Error>;

pub struct Parser {
    pub sprites: Vec<Sprite>,
    pub glyph_url: String,
    /// `std::shared_ptr<FontFaces> fontFaces` (symbols: MLN_TEXT_SHAPING_HARFBUZZ).
    pub font_faces: Option<std::sync::Arc<mln_text::glyph::FontFaces>>,
    pub sources: Vec<Box<dyn Source>>,
    pub layers: Vec<Box<dyn Layer>>,

    pub transition: TransitionOptions,
    pub light: Light,

    /// Default values of the root "state" property, keyed by state property
    /// name, used by the "global-state" expression.
    pub global_state_defaults: GlobalStateMap,

    pub name: String,
    pub lat_lng: LatLng,
    pub zoom: f64,
    pub bearing: f64,
    pub pitch: f64,
    pub roll: f64,
    pub center_altitude: f64,
}

impl Default for Parser {
    fn default() -> Self {
        Parser {
            sprites: Vec::new(),
            glyph_url: String::new(),
            font_faces: None,
            sources: Vec::new(),
            layers: Vec::new(),
            transition: TransitionOptions::with_duration(DEFAULT_TRANSITION_DURATION),
            light: Light::new(),
            global_state_defaults: GlobalStateMap::new(),
            name: String::new(),
            lat_lng: LatLng::default(),
            zoom: 0.0,
            bearing: 0.0,
            pitch: 0.0,
            roll: 0.0,
            center_altitude: 0.0,
        }
    }
}

fn is_number(value: &serde_json::Value) -> bool {
    value.is_number()
}

fn get_double(value: &serde_json::Value) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

impl Parser {
    pub fn parse(&mut self, json: &str) -> StyleParseResult {
        let document: serde_json::Value = match serde_json::from_str(json) {
            Ok(document) => document,
            Err(error) => return Err(mln_util::Error::invalid_argument(format_json_parse_error(&error))),
        };

        let Some(object) = document.as_object() else {
            return Err(mln_util::Error::invalid_argument("style must be an object"));
        };

        if let Some(version_value) = object.get("version") {
            // rapidjson GetInt() on a number.
            let version = if is_number(version_value) {
                version_value.as_i64().unwrap_or_else(|| get_double(version_value) as i64)
            } else {
                0
            };
            if version != 8 {
                Log::warning(
                    Event::ParseStyle,
                    "current renderer implementation only supports style spec version 8; using an outdated style will cause rendering errors",
                );
            }
        }

        if let Some(value) = object.get("name") {
            if let Some(name) = value.as_str() {
                self.name = name.to_owned();
            }
        }

        if let Some(value) = object.get("center") {
            let mut error = Error::default();
            match convert::<LatLng>(&Convertible::from(value), &mut error) {
                Some(lat_lng) => self.lat_lng = lat_lng,
                None => Log::warning(Event::ParseStyle, "center coordinate must be a longitude, latitude pair"),
            }
        }

        if let Some(value) = object.get("centerAltitude") {
            if is_number(value) {
                self.center_altitude = get_double(value);
            }
        }

        if let Some(value) = object.get("zoom") {
            if is_number(value) {
                self.zoom = get_double(value);
            }
        }

        if let Some(value) = object.get("bearing") {
            if is_number(value) {
                self.bearing = get_double(value);
            }
        }

        if let Some(value) = object.get("pitch") {
            if is_number(value) {
                self.pitch = get_double(value);
            }
        }

        if let Some(value) = object.get("roll") {
            if is_number(value) {
                self.roll = get_double(value);
            }
        }

        if let Some(value) = object.get("transition") {
            self.parse_transition(value);
        }

        if let Some(value) = object.get("light") {
            self.parse_light(value);
        }

        if let Some(value) = object.get("state") {
            self.parse_state(value);
        }

        if let Some(value) = object.get("sources") {
            self.parse_sources(value);
        }

        if let Some(value) = object.get("layers") {
            self.parse_layers(value);
        }

        if let Some(value) = object.get("sprite") {
            self.parse_sprites(value);
        }

        if let Some(glyphs) = object.get("glyphs") {
            if let Some(glyphs) = glyphs.as_str() {
                self.glyph_url = glyphs.to_owned();
            }
        }

        // symbols: "font-faces" (MLN_TEXT_SHAPING_HARFBUZZ).
        if let Some(faces) = object.get("font-faces") {
            if let Some(faces) = faces.as_object() {
                self.font_faces = Some(std::sync::Arc::new(parse_font_faces(faces)));
            }
        }

        // Call for side effect of logging warnings for invalid values.
        self.font_stacks();

        Ok(())
    }

    fn parse_transition(&mut self, value: &serde_json::Value) {
        let mut error = Error::default();
        let Some(converted) = convert::<TransitionOptions>(&Convertible::from(value), &mut error) else {
            Log::warning(Event::ParseStyle, &error.message);
            return;
        };

        self.transition = converted;
    }

    fn parse_light(&mut self, value: &serde_json::Value) {
        let mut error = Error::default();
        let Some(converted) = <Light as Converter>::convert(&Convertible::from(value), &mut error) else {
            Log::warning(Event::ParseStyle, &error.message);
            return;
        };

        self.light = converted;
    }

    fn parse_state(&mut self, value: &serde_json::Value) {
        let Some(object) = value.as_object() else {
            Log::warning(Event::ParseStyle, "state must be an object");
            return;
        };

        for (property_name, member) in object {
            let Some(member_object) = member.as_object() else {
                Log::warning(Event::ParseStyle, format!("state property \"{property_name}\" must be an object"));
                continue;
            };
            let Some(default) = member_object.get("default") else {
                continue;
            };
            // emplace: the first definition wins.
            self.global_state_defaults
                .entry(property_name.clone())
                .or_insert_with(|| mln_conversion::json_to_value(default));
        }
    }

    fn parse_sources(&mut self, value: &serde_json::Value) {
        let Some(object) = value.as_object() else {
            Log::warning(Event::ParseStyle, "sources must be an object");
            return;
        };

        for (id, property) in object {
            let mut error = Error::default();
            let Some(source) = convert_source(&Convertible::from(property), &mut error, id) else {
                Log::warning(Event::ParseStyle, &error.message);
                continue;
            };

            self.sources.push(source);
        }
    }

    fn parse_sprites(&mut self, value: &serde_json::Value) {
        if let Some(url) = value.as_str() {
            let sprite = Sprite::new("default", url);
            self.sprites.push(sprite);
        } else if let Some(array) = value.as_array() {
            let mut sprite_ids = HashSet::new();
            for sprite_value in array {
                if !sprite_value.is_object() {
                    Log::warning(Event::ParseStyle, "sprite child must be an object");
                    continue;
                }

                let mut error = Error::default();
                let Some(sprite) = convert::<Sprite>(&Convertible::from(sprite_value), &mut error) else {
                    Log::warning(Event::ParseStyle, &error.message);
                    continue;
                };

                if sprite_ids.contains(&sprite.id) {
                    Log::warning(Event::ParseStyle, "sprite ids must be unique");
                    continue;
                }

                sprite_ids.insert(sprite.id.clone());
                self.sprites.push(sprite);
            }
        } else {
            Log::warning(Event::ParseStyle, "sprite must be an object or string");
        }
    }

    fn parse_layers(&mut self, value: &serde_json::Value) {
        let mut ids: Vec<String> = Vec::new();

        let Some(array) = value.as_array() else {
            Log::warning(Event::ParseStyle, "layers must be an array");
            return;
        };

        let mut layers_map: HashMap<String, LayerEntry<'_>> = HashMap::new();

        for layer_value in array {
            let Some(layer_object) = layer_value.as_object() else {
                Log::warning(Event::ParseStyle, "layer must be an object");
                continue;
            };

            let Some(id) = layer_object.get("id") else {
                Log::warning(Event::ParseStyle, "layer must have an id");
                continue;
            };

            let Some(id) = id.as_str() else {
                Log::warning(Event::ParseStyle, "layer id must be a string");
                continue;
            };

            let layer_id = id.to_owned();
            if layers_map.contains_key(&layer_id) {
                Log::warning(Event::ParseStyle, format!("duplicate layer id {layer_id}"));
                continue;
            }

            layers_map.insert(layer_id.clone(), LayerEntry { value: layer_value, layer: None });
            ids.push(layer_id);
        }

        // Store a stack of layer IDs we're parsing right now. This is to
        // prevent reference cycles.
        let mut stack: Vec<String> = Vec::new();

        for id in &ids {
            if layers_map.contains_key(id) {
                parse_layer(id, &mut layers_map, &mut stack);
            }
        }

        for id in &ids {
            if let Some(entry) = layers_map.get_mut(id) {
                if let Some(layer) = entry.layer.take() {
                    self.layers.push(layer);
                }
            }
        }
    }

    /// Statically evaluate layer properties to determine what font stacks are used.
    pub fn font_stacks(&self) -> VecSet<FontStack> {
        // mln::fontStacks(impls) (src/mln/util/font_stack.cpp).
        let mut result = VecSet::new();
        for layer in &self.layers {
            layer.base_impl().populate_font_stack(&mut result);
        }
        result
    }
}

struct LayerEntry<'a> {
    value: &'a serde_json::Value,
    layer: Option<Box<dyn Layer>>,
}

/// `Parser::parseLayer(id, value, layer)`.
fn parse_layer(id: &str, layers_map: &mut HashMap<String, LayerEntry<'_>>, stack: &mut Vec<String>) {
    let Some(entry) = layers_map.get(id) else {
        return;
    };
    if entry.layer.is_some() {
        // Skip parsing this again. We already have a valid layer definition.
        return;
    }
    let value = entry.value;

    // Make sure we have not previously attempted to parse this layer.
    if stack.iter().any(|s| s == id) {
        Log::warning(Event::ParseStyle, format!("layer reference of '{id}' is circular"));
        return;
    }

    if let Some(ref_val) = value.get("ref") {
        // This layer is referencing another layer. Recursively parse that layer.
        let Some(ref_) = ref_val.as_str() else {
            Log::warning(Event::ParseStyle, format!("layer ref of '{id}' must be a string"));
            return;
        };

        if !layers_map.contains_key(ref_) {
            Log::warning(Event::ParseStyle, format!("layer '{id}' references unknown layer {ref_}"));
            return;
        }

        // Recursively parse the referenced layer.
        stack.insert(0, id.to_owned());
        parse_layer(ref_, layers_map, stack);
        stack.remove(0);

        let Some(reference) = layers_map.get(ref_).and_then(|e| e.layer.as_ref()) else {
            return;
        };

        let mut layer = reference.clone_ref(id);
        // C++ ignores the returned error.
        let _ = set_paint_properties(layer.as_mut(), &Convertible::from(value));
        if let Some(entry) = layers_map.get_mut(id) {
            entry.layer = Some(layer);
        }
    } else {
        let mut error = Error::default();
        let Some(converted) = convert_layer(&Convertible::from(value), &mut error) else {
            Log::warning(Event::ParseStyle, &error.message);
            return;
        };
        if let Some(entry) = layers_map.get_mut(id) {
            entry.layer = Some(converted);
        }
    }
}

/// `formatJSONParseError` (src/mln/util/rapidjson.cpp), with serde_json's
/// message instead of rapidjson's (see the file comment).
fn format_json_parse_error(error: &serde_json::Error) -> String {
    error.to_string()
}

/// `parseFontFile(faceName, fontFile, warnings)`: a font file of a font
/// face, an object with a `url`, and with an optional `unicode-range` that
/// limits the code points the font draws. Adds a warning for each range it
/// can't read, and skips the range.
fn parse_font_file(
    face_name: &str,
    font_file: &serde_json::Map<String, serde_json::Value>,
    warnings: &mut Vec<String>,
) -> mln_text::glyph::FontFace {
    let mut font_face = mln_text::glyph::FontFace::new(face_name, "", vec![(0, 0x10FFFF)]);

    if let Some(url) = font_file.get("url").and_then(|u| u.as_str()) {
        font_face.url = url.to_owned();
    }

    if let Some(unicode_range) = font_file.get("unicode-range") {
        font_face.ranges.clear();
        if let Some(ranges) = unicode_range.as_array() {
            for range in ranges {
                let Some(range_string) = range.as_str() else {
                    continue;
                };
                if range_string.len() <= 2 {
                    continue;
                }
                // `substr` works on bytes: a multibyte character may be cut.
                let range_string = &range_string.as_bytes()[2..];
                let Some(pos) = range_string.iter().position(|&b| b == b'-') else {
                    continue;
                };
                let (start, end) = (&range_string[..pos], &range_string[pos + 1..]);
                if start.is_empty() || end.is_empty() {
                    continue;
                }
                // `util::parse<int>(s, 16)`: std::from_chars, the whole
                // string must be consumed.
                let parse = |s: &[u8]| std::str::from_utf8(s).ok().and_then(|s| i32::from_str_radix(s, 16).ok());
                match (parse(start), parse(end)) {
                    (Some(start), Some(end)) => font_face.ranges.push((start as u32, end as u32)),
                    _ => warnings
                        .push(format!("Invalid unicode-range in font-face: {}", String::from_utf8_lossy(range_string))),
                }
            }
        }
    }

    font_face
}

/// `parseFallbackFontFaces(json)`: fallback font faces from `json`, an
/// array of font files in the format of a font face in a style's
/// `font-faces`, without a font name. It fails with
/// [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) if `json` isn't such
/// an array.
pub fn parse_fallback_font_faces(json: &str) -> Result<mln_text::glyph::FontFaces, mln_util::Error> {
    let document: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| mln_util::Error::invalid_argument(format_json_parse_error(&error)))?;
    let Some(files) = document.as_array() else {
        return Err(mln_util::Error::invalid_argument("fallback font faces must be an array"));
    };

    let mut font_faces = mln_text::glyph::FontFaces::new();
    for font_file in files {
        let Some(font_file) = font_file.as_object() else {
            return Err(mln_util::Error::invalid_argument("each fallback font face must be an object"));
        };
        let mut warnings = Vec::new();
        let font_face = parse_font_file("", font_file, &mut warnings);
        if let Some(warning) = warnings.into_iter().next() {
            return Err(mln_util::Error::invalid_argument(warning));
        }
        if font_face.url.is_empty() {
            return Err(mln_util::Error::invalid_argument("each fallback font face must have a url"));
        }
        if font_face.ranges.is_empty() {
            return Err(mln_util::Error::invalid_argument("each fallback font face must cover a unicode-range"));
        }
        font_faces.push(mln_text::glyph::FontFace::fallback(&font_face.url, font_face.ranges));
    }
    Ok(font_faces)
}

/// The "font-faces" block of `Parser::parse` (MLN_TEXT_SHAPING_HARFBUZZ).
fn parse_font_faces(faces: &serde_json::Map<String, serde_json::Value>) -> mln_text::glyph::FontFaces {
    use mln_text::glyph::{FontFace, gen_new_glyph_id_type};
    let mut font_faces = mln_text::glyph::FontFaces::new();
    for (face_name, face_value) in faces {
        if let Some(files) = face_value.as_array() {
            // If the face is an array, we assume it is a list of font file
            // objects.
            for font_file in files {
                let Some(font_file) = font_file.as_object() else {
                    continue;
                };
                let mut warnings = Vec::new();
                let mut font_face = parse_font_file(face_name, font_file, &mut warnings);
                for warning in warnings {
                    Log::warning(Event::ParseStyle, warning);
                }

                // If valid, generate a unique glyph ID type for this font
                // face and add it to the font faces list.
                if font_face.valid() {
                    font_face.type_ =
                        gen_new_glyph_id_type(&font_face.url, &vec![font_face.name.clone()], &font_face.ranges);
                    font_faces.push(font_face);
                } else {
                    Log::warning(Event::ParseStyle, format!("Invalid font-face definition for: {face_name}"));
                }
            }
        } else if let Some(url) = face_value.as_str() {
            let mut font_face = FontFace::new(face_name, url, vec![(0, 0x10FFFF)]);
            font_face.type_ = gen_new_glyph_id_type(&font_face.url, &vec![font_face.name.clone()], &font_face.ranges);
            font_faces.push(font_face);
        } else {
            Log::warning(Event::ParseStyle, "font-face must be an object or array");
        }
    }
    font_faces
}
