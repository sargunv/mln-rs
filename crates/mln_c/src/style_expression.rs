//! `style_expression.h`: style expressions parsed and evaluated without a map.
//!
//! Both run on the [`parse_thread`](crate::parse_thread), so a deep expression
//! needs no stack from the caller's thread.

use core::ffi::c_char;
use std::sync::Arc;

use feature::{Feature, Value as JsonValue, ValueObject};
use mln_conversion::Convertible;
use mln_expression::{
    EvaluationContext, Expression, ParsingContext, Type, TypeAnnotationOption, is_feature_constant, is_zoom_constant,
};
use mln_tile_id::CanonicalTileID;
use mln_vec_map::VecSet;

use crate::abi::{self, Fields};
use crate::error::Error;
use crate::generated::types::{
    MLN_STYLE_EXPRESSION_INPUT_CANONICAL_TILE_ID, MLN_STYLE_EXPRESSION_INPUT_COLOR_RAMP_PARAMETER,
    MLN_STYLE_EXPRESSION_INPUT_ZOOM, MLN_STYLE_EXPRESSION_OPTION_EXPECTED_TYPE, MLN_STYLE_EXPRESSION_TYPE_ARRAY,
    MLN_STYLE_EXPRESSION_TYPE_BOOLEAN, MLN_STYLE_EXPRESSION_TYPE_COLOR, MLN_STYLE_EXPRESSION_TYPE_FORMATTED,
    MLN_STYLE_EXPRESSION_TYPE_IMAGE, MLN_STYLE_EXPRESSION_TYPE_NUMBER, MLN_STYLE_EXPRESSION_TYPE_OBJECT,
    MLN_STYLE_EXPRESSION_TYPE_PADDING, MLN_STYLE_EXPRESSION_TYPE_STRING, MLN_STYLE_EXPRESSION_TYPE_VALUE,
    MLN_STYLE_EXPRESSION_TYPE_VARIABLE_ANCHOR_OFFSET_COLLECTION, mln_buffer_view, mln_diagnostic, mln_status,
    mln_style_expression, mln_style_expression_info, mln_style_expression_inputs, mln_style_expression_options,
    mln_style_expression_type, mln_style_expression_type_kind,
};
use crate::handles::{self, HandleKind, HandleTable};
use crate::{buffer, geo, parse_thread};

abi::versioned!(mln_style_expression_info, mln_style_expression_inputs, mln_style_expression_options);

/// Parsed expressions, which only evaluation reads, so any number of threads
/// may evaluate one at once.
static EXPRESSIONS: HandleTable<Box<dyn Expression>> = HandleTable::new(HandleKind::StyleExpression);

/// The text that a caller's view names, as an owned copy for the parse
/// thread.
///
/// # Safety
///
/// As for [`buffer::text`].
unsafe fn owned_text(view: mln_buffer_view, name: &str) -> Result<String, Error> {
    if view.size == 0 {
        return Err(Error::invalid_argument(format!("{name} must not be empty")));
    }
    // SAFETY: the caller's contract.
    Ok(unsafe { buffer::text(view, name) }?.to_owned())
}

/// [`owned_text`] for an optional input, which an empty view leaves out.
///
/// # Safety
///
/// As for [`buffer::text`].
unsafe fn optional_text(view: mln_buffer_view, name: &str) -> Result<Option<String>, Error> {
    if view.size == 0 {
        return Ok(None);
    }
    // SAFETY: the caller's contract.
    unsafe { owned_text(view, name) }.map(Some)
}

fn parse_json(text: &str, name: &str) -> Result<serde_json::Value, Error> {
    serde_json::from_str(text).map_err(|error| Error::invalid_argument(format!("{name} is not valid JSON: {error}")))
}

fn type_kind(kind: mln_style_expression_type_kind) -> Option<Type> {
    Some(match kind {
        MLN_STYLE_EXPRESSION_TYPE_VALUE => Type::Value,
        MLN_STYLE_EXPRESSION_TYPE_NUMBER => Type::Number,
        MLN_STYLE_EXPRESSION_TYPE_STRING => Type::String,
        MLN_STYLE_EXPRESSION_TYPE_BOOLEAN => Type::Boolean,
        MLN_STYLE_EXPRESSION_TYPE_COLOR => Type::Color,
        MLN_STYLE_EXPRESSION_TYPE_OBJECT => Type::Object,
        MLN_STYLE_EXPRESSION_TYPE_FORMATTED => Type::Formatted,
        MLN_STYLE_EXPRESSION_TYPE_IMAGE => Type::Image,
        MLN_STYLE_EXPRESSION_TYPE_PADDING => Type::Padding,
        MLN_STYLE_EXPRESSION_TYPE_VARIABLE_ANCHOR_OFFSET_COLLECTION => Type::VariableAnchorOffsetCollection,
        _ => return None,
    })
}

fn expected_type(expected: mln_style_expression_type) -> Result<Type, Error> {
    if expected.kind != MLN_STYLE_EXPRESSION_TYPE_ARRAY {
        return type_kind(expected.kind).ok_or_else(|| Error::invalid_argument("expected_type.kind is unknown"));
    }
    if expected.item_kind == MLN_STYLE_EXPRESSION_TYPE_ARRAY {
        return Err(Error::invalid_argument("expected_type.item_kind must not be an array"));
    }
    let item =
        type_kind(expected.item_kind).ok_or_else(|| Error::invalid_argument("expected_type.item_kind is unknown"))?;
    Ok(match expected.item_count {
        0 => Type::array(item),
        count => Type::array_n(item, count as usize),
    })
}

/// The type that a caller's options require of the result, if any.
///
/// # Safety
///
/// `options` is null or valid for [`abi::read_sized`].
unsafe fn read_expected_type(options: *const mln_style_expression_options) -> Result<Option<Type>, Error> {
    if options.is_null() {
        return Ok(None);
    }
    // SAFETY: non-null, and the caller's contract.
    let options = unsafe { abi::read_sized(options) }?;
    let fields =
        Fields::new(options.fields, MLN_STYLE_EXPRESSION_OPTION_EXPECTED_TYPE, "mln_style_expression_options.fields")?;
    fields.get(MLN_STYLE_EXPRESSION_OPTION_EXPECTED_TYPE, options.expected_type).map(expected_type).transpose()
}

fn parse(json: &str, expected: Option<Type>) -> Result<Box<dyn Expression>, Error> {
    let document = parse_json(json, "expression")?;
    // GL JS's createExpression() coerces a result that must be a string
    // rather than asserting it, as a string layer property does.
    let annotation = (expected == Some(Type::String)).then_some(TypeAnnotationOption::Coerce);
    let mut context = match expected {
        Some(expected) => ParsingContext::with_expected(expected),
        None => ParsingContext::new(),
    };
    let Some(parsed) = context.parse_expression(&Convertible::from(&document), annotation) else {
        return Err(Error::invalid_argument(format!("expression: {}", context.get_combined_errors())));
    };
    // No value of another type holds a collator, so checking the result's
    // type keeps every result serializable.
    if *parsed.get_type() == Type::Collator {
        return Err(Error::invalid_argument("expression: a collator result has no JSON form"));
    }
    Ok(parsed)
}

/// `mln_style_expression_create()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_style_expression_create(
    expression: mln_buffer_view,
    options: *const mln_style_expression_options,
    out_expression: *mut mln_style_expression,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, {
        // SAFETY: the caller's contract.
        let out_expression = unsafe { handles::out_handle(out_expression, "out_expression") }?;
        // SAFETY: the caller's contract.
        let expected = unsafe { read_expected_type(options) }?;
        // SAFETY: the caller's contract.
        let json = unsafe { owned_text(expression, "expression") }?;
        let parsed = parse_thread::run(move || parse(&json, expected))??;
        *out_expression = EXPRESSIONS.insert(Arc::new(parsed))?;
        Ok(())
    })
}

/// `mln_style_expression_release()`.
#[unsafe(no_mangle)]
pub extern "C" fn mln_style_expression_release(expression: mln_style_expression) {
    // An evaluation that is running finishes with its own reference.
    let _ = EXPRESSIONS.remove(expression);
}

/// `mln_style_expression_get_info()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_style_expression_get_info(
    expression: mln_style_expression,
    out_info: *mut mln_style_expression_info,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_info), {
        let expression = EXPRESSIONS.get(expression)?;
        let info = mln_style_expression_info {
            size: 0,
            is_feature_constant: is_feature_constant(&**expression),
            is_zoom_constant: is_zoom_constant(&**expression),
        };
        // SAFETY: the caller's contract.
        unsafe { abi::write_sized(out_info, &info) }
    })
}

/// The inputs of one evaluation, copied from the caller's for the parse
/// thread. The JSON ones are still text.
#[derive(Default)]
struct Inputs {
    zoom: Option<f32>,
    color_ramp_parameter: Option<f64>,
    feature: Option<String>,
    feature_state: Option<String>,
    global_state: Option<String>,
    canonical_tile_id: Option<CanonicalTileID>,
    available_images: VecSet<String>,
}

fn finite(value: Option<f64>, name: &str) -> Result<Option<f64>, Error> {
    match value {
        Some(value) if !value.is_finite() => Err(Error::invalid_argument(format!("{name} must be finite"))),
        value => Ok(value),
    }
}

/// Checks and copies a caller's inputs.
///
/// # Safety
///
/// `inputs` is null or valid for [`abi::read_sized`], and its views and its
/// image array are valid for the header's use of them.
unsafe fn read_inputs(inputs: *const mln_style_expression_inputs) -> Result<Inputs, Error> {
    if inputs.is_null() {
        return Ok(Inputs::default());
    }
    // SAFETY: non-null, and the caller's contract.
    let inputs = unsafe { abi::read_sized(inputs) }?;
    let fields = Fields::new(
        inputs.fields,
        MLN_STYLE_EXPRESSION_INPUT_ZOOM
            | MLN_STYLE_EXPRESSION_INPUT_COLOR_RAMP_PARAMETER
            | MLN_STYLE_EXPRESSION_INPUT_CANONICAL_TILE_ID,
        "mln_style_expression_inputs.fields",
    )?;
    let zoom = finite(fields.get(MLN_STYLE_EXPRESSION_INPUT_ZOOM, inputs.zoom), "zoom")?;
    let color_ramp_parameter = finite(
        fields.get(MLN_STYLE_EXPRESSION_INPUT_COLOR_RAMP_PARAMETER, inputs.color_ramp_parameter),
        "color_ramp_parameter",
    )?;
    // SAFETY: the caller's contract, for each view.
    let (feature, feature_state, global_state) = unsafe {
        (
            optional_text(inputs.feature, "feature")?,
            optional_text(inputs.feature_state, "feature_state")?,
            optional_text(inputs.global_state, "global_state")?,
        )
    };
    let canonical_tile_id = fields
        .get(MLN_STYLE_EXPRESSION_INPUT_CANONICAL_TILE_ID, inputs.canonical_tile_id)
        .map(|tile| geo::canonical_tile_id(tile, "canonical_tile_id"))
        .transpose()?;
    let available_images = if inputs.available_image_count == 0 {
        VecSet::new()
    } else if inputs.available_images.is_null() {
        return Err(Error::invalid_argument("available_images must not be null when available_image_count is nonzero"));
    } else {
        // SAFETY: non-null, and the caller's contract: the array has
        // `available_image_count` views.
        let views = unsafe { core::slice::from_raw_parts(inputs.available_images, inputs.available_image_count) };
        // SAFETY: the caller's contract, for each view.
        views.iter().map(|view| unsafe { owned_text(*view, "available image ID") }).collect::<Result<_, _>>()?
    };
    Ok(Inputs {
        zoom: zoom.map(|zoom| zoom as f32),
        color_ramp_parameter,
        feature,
        feature_state,
        global_state,
        canonical_tile_id,
        available_images,
    })
}

fn parse_feature(text: &str) -> Result<Feature<f64>, Error> {
    let document = parse_json(text, "feature")?;
    match geojson::convert(&document) {
        Ok(geojson::GeoJSON::Feature(feature)) => Ok(feature),
        Ok(_) => Err(Error::invalid_argument("feature must contain one GeoJSON Feature")),
        Err(error) => Err(Error::invalid_argument(format!("feature is invalid: {}", error.message()))),
    }
}

fn parse_object(text: Option<&str>, name: &str) -> Result<Option<ValueObject>, Error> {
    let Some(text) = text else { return Ok(None) };
    let document = parse_json(text, name)?;
    if !document.is_object() {
        return Err(Error::invalid_argument(format!("{name} must contain a JSON object")));
    }
    match geojson::convert_value(&document) {
        JsonValue::Object(object) => Ok(Some(object)),
        _ => Err(Error::invalid_argument(format!("{name} must contain a JSON object"))),
    }
}

/// JSON has no number that isn't finite, so such a number becomes null, as
/// `JSON.stringify()` writes it.
fn replace_non_finite_numbers(value: &mut JsonValue) {
    match value {
        JsonValue::Double(number) if !number.is_finite() => *value = JsonValue::Null,
        JsonValue::Array(items) => items.iter_mut().for_each(replace_non_finite_numbers),
        JsonValue::Object(members) => members.values_mut().for_each(replace_non_finite_numbers),
        _ => {}
    }
}

/// Evaluates `expression` and writes its result as JSON.
fn evaluate(expression: &dyn Expression, inputs: &Inputs) -> Result<String, Error> {
    let feature = inputs.feature.as_deref().map(parse_feature).transpose()?;
    let feature_state = parse_object(inputs.feature_state.as_deref(), "feature_state")?;
    let global_state = parse_object(inputs.global_state.as_deref(), "global_state")?;
    let context = EvaluationContext {
        zoom: inputs.zoom,
        color_ramp_parameter: inputs.color_ramp_parameter,
        feature_state: feature_state.as_ref(),
        global_state: global_state.as_ref(),
        canonical: inputs.canonical_tile_id.as_ref(),
        available_images: Some(&inputs.available_images),
        ..EvaluationContext::default()
    };
    let result = match &feature {
        Some(feature) => mln_expression::expression::evaluate_feature_with_context(expression, feature, context),
        None => expression.evaluate(&context),
    };
    let result = result.map_err(|error| Error::invalid_argument(format!("expression: {}", error.message)))?;
    let mut value = mln_expression::value::to_mbgl_value(&result);
    replace_non_finite_numbers(&mut value);
    let mut writer = rapidjson::Writer::new();
    mln_style::conversion::stringify::stringify(&mut writer, &value);
    Ok(writer.into_string())
}

/// `mln_style_expression_evaluate()`.
///
/// # Safety
///
/// Each pointer is null or valid for the header's use of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mln_style_expression_evaluate(
    expression: mln_style_expression,
    inputs: *const mln_style_expression_inputs,
    out_json: *mut c_char,
    json_capacity: usize,
    out_json_length: *mut usize,
    out_diagnostic: *mut mln_diagnostic,
) -> mln_status {
    export!(out_diagnostic, non_null(out_json_length), {
        if out_json.is_null() && json_capacity != 0 {
            return Err(Error::invalid_argument("out_json must not be null when json_capacity is nonzero"));
        }
        let expression = EXPRESSIONS.get(expression)?;
        // SAFETY: the caller's contract.
        let inputs = unsafe { read_inputs(inputs) }?;
        let json = parse_thread::run(move || evaluate(&**expression, &inputs))??;
        // SAFETY: the caller's contract.
        unsafe { buffer::write_text(&json, out_json, json_capacity, out_json_length, ("out_json", "out_json_length")) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::tests::{diagnostic, message};
    use crate::generated::types::{
        MLN_STATUS_INVALID_ARGUMENT, MLN_STATUS_INVALID_STATE, MLN_STATUS_OK, mln_canonical_tile_id,
    };

    type Failure = (mln_status, String);

    fn view(text: &str) -> mln_buffer_view {
        mln_buffer_view { data: text.as_ptr().cast(), size: text.len() }
    }

    fn create_with(
        json: &str,
        options: Option<&mln_style_expression_options>,
    ) -> Result<mln_style_expression, Failure> {
        let mut expression = handles::NULL;
        let mut out = diagnostic();
        let options = options.map_or(core::ptr::null(), core::ptr::from_ref);
        // SAFETY: locals that outlive the call.
        let status = unsafe { mln_style_expression_create(view(json), options, &mut expression, &mut *out) };
        if status == MLN_STATUS_OK {
            Ok(expression)
        } else {
            assert_eq!(expression, handles::NULL);
            Err((status, message(&out).to_owned()))
        }
    }

    fn create(json: &str) -> mln_style_expression {
        create_with(json, None).unwrap()
    }

    fn evaluate_with(
        expression: mln_style_expression,
        inputs: Option<&mln_style_expression_inputs>,
    ) -> Result<String, Failure> {
        let mut json = [0 as c_char; 512];
        let mut length = 0;
        let mut out = diagnostic();
        let inputs = inputs.map_or(core::ptr::null(), core::ptr::from_ref);
        // SAFETY: locals that outlive the call, and the capacity of `json`.
        let status = unsafe {
            mln_style_expression_evaluate(expression, inputs, json.as_mut_ptr(), json.len(), &mut length, &mut *out)
        };
        if status != MLN_STATUS_OK {
            return Err((status, message(&out).to_owned()));
        }
        let bytes: Vec<u8> = json[..length].iter().map(|byte| *byte as u8).collect();
        Ok(String::from_utf8(bytes).unwrap())
    }

    fn evaluates(json: &str, inputs: Option<&mln_style_expression_inputs>) -> Result<String, Failure> {
        let expression = create(json);
        let result = evaluate_with(expression, inputs);
        mln_style_expression_release(expression);
        result
    }

    fn inputs() -> mln_style_expression_inputs {
        let empty = mln_buffer_view { data: core::ptr::null(), size: 0 };
        mln_style_expression_inputs {
            size: size_of::<mln_style_expression_inputs>() as u32,
            fields: 0,
            zoom: 0.0,
            color_ramp_parameter: 0.0,
            feature: empty,
            feature_state: empty,
            global_state: empty,
            canonical_tile_id: mln_canonical_tile_id { z: 0, x: 0, y: 0 },
            available_images: core::ptr::null(),
            available_image_count: 0,
        }
    }

    fn feature_inputs(feature: &'static str) -> mln_style_expression_inputs {
        mln_style_expression_inputs { feature: view(feature), ..inputs() }
    }

    fn expect_type(kind: u32, item_kind: u32, item_count: u32) -> mln_style_expression_options {
        mln_style_expression_options {
            size: size_of::<mln_style_expression_options>() as u32,
            fields: MLN_STYLE_EXPRESSION_OPTION_EXPECTED_TYPE,
            expected_type: mln_style_expression_type { kind, item_kind, item_count },
        }
    }

    #[test]
    fn each_input_reaches_the_expression_that_reads_it() {
        let zoom = mln_style_expression_inputs { fields: MLN_STYLE_EXPRESSION_INPUT_ZOOM, zoom: 5.0, ..inputs() };
        let interpolate = r#"["interpolate", ["linear"], ["zoom"], 0, 0, 10, 100]"#;
        assert_eq!(evaluates(interpolate, Some(&zoom)).unwrap(), "50.0");

        let ramp = mln_style_expression_inputs {
            fields: MLN_STYLE_EXPRESSION_INPUT_COLOR_RAMP_PARAMETER,
            color_ramp_parameter: 0.25,
            ..inputs()
        };
        for json in [r#"["heatmap-density"]"#, r#"["line-progress"]"#, r#"["elevation"]"#] {
            assert_eq!(evaluates(json, Some(&ramp)).unwrap(), "0.25", "{json}");
        }

        let park = feature_inputs(
            r#"{"type":"Feature","id":7,"geometry":{"type":"Point","coordinates":[1,2]},"properties":{"name":"park"}}"#,
        );
        assert_eq!(evaluates(r#"["get", "name"]"#, Some(&park)).unwrap(), r#""park""#);
        assert_eq!(evaluates(r#"["id"]"#, Some(&park)).unwrap(), "7.0");
        assert_eq!(evaluates(r#"["geometry-type"]"#, Some(&park)).unwrap(), r#""Point""#);

        let state = mln_style_expression_inputs { feature_state: view(r#"{"hover":true}"#), ..inputs() };
        assert_eq!(evaluates(r#"["feature-state", "hover"]"#, Some(&state)).unwrap(), "true");
        let global = mln_style_expression_inputs { global_state: view(r#"{"theme":"dark"}"#), ..inputs() };
        assert_eq!(evaluates(r#"["global-state", "theme"]"#, Some(&global)).unwrap(), r#""dark""#);

        let images = [view("monument-15")];
        let available =
            mln_style_expression_inputs { available_images: images.as_ptr(), available_image_count: 1, ..inputs() };
        let image: serde_json::Value =
            serde_json::from_str(&evaluates(r#"["image", "monument-15"]"#, Some(&available)).unwrap()).unwrap();
        assert_eq!(image, serde_json::json!({"name": "monument-15", "available": true}));
        let missing: serde_json::Value =
            serde_json::from_str(&evaluates(r#"["image", "missing"]"#, Some(&available)).unwrap()).unwrap();
        assert_eq!(missing, serde_json::json!({"name": "missing", "available": false}));
    }

    #[test]
    fn within_reads_the_canonical_tile() {
        let within = r#"["within", {"type": "Polygon", "coordinates": [[[0, 0], [0, 5], [5, 5], [5, 0], [0, 0]]]}]"#;
        let tile = mln_canonical_tile_id { z: 3, x: 3, y: 3 };
        let point = |feature| mln_style_expression_inputs {
            fields: MLN_STYLE_EXPRESSION_INPUT_CANONICAL_TILE_ID,
            canonical_tile_id: tile,
            ..feature_inputs(feature)
        };
        let inside = point(r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[2,2]},"properties":{}}"#);
        let outside = point(r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[6,6]},"properties":{}}"#);
        assert_eq!(evaluates(within, Some(&inside)).unwrap(), "true");
        assert_eq!(evaluates(within, Some(&outside)).unwrap(), "false");
    }

    #[test]
    fn values_serialize_as_style_json() {
        let cases = [
            (r#"["to-color", "red"]"#, r#"["rgba",255.0,0.0,0.0,1.0]"#),
            (r#"["format", "a", {"font-scale": 2}]"#, r#"["format","a",{"font-scale":2.0}]"#),
            // JSON has no NaN or infinity, so they become null.
            (r#"["/", 0, 0]"#, "null"),
            (r#"["-", ["ln", 0]]"#, "null"),
            (r#"["literal", [1, 2]]"#, "[1.0,2.0]"),
            (r#"["literal", {"a": [1, true]}]"#, r#"{"a":[1.0,true]}"#),
            (r#""text""#, r#""text""#),
        ];
        for (json, expected) in cases {
            assert_eq!(evaluates(json, None).unwrap(), expected, "{json}");
        }
    }

    #[test]
    fn an_expected_type_coerces_strings_and_asserts_the_rest() {
        let number = feature_inputs(r#"{"type":"Feature","geometry":null,"properties":{"n":5}}"#);
        let string =
            create_with(r#"["get", "n"]"#, Some(&expect_type(MLN_STYLE_EXPRESSION_TYPE_STRING, 0, 0))).unwrap();
        assert_eq!(evaluate_with(string, Some(&number)).unwrap(), r#""5""#);
        mln_style_expression_release(string);

        let name = feature_inputs(r#"{"type":"Feature","geometry":null,"properties":{"name":"park"}}"#);
        let number_type = expect_type(MLN_STYLE_EXPRESSION_TYPE_NUMBER, 0, 0);
        let asserted = create_with(r#"["get", "name"]"#, Some(&number_type)).unwrap();
        let (status, message) = evaluate_with(asserted, Some(&name)).unwrap_err();
        assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
        assert!(message.starts_with("expression: ") && message.contains("number"), "{message}");
        mln_style_expression_release(asserted);

        let color = create_with(r##""#00ff00""##, Some(&expect_type(MLN_STYLE_EXPRESSION_TYPE_COLOR, 0, 0))).unwrap();
        assert_eq!(evaluate_with(color, None).unwrap(), r#"["rgba",0.0,255.0,0.0,1.0]"#);
        mln_style_expression_release(color);

        let pair = expect_type(MLN_STYLE_EXPRESSION_TYPE_ARRAY, MLN_STYLE_EXPRESSION_TYPE_NUMBER, 2);
        let array = create_with(r#"["literal", [1, 2]]"#, Some(&pair)).unwrap();
        assert_eq!(evaluate_with(array, None).unwrap(), "[1.0,2.0]");
        mln_style_expression_release(array);
        assert!(create_with(r#"["literal", [1, 2, 3]]"#, Some(&pair)).is_err());
        assert!(create_with(r#"["literal", ["a", "b"]]"#, Some(&pair)).is_err());
    }

    #[test]
    fn parse_errors_name_their_location() {
        let (status, message) = create_with(r#"["no-such-operator"]"#, None).unwrap_err();
        assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
        assert_eq!(
            message,
            r#"expression: [0]: Unknown expression "no-such-operator". If you wanted a literal array, use ["literal", [...]]."#
        );
        assert!(create_with(r#"["+", 1, "a"]"#, None).is_err());
        assert!(create_with(r#"{"a": 1}"#, None).is_err());
        assert!(create_with("[", None).unwrap_err().1.contains("not valid JSON"));
        // A collator has no JSON form.
        assert!(create_with(r#"["collator", {}]"#, None).unwrap_err().1.contains("collator"));
    }

    #[test]
    fn creation_rejects_invalid_arguments() {
        // SAFETY: a null out-handle and diagnostic.
        let status = unsafe {
            mln_style_expression_create(view("1"), core::ptr::null(), core::ptr::null_mut(), core::ptr::null_mut())
        };
        assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
        let empty = create_with("", None).unwrap_err().1;
        assert_eq!(empty, "expression must not be empty");

        let expression = create("1");
        let mut occupied = expression;
        // SAFETY: a local out-handle and a null diagnostic.
        let status =
            unsafe { mln_style_expression_create(view("1"), core::ptr::null(), &mut occupied, core::ptr::null_mut()) };
        assert_eq!((status, occupied), (MLN_STATUS_INVALID_ARGUMENT, expression));
        mln_style_expression_release(expression);

        let small = mln_style_expression_options { size: 4, ..expect_type(MLN_STYLE_EXPRESSION_TYPE_VALUE, 0, 0) };
        let unknown_bit = mln_style_expression_options { fields: 1 << 7, ..small };
        let unknown_bit =
            mln_style_expression_options { size: small.size.max(size_of_val(&small) as u32), ..unknown_bit };
        for (options, json) in [
            (small, "1"),
            (unknown_bit, "1"),
            (expect_type(99, 0, 0), "1"),
            (expect_type(MLN_STYLE_EXPRESSION_TYPE_ARRAY, MLN_STYLE_EXPRESSION_TYPE_ARRAY, 0), r#"["literal", [[1]]]"#),
            (expect_type(MLN_STYLE_EXPRESSION_TYPE_ARRAY, 99, 0), r#"["literal", [1]]"#),
        ] {
            assert_eq!(create_with(json, Some(&options)).unwrap_err().0, MLN_STATUS_INVALID_ARGUMENT);
        }
    }

    #[test]
    fn info_reports_zoom_and_feature_dependence() {
        for (json, feature_constant, zoom_constant) in [
            ("1", true, true),
            (r#"["get", "x"]"#, false, true),
            (r#"["feature-state", "x"]"#, false, true),
            (r#"["interpolate", ["linear"], ["zoom"], 0, 0, 1, 1]"#, true, false),
            (r#"["step", ["zoom"], ["get", "x"], 5, 1]"#, false, false),
        ] {
            let expression = create(json);
            let mut info = mln_style_expression_info {
                size: size_of::<mln_style_expression_info>() as u32,
                is_feature_constant: !feature_constant,
                is_zoom_constant: !zoom_constant,
            };
            // SAFETY: a local output and a null diagnostic.
            let status = unsafe { mln_style_expression_get_info(expression, &mut info, core::ptr::null_mut()) };
            assert_eq!(status, MLN_STATUS_OK);
            assert_eq!((info.is_feature_constant, info.is_zoom_constant), (feature_constant, zoom_constant), "{json}");
            mln_style_expression_release(expression);
        }

        let expression = create("1");
        let mut small = mln_style_expression_info { size: 4, is_feature_constant: false, is_zoom_constant: false };
        // SAFETY: a local or null output, and a null diagnostic.
        unsafe {
            let status = mln_style_expression_get_info(expression, core::ptr::null_mut(), core::ptr::null_mut());
            assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
            let status = mln_style_expression_get_info(expression, &mut small, core::ptr::null_mut());
            assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT);
        }
        mln_style_expression_release(expression);
    }

    #[test]
    fn a_short_buffer_reports_the_length_it_needs() {
        let expression = create(r#""abcdef""#);
        let mut length = 0;
        let mut exact = [b'x' as c_char; 8];
        let mut fits = [0 as c_char; 9];
        // SAFETY: local buffers of the capacities passed, and nulls.
        unsafe {
            let evaluate = |out: *mut c_char, capacity, length: *mut usize| {
                mln_style_expression_evaluate(
                    expression,
                    core::ptr::null(),
                    out,
                    capacity,
                    length,
                    core::ptr::null_mut(),
                )
            };
            assert_eq!(evaluate(core::ptr::null_mut(), 0, &mut length), MLN_STATUS_OK);
            assert_eq!(length, 8);
            assert_eq!(evaluate(exact.as_mut_ptr(), exact.len(), &mut length), MLN_STATUS_OK);
            assert!(exact.iter().all(|byte| *byte == b'x' as c_char));
            assert_eq!(evaluate(fits.as_mut_ptr(), fits.len(), &mut length), MLN_STATUS_OK);
            assert_eq!(fits.map(|byte| byte as u8), *b"\"abcdef\"\0");
            assert_eq!(evaluate(core::ptr::null_mut(), 1, &mut length), MLN_STATUS_INVALID_ARGUMENT);
            assert_eq!(evaluate(fits.as_mut_ptr(), fits.len(), core::ptr::null_mut()), MLN_STATUS_INVALID_ARGUMENT);
        }
        mln_style_expression_release(expression);
    }

    #[test]
    fn absent_inputs_fail_the_expressions_that_need_them() {
        // Empty views leave their inputs out, whatever they point to.
        let empty = mln_style_expression_inputs {
            feature: mln_buffer_view { data: c"{}".as_ptr().cast(), size: 0 },
            feature_state: mln_buffer_view { data: c"{}".as_ptr().cast(), size: 0 },
            ..inputs()
        };
        for absent in [None, Some(&empty)] {
            for json in [r#"["step", ["zoom"], 0, 1, 1]"#, r#"["heatmap-density"]"#, r#"["get", "x"]"#] {
                let (status, message) = evaluates(json, absent).unwrap_err();
                assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT, "{json}");
                assert!(message.contains("unavailable"), "{json}: {message}");
            }
            assert_eq!(evaluates(r#"["feature-state", "x"]"#, absent).unwrap(), "null");
            assert_eq!(evaluates(r#"["global-state", "x"]"#, absent).unwrap(), "null");
        }
    }

    #[test]
    fn evaluation_rejects_invalid_inputs() {
        let expression = create("1");
        let failure = |inputs: &mln_style_expression_inputs| {
            let (status, message) = evaluate_with(expression, Some(inputs)).unwrap_err();
            assert_eq!(status, MLN_STATUS_INVALID_ARGUMENT, "{message}");
            message
        };
        failure(&mln_style_expression_inputs { size: 4, ..inputs() });
        failure(&mln_style_expression_inputs { fields: 1 << 20, ..inputs() });
        for field in [MLN_STYLE_EXPRESSION_INPUT_ZOOM, MLN_STYLE_EXPRESSION_INPUT_COLOR_RAMP_PARAMETER] {
            let infinite = mln_style_expression_inputs {
                fields: field,
                zoom: f64::NAN,
                color_ramp_parameter: f64::INFINITY,
                ..inputs()
            };
            assert!(failure(&infinite).contains("must be finite"));
        }
        failure(&feature_inputs(r#"{"type":"Point","coordinates":[0,0]}"#));
        failure(&feature_inputs("{"));
        let array_state = mln_style_expression_inputs { feature_state: view("[1]"), ..inputs() };
        assert!(failure(&array_state).contains("feature_state must contain a JSON object"));
        failure(&mln_style_expression_inputs { global_state: view("[1]"), ..inputs() });
        failure(&mln_style_expression_inputs {
            global_state: mln_buffer_view { data: core::ptr::null(), size: 2 },
            ..inputs()
        });
        failure(&mln_style_expression_inputs {
            fields: MLN_STYLE_EXPRESSION_INPUT_CANONICAL_TILE_ID,
            canonical_tile_id: mln_canonical_tile_id { z: 1, x: 2, y: 0 },
            ..inputs()
        });
        failure(&mln_style_expression_inputs { available_image_count: 1, ..inputs() });
        let empty_id = [mln_buffer_view { data: core::ptr::null(), size: 0 }];
        failure(&mln_style_expression_inputs {
            available_images: empty_id.as_ptr(),
            available_image_count: 1,
            ..inputs()
        });
        mln_style_expression_release(expression);
    }

    #[test]
    fn a_released_expression_is_stale_and_any_thread_may_use_one() {
        let expression = create(r#"["concat", "a", "b"]"#);
        let result = std::thread::spawn(move || {
            let result = evaluate_with(expression, None);
            mln_style_expression_release(expression);
            result
        })
        .join()
        .unwrap();
        assert_eq!(result.unwrap(), r#""ab""#);
        mln_style_expression_release(expression);
        mln_style_expression_release(handles::NULL);
        assert_eq!(evaluate_with(expression, None).unwrap_err().0, MLN_STATUS_INVALID_STATE);
        let mut info = mln_style_expression_info { size: 4, is_feature_constant: false, is_zoom_constant: false };
        info.size = size_of_val(&info) as u32;
        // SAFETY: a local output and a null diagnostic.
        let status = unsafe { mln_style_expression_get_info(expression, &mut info, core::ptr::null_mut()) };
        assert_eq!(status, MLN_STATUS_INVALID_STATE);
        assert_eq!(evaluate_with(handles::NULL, None).unwrap_err().0, MLN_STATUS_INVALID_ARGUMENT);
    }

    #[test]
    fn the_deepest_expression_parses_and_evaluates_on_a_small_stack() {
        // serde_json's recursion limit, as nested sums, from a thread with
        // musl's default stack.
        let depth = 126;
        let json = format!("{}1{}", r#"["+", 1, "#.repeat(depth), "]".repeat(depth));
        let result = std::thread::Builder::new()
            .stack_size(128 << 10)
            .spawn(move || evaluates(&json, None))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(result.unwrap(), format!("{}.0", depth + 1));
    }
}
