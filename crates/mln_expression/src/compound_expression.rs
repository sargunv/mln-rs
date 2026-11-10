// Ports include/mln/style/expression/compound_expression.hpp and
// src/mln/style/expression/compound_expression.cpp.
//
// C++ derives each signature's parameter and result types from the C++
// types of a lambda (`Signature<R(Params...)>`). Here each definition states
// its `type::Type`s explicitly and its `apply` function evaluates the
// arguments and unpacks the values the way `fromExpressionValue<T>` does.
// The registry keeps the C++ definition order, which is the overload order.

use std::sync::LazyLock;

use feature::{Identifier as FeatureIdentifier, Value as MbglValue};
use mln_conversion::Convertible;
use mln_util::geometry::FeatureType;
use mln_util::i18n;

use crate::check_subtype::check_subtype;
use crate::collator::Collator;
use crate::expression::{
    Dependency, EvaluationContext, EvaluationError, EvaluationResult, Expression, ExpressionBase, Kind, children_equal,
    collect_dependencies, downcast, evaluate_ref, mbgl_to_expression_value,
};
use crate::parsing_context::{ParseResult, ParsingContext};
use crate::type_::{self, Type};
use crate::utf8_op_helpers::{get_unicode_character_offset_on_validated_utf8, unicode_length_on_validated_utf8};
use crate::util::rgba;
use crate::value::{ObjectType, Value, from_mbgl_value_owned, to_string, type_of};

/// The parameter list of a signature: fixed types or `VarargsType`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Params {
    Fixed(Vec<Type>),
    Varargs(Type),
}

type ApplyFn = fn(&EvaluationContext<'_>, &[Box<dyn Expression>]) -> EvaluationResult;

/// `detail::SignatureBase`.
pub struct SignatureBase {
    pub result: Type,
    pub params: Params,
    pub name: &'static str,
    pub dependencies: Dependency,
    apply: ApplyFn,
}

impl SignatureBase {
    pub fn apply(&self, params: &EvaluationContext<'_>, args: &[Box<dyn Expression>]) -> EvaluationResult {
        (self.apply)(params, args)
    }
}

/// Evaluates the arguments in order, stopping at the first error (both
/// `Signature::applyImpl` and the varargs `apply`).
fn eval_args(params: &EvaluationContext<'_>, args: &[Box<dyn Expression>]) -> Result<Vec<Value>, EvaluationError> {
    args.iter().map(|arg| arg.evaluate(params)).collect()
}

// `*fromExpressionValue<T>(value)`: the type checker guarantees the
// alternative.
fn num(v: &Value) -> f64 {
    v.as_number().expect("number argument")
}
fn boolean(v: &Value) -> bool {
    v.as_bool().expect("boolean argument")
}
fn string(v: &Value) -> &str {
    v.as_str().expect("string argument")
}

fn sig(name: &'static str, result: Type, params: Params, dependencies: Dependency, apply: ApplyFn) -> SignatureBase {
    SignatureBase { result, params, name, dependencies, apply }
}

fn fixed(types: &[Type]) -> Params {
    Params::Fixed(types.to_vec())
}

fn feature_id_as_expression_value(params: &EvaluationContext<'_>) -> Value {
    let feature = params.feature.expect("feature");
    let id = feature.get_id();
    if let FeatureIdentifier::Null = id {
        return Value::Null;
    }
    identifier_to_expression_value(&id)
}

/// `toExpressionValue(mln::Value(identifier))`.
fn identifier_to_expression_value(id: &FeatureIdentifier) -> Value {
    match id {
        FeatureIdentifier::Null => Value::Null,
        FeatureIdentifier::Uint(v) => Value::Number(*v as f64),
        FeatureIdentifier::Int(v) => Value::Number(*v as f64),
        FeatureIdentifier::Double(v) => Value::Number(*v),
        FeatureIdentifier::String(v) => Value::String(v.clone()),
    }
}

fn feature_property_as_expression_value(params: &EvaluationContext<'_>, key: &str) -> Option<Value> {
    let feature = params.feature.expect("feature");
    feature.get_value(key).map(from_mbgl_value_owned)
}

fn feature_type_as_string(t: FeatureType) -> Option<String> {
    Some(
        match t {
            FeatureType::Point => "Point",
            FeatureType::LineString => "LineString",
            FeatureType::Polygon => "Polygon",
            FeatureType::Unknown => "Unknown",
        }
        .to_owned(),
    )
}

fn feature_property_as_double(params: &EvaluationContext<'_>, key: &str) -> Option<f64> {
    let feature = params.feature.expect("feature");
    match feature.get_value(key)? {
        MbglValue::Double(v) => Some(v),
        MbglValue::Uint(v) => Some(v as f64),
        MbglValue::Int(v) => Some(v as f64),
        _ => None,
    }
}

fn feature_property_as_string(params: &EvaluationContext<'_>, key: &str) -> Option<String> {
    let feature = params.feature.expect("feature");
    match feature.get_value(key)? {
        MbglValue::String(v) => Some(v),
        _ => None,
    }
}

fn feature_id_as_double(params: &EvaluationContext<'_>) -> Option<f64> {
    let feature = params.feature.expect("feature");
    match feature.get_id() {
        FeatureIdentifier::Double(v) => Some(v),
        FeatureIdentifier::Uint(v) => Some(v as f64),
        FeatureIdentifier::Int(v) => Some(v as f64),
        _ => None,
    }
}

fn feature_id_as_string(params: &EvaluationContext<'_>) -> Option<String> {
    let feature = params.feature.expect("feature");
    match feature.get_id() {
        FeatureIdentifier::String(v) => Some(v),
        _ => None,
    }
}

fn feature_unavailable() -> EvaluationError {
    EvaluationError::new("Feature data is unavailable in the current evaluation context.")
}

use mln_math::{fmax, fmin};

/// "is-supported-script": a script that needs complex shaping is supported
/// where a font face of the style covers it, since HarfBuzz shapes the text
/// that font faces draw.
fn is_supported_script(params: &EvaluationContext<'_>, x: &str) -> bool {
    let Some(font_faces) = params.font_faces.filter(|faces| !faces.is_empty()) else {
        return i18n::is_string_in_supported_script(x);
    };
    x.encode_utf16().all(|chr| {
        i18n::char_in_supported_script(chr)
            || font_faces
                .iter()
                .any(|face| face.ranges.iter().any(|range| u32::from(chr) >= range.0 && u32::from(chr) <= range.1))
    })
}

/// `platform::uppercase` / `platform::lowercase` (platform/default
/// string_stdlib.cpp, nunicode case mapping). nunicode writes invalid UTF-8
/// for some 4-byte results; a Rust String holds U+FFFD there.
fn uppercase(s: &str) -> String {
    nunicode::uppercase(s)
}
fn lowercase(s: &str) -> String {
    nunicode::lowercase(s)
}

// A legacy comparison filter compares a number to a numeric feature value and
// a string to a string one. One signature takes either, rather than an
// overload for each, because parsing tries every overload of an arity on each
// argument.
fn filter_property(
    params: &EvaluationContext<'_>,
    args: &[Box<dyn Expression>],
    compare_double: fn(&f64, &f64) -> bool,
    compare_string: fn(&str, &str) -> bool,
) -> EvaluationResult {
    let a = eval_args(params, args)?;
    let key = string(&a[0]);
    Ok(Value::Bool(match &a[1] {
        Value::Number(value) => feature_property_as_double(params, key).is_some_and(|rhs| compare_double(&rhs, value)),
        Value::String(value) => feature_property_as_string(params, key).is_some_and(|rhs| compare_string(&rhs, value)),
        _ => false,
    }))
}

fn filter_id(
    params: &EvaluationContext<'_>,
    args: &[Box<dyn Expression>],
    compare_double: fn(&f64, &f64) -> bool,
    compare_string: fn(&str, &str) -> bool,
) -> EvaluationResult {
    let a = eval_args(params, args)?;
    Ok(Value::Bool(match &a[0] {
        Value::Number(value) => feature_id_as_double(params).is_some_and(|rhs| compare_double(&rhs, value)),
        Value::String(value) => feature_id_as_string(params).is_some_and(|rhs| compare_string(&rhs, value)),
        _ => false,
    }))
}

fn math1(params: &EvaluationContext<'_>, args: &[Box<dyn Expression>], f: fn(f64) -> f64) -> EvaluationResult {
    let a = eval_args(params, args)?;
    Ok(Value::Number(f(num(&a[0]))))
}

static REGISTRY: LazyLock<Vec<SignatureBase>> = LazyLock::new(|| {
    use Dependency as D;
    use Type as T;
    let s = T::String;
    let n = T::Number;
    vec![
        sig("e", n.clone(), fixed(&[]), D::NONE, |_, _| Ok(Value::Number(std::f64::consts::E))),
        sig("pi", n.clone(), fixed(&[]), D::NONE, |_, _| Ok(Value::Number(std::f64::consts::PI))),
        sig("ln2", n.clone(), fixed(&[]), D::NONE, |_, _| Ok(Value::Number(std::f64::consts::LN_2))),
        sig("typeof", s.clone(), fixed(&[T::Value]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            Ok(Value::String(type_::to_string(&type_of(&a[0]))))
        }),
        sig("to-rgba", T::array_n(T::Number, 4), fixed(&[T::Color]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            let Value::Color(color) = &a[0] else { panic!("color argument") };
            Ok(Value::Array(color.to_array().iter().map(|v| Value::Number(*v)).collect()))
        }),
        sig("rgba", T::Color, fixed(&[n.clone(), n.clone(), n.clone(), n.clone()]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            Ok(Value::Color(rgba(num(&a[0]), num(&a[1]), num(&a[2]), num(&a[3]))?))
        }),
        sig("rgb", T::Color, fixed(&[n.clone(), n.clone(), n.clone()]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            Ok(Value::Color(rgba(num(&a[0]), num(&a[1]), num(&a[2]), 1.0f32 as f64)?))
        }),
        sig("zoom", n.clone(), fixed(&[]), D::ZOOM, |p, _| match p.zoom {
            None => {
                Err(EvaluationError::new("The 'zoom' expression is unavailable in the current evaluation context."))
            }
            Some(zoom) => Ok(Value::Number(zoom as f64)),
        }),
        sig("heatmap-density", n.clone(), fixed(&[]), D::NONE, |p, _| match p.color_ramp_parameter {
            None => Err(EvaluationError::new(
                "The 'heatmap-density' expression is unavailable in the current evaluation context.",
            )),
            Some(v) => Ok(Value::Number(v)),
        }),
        sig("elevation", n.clone(), fixed(&[]), D::ELEVATION, |p, _| {
            // For color-relief, elevation is passed via colorRampParameter
            if let Some(v) = p.color_ramp_parameter {
                return Ok(Value::Number(v));
            }
            // For 3D terrain, elevation is passed directly
            if let Some(v) = p.elevation {
                return Ok(Value::Number(v as f64));
            }
            // During parsing/validation, return a valid value to allow
            // parsing to succeed. The Dependency::Elevation flag prevents
            // this from being optimized to a constant.
            Ok(Value::Number(0.0))
        }),
        sig("line-progress", n.clone(), fixed(&[]), D::NONE, |p, _| match p.color_ramp_parameter {
            None => Err(EvaluationError::new(
                "The 'line-progress' expression is unavailable in the current evaluation context.",
            )),
            Some(v) => Ok(Value::Number(v)),
        }),
        sig("accumulated", T::Value, fixed(&[]), D::NONE, |p, _| match p.accumulated {
            None => Err(EvaluationError::new(
                "The 'accumulated' expression is unavailable in the current evaluation context.",
            )),
            Some(v) => Ok(mbgl_to_expression_value(v)),
        }),
        sig("has", T::Boolean, fixed(&[s.clone()]), D::FEATURE, |p, args| {
            let key = evaluate_ref(&*args[0], p)?;
            let Some(feature) = p.feature else {
                return Err(feature_unavailable());
            };
            Ok(Value::Bool(feature.get_value(string(&key)).is_some()))
        }),
        sig("has", T::Boolean, fixed(&[s.clone(), T::Object]), D::FEATURE, |p, args| {
            let a = eval_args(p, args)?;
            let Value::Object(object) = &a[1] else { panic!("object argument") };
            Ok(Value::Bool(object.contains_key(string(&a[0]))))
        }),
        sig("get", T::Value, fixed(&[s.clone()]), D::FEATURE, |p, args| {
            let key = evaluate_ref(&*args[0], p)?;
            let Some(feature) = p.feature else {
                return Err(feature_unavailable());
            };
            match feature.get_value(string(&key)) {
                None => Ok(Value::Null),
                Some(v) => Ok(from_mbgl_value_owned(v)),
            }
        }),
        sig("get", T::Value, fixed(&[s.clone(), T::Object]), D::FEATURE, |p, args| {
            let a = eval_args(p, args)?;
            let Value::Object(object) = &a[1] else { panic!("object argument") };
            Ok(object.get(string(&a[0])).cloned().unwrap_or(Value::Null))
        }),
        sig("properties", T::Object, fixed(&[]), D::FEATURE, |p, _| {
            let Some(feature) = p.feature else {
                return Err(feature_unavailable());
            };
            let mut result = ObjectType::new();
            for (k, v) in feature.get_properties() {
                result.insert(k.clone(), mbgl_to_expression_value(v));
            }
            Ok(Value::Object(result))
        }),
        sig("geometry-type", s.clone(), fixed(&[]), D::FEATURE, |p, _| {
            let Some(feature) = p.feature else {
                return Err(feature_unavailable());
            };
            Ok(Value::String(
                match feature.get_type() {
                    FeatureType::Point => "Point",
                    FeatureType::LineString => "LineString",
                    FeatureType::Polygon => "Polygon",
                    FeatureType::Unknown => "Unknown",
                }
                .to_owned(),
            ))
        }),
        sig("id", T::Value, fixed(&[]), D::FEATURE, |p, _| {
            let Some(feature) = p.feature else {
                return Err(feature_unavailable());
            };
            Ok(identifier_to_expression_value(&feature.get_id()))
        }),
        sig("+", n.clone(), Params::Varargs(n.clone()), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            let mut sum = 0.0f32 as f64;
            for arg in &a {
                sum += num(arg);
            }
            Ok(Value::Number(sum))
        }),
        sig("-", n.clone(), fixed(&[n.clone(), n.clone()]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            Ok(Value::Number(num(&a[0]) - num(&a[1])))
        }),
        sig("-", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            Ok(Value::Number(-num(&a[0])))
        }),
        sig("*", n.clone(), Params::Varargs(n.clone()), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            let mut prod = 1.0f32 as f64;
            for arg in &a {
                prod *= num(arg);
            }
            Ok(Value::Number(prod))
        }),
        sig("/", n.clone(), fixed(&[n.clone(), n.clone()]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            let (x, y) = (num(&a[0]), num(&a[1]));
            if y == 0.0 {
                if x == 0.0 {
                    return Ok(Value::Number(f64::NAN));
                }
                if x > 0.0 {
                    return Ok(Value::Number(f64::INFINITY));
                }
                if x < 0.0 {
                    return Ok(Value::Number(f64::NEG_INFINITY));
                }
            }
            Ok(Value::Number(x / y))
        }),
        sig("%", n.clone(), fixed(&[n.clone(), n.clone()]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            // Rust's float `%` is C fmod.
            Ok(Value::Number(num(&a[0]) % num(&a[1])))
        }),
        sig("^", n.clone(), fixed(&[n.clone(), n.clone()]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            Ok(Value::Number(num(&a[0]).powf(num(&a[1]))))
        }),
        sig("sqrt", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| math1(p, args, f64::sqrt)),
        sig("log10", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| math1(p, args, f64::log10)),
        sig("ln", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| math1(p, args, f64::ln)),
        sig("log2", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| math1(p, args, f64::log2)),
        sig("sin", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| math1(p, args, f64::sin)),
        sig("cos", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| math1(p, args, f64::cos)),
        sig("tan", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| math1(p, args, f64::tan)),
        sig("asin", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| math1(p, args, f64::asin)),
        sig("acos", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| math1(p, args, f64::acos)),
        sig("atan", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| math1(p, args, f64::atan)),
        sig("min", n.clone(), Params::Varargs(n.clone()), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            let mut result = f64::INFINITY;
            for arg in &a {
                result = fmin(num(arg), result);
            }
            Ok(Value::Number(result))
        }),
        sig("max", n.clone(), Params::Varargs(n.clone()), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            let mut result = f64::NEG_INFINITY;
            for arg in &a {
                result = fmax(num(arg), result);
            }
            Ok(Value::Number(result))
        }),
        // C `round` rounds half away from zero, as f64::round does.
        sig("round", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| math1(p, args, f64::round)),
        sig("floor", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| math1(p, args, f64::floor)),
        sig("ceil", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| math1(p, args, f64::ceil)),
        sig("abs", n.clone(), fixed(&[n.clone()]), D::NONE, |p, args| math1(p, args, f64::abs)),
        sig("!", T::Boolean, fixed(&[T::Boolean]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            Ok(Value::Bool(!boolean(&a[0])))
        }),
        sig("is-supported-script", T::Boolean, fixed(&[s.clone()]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            Ok(Value::Bool(is_supported_script(p, string(&a[0]))))
        }),
        sig("upcase", s.clone(), fixed(&[s.clone()]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            Ok(Value::String(uppercase(string(&a[0]))))
        }),
        sig("downcase", s.clone(), fixed(&[s.clone()]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            Ok(Value::String(lowercase(string(&a[0]))))
        }),
        sig("concat", s.clone(), Params::Varargs(T::Value), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            let mut s = String::new();
            for arg in &a {
                s += &to_string(arg);
            }
            Ok(Value::String(s))
        }),
        sig("split", T::array(s.clone()), fixed(&[s.clone(), s.clone()]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            let (input, delimiter) = (string(&a[0]), string(&a[1]));
            let mut result = Vec::new();

            if delimiter.is_empty() {
                let mut remaining = input;
                while !remaining.is_empty() {
                    let character_length = get_unicode_character_offset_on_validated_utf8(remaining, 1);
                    result.push(Value::String(remaining[..character_length].to_owned()));
                    remaining = &remaining[character_length..];
                }
                let _ = unicode_length_on_validated_utf8(input);
                return Ok(Value::Array(result));
            }

            let mut start = 0;
            loop {
                match input[start..].find(delimiter) {
                    None => {
                        result.push(Value::String(input[start..].to_owned()));
                        break;
                    }
                    Some(offset) => {
                        let position = start + offset;
                        result.push(Value::String(input[start..position].to_owned()));
                        start = position + delimiter.len();
                    }
                }
            }
            Ok(Value::Array(result))
        }),
        sig("join", s.clone(), fixed(&[T::array(s.clone()), s.clone()]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            let Value::Array(input) = &a[0] else { panic!("array argument") };
            let input: Vec<&str> = input.iter().map(string).collect();
            Ok(Value::String(input.join(string(&a[1]))))
        }),
        sig("resolved-locale", s.clone(), fixed(&[T::Collator]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            let Value::Collator(collator) = &a[0] else { panic!("collator argument") };
            Ok(Value::String(Collator::resolved_locale(collator)))
        }),
        sig("error", T::Error, fixed(&[s.clone()]), D::NONE, |p, args| {
            let a = eval_args(p, args)?;
            Err(EvaluationError::new(string(&a[0])))
        }),
        sig("feature-state", T::Value, fixed(&[s.clone()]), D::FEATURE, |p, args| {
            let a = eval_args(p, args)?;
            let mut state = MbglValue::Null;
            if let Some(feature_state) = p.feature_state {
                if let Some(v) = feature_state.get(string(&a[0])) {
                    state = v.clone();
                }
            }
            Ok(mbgl_to_expression_value(&state))
        }),
        sig("global-state", T::Value, fixed(&[s.clone()]), D::GLOBAL_STATE, |p, args| {
            let a = eval_args(p, args)?;
            let mut state = MbglValue::Null;
            if let Some(global_state) = p.global_state {
                if let Some(v) = global_state.get(string(&a[0])) {
                    state = v.clone();
                }
            }
            Ok(mbgl_to_expression_value(&state))
        }),
        // Legacy Filters
        sig("filter-==", T::Boolean, fixed(&[s.clone(), T::Value]), D::FEATURE, |p, args| {
            let a = eval_args(p, args)?;
            let rhs = feature_property_as_expression_value(p, string(&a[0]));
            Ok(Value::Bool(rhs.is_some_and(|rhs| a[1] == rhs)))
        }),
        sig("filter-id-==", T::Boolean, fixed(&[T::Value]), D::FEATURE, |p, args| {
            let a = eval_args(p, args)?;
            Ok(Value::Bool(a[0] == feature_id_as_expression_value(p)))
        }),
        sig("filter-type-==", T::Boolean, fixed(&[s.clone()]), D::FEATURE, |p, args| {
            let a = eval_args(p, args)?;
            let Some(feature) = p.feature else {
                return Ok(Value::Bool(false));
            };
            Ok(Value::Bool(feature_type_as_string(feature.get_type()).as_deref() == Some(string(&a[0]))))
        }),
        sig("filter-<", T::Boolean, fixed(&[s.clone(), T::Value]), D::FEATURE, |p, args| {
            filter_property(p, args, PartialOrd::lt, PartialOrd::lt)
        }),
        sig("filter-id-<", T::Boolean, fixed(&[T::Value]), D::FEATURE, |p, args| {
            filter_id(p, args, PartialOrd::lt, PartialOrd::lt)
        }),
        sig("filter->", T::Boolean, fixed(&[s.clone(), T::Value]), D::FEATURE, |p, args| {
            filter_property(p, args, PartialOrd::gt, PartialOrd::gt)
        }),
        sig("filter-id->", T::Boolean, fixed(&[T::Value]), D::FEATURE, |p, args| {
            filter_id(p, args, PartialOrd::gt, PartialOrd::gt)
        }),
        sig("filter-<=", T::Boolean, fixed(&[s.clone(), T::Value]), D::FEATURE, |p, args| {
            filter_property(p, args, PartialOrd::le, PartialOrd::le)
        }),
        sig("filter-id-<=", T::Boolean, fixed(&[T::Value]), D::FEATURE, |p, args| {
            filter_id(p, args, PartialOrd::le, PartialOrd::le)
        }),
        sig("filter->=", T::Boolean, fixed(&[s.clone(), T::Value]), D::FEATURE, |p, args| {
            filter_property(p, args, PartialOrd::ge, PartialOrd::ge)
        }),
        sig("filter-id->=", T::Boolean, fixed(&[T::Value]), D::FEATURE, |p, args| {
            filter_id(p, args, PartialOrd::ge, PartialOrd::ge)
        }),
        sig("filter-has", T::Boolean, fixed(&[s.clone()]), D::FEATURE, |p, args| {
            let a = eval_args(p, args)?;
            let feature = p.feature.expect("feature");
            Ok(Value::Bool(feature.get_value(string(&a[0])).is_some()))
        }),
        sig("filter-has-id", T::Boolean, fixed(&[]), D::FEATURE, |p, _| {
            let feature = p.feature.expect("feature");
            Ok(Value::Bool(!matches!(feature.get_id(), FeatureIdentifier::Null)))
        }),
        sig("filter-type-in", T::Boolean, Params::Varargs(s.clone()), D::FEATURE, |p, args| {
            let a = eval_args(p, args)?;
            let feature = p.feature.expect("feature");
            let t = feature_type_as_string(feature.get_type());
            Ok(Value::Bool(a.iter().any(|v| t.as_deref() == Some(string(v)))))
        }),
        sig("filter-id-in", T::Boolean, Params::Varargs(T::Value), D::FEATURE, |p, args| {
            let a = eval_args(p, args)?;
            let id = feature_id_as_expression_value(p);
            Ok(Value::Bool(a.contains(&id)))
        }),
        sig("filter-in", T::Boolean, Params::Varargs(T::Value), D::FEATURE, |p, args| {
            let a = eval_args(p, args)?;
            if a.len() < 2 {
                return Ok(Value::Bool(false));
            }
            let value = feature_property_as_expression_value(p, string(&a[0]));
            Ok(Value::Bool(value.is_some_and(|value| a[1..].contains(&value))))
        }),
    ]
});

/// `compoundExpressionRegistry.equal_range(name)`, in definition order.
fn definitions(name: &str) -> Vec<&'static SignatureBase> {
    REGISTRY.iter().filter(|s| s.name == name).collect()
}

pub struct CompoundExpression {
    base: ExpressionBase,
    signature: &'static SignatureBase,
    args: Vec<Box<dyn Expression>>,
}

impl CompoundExpression {
    pub fn new(signature: &'static SignatureBase, args: Vec<Box<dyn Expression>>) -> Self {
        CompoundExpression {
            base: ExpressionBase::new(
                Kind::CompoundExpression,
                signature.result.clone(),
                signature.dependencies | collect_dependencies(&args),
            ),
            signature,
            args,
        }
    }

    pub fn get_parameter_count(&self) -> Option<usize> {
        match &self.signature.params {
            Params::Varargs(_) => None,
            Params::Fixed(p) => Some(p.len()),
        }
    }

    pub fn exists(name: &str) -> bool {
        REGISTRY.iter().any(|s| s.name == name)
    }
}

impl Expression for CompoundExpression {
    fn base(&self) -> &ExpressionBase {
        &self.base
    }

    fn evaluate(&self, params: &EvaluationContext<'_>) -> EvaluationResult {
        self.signature.apply(params, &self.args)
    }

    fn each_child<'a>(&'a self, visit: &mut dyn FnMut(&'a dyn Expression)) {
        for e in &self.args {
            visit(&**e);
        }
    }

    fn equals(&self, e: &dyn Expression) -> bool {
        if e.get_kind() == Kind::CompoundExpression {
            let rhs = downcast::<CompoundExpression>(e).expect("CompoundExpression");
            return self.signature.name == rhs.signature.name
                && self.signature.result == rhs.signature.result
                && self.signature.params == rhs.signature.params
                && children_equal(&self.args, &rhs.args);
        }
        false
    }

    fn possible_outputs(&self) -> Vec<Option<Value>> {
        // Port-only: "error" never outputs a value, as the `Error` it
        // serializes from says.
        if self.signature.name == "error" {
            return Vec::new();
        }
        vec![None]
    }

    fn get_operator(&self) -> String {
        self.signature.name.to_owned()
    }
}

fn expected_types_error(definitions: &[&'static SignatureBase], args: &[Box<dyn Expression>]) -> String {
    // Only used if there are no overloads with matching number of args
    let mut available_overloads: Vec<String> = Vec::new();
    let mut overloads: Vec<String> = Vec::new();
    for signature in definitions {
        match &signature.params {
            Params::Varargs(t) => {
                overloads.push(format!("({})", type_::to_string(t)));
            }
            Params::Fixed(params) => {
                let overload = format!("({})", params.iter().map(type_::to_string).collect::<Vec<_>>().join(", "));
                if params.len() == args.len() {
                    overloads.push(overload);
                } else {
                    available_overloads.push(overload);
                }
            }
        }
    }

    let signatures = if overloads.is_empty() { available_overloads.join(" | ") } else { overloads.join(" | ") };

    let actual_types = args.iter().map(|arg| type_::to_string(arg.get_type())).collect::<Vec<_>>().join(", ");

    format!("Expected arguments of type {signatures}, but found ({actual_types}) instead.")
}

fn create_compound_expression_from(
    definitions: &[&'static SignatureBase],
    args: Vec<Box<dyn Expression>>,
    ctx: &mut ParsingContext,
) -> ParseResult {
    let mut signature_context = ParsingContext::with_key(ctx.get_key());

    for signature in definitions {
        signature_context.clear_errors();

        match &signature.params {
            Params::Fixed(params) => {
                if params.len() != args.len() {
                    signature_context.error(format!(
                        "Expected {} arguments, but found {} instead.",
                        params.len(),
                        args.len()
                    ));
                    continue;
                }

                for (i, arg) in args.iter().enumerate() {
                    if let Some(err) = check_subtype(&params[i], arg.get_type()) {
                        signature_context.error_at(err, i + 1);
                    }
                }
            }
            Params::Varargs(param_type) => {
                for (i, arg) in args.iter().enumerate() {
                    if let Some(err) = check_subtype(param_type, arg.get_type()) {
                        signature_context.error_at(err, i + 1);
                    }
                }
            }
        }

        if !signature_context.has_errors() {
            return Some(Box::new(CompoundExpression::new(signature, args)));
        }
    }

    if definitions.len() == 1 {
        ctx.append_errors(signature_context);
    } else {
        ctx.error(expected_types_error(definitions, &args));
    }

    None
}

pub fn parse_compound_expression(name: &str, value: &Convertible<'_>, ctx: &mut ParsingContext) -> ParseResult {
    debug_assert!(value.is_array() && value.array_length() > 0);

    let definitions = definitions(name);
    if definitions.is_empty() {
        ctx.error_at(
            format!(r#"Unknown expression "{name}". If you wanted a literal array, use ["literal", [...]]."#),
            0,
        );
        return None;
    }

    let length = value.array_length();

    // Per the style specification, the "global-state" property name must be
    // a string literal, not the result of a sub-expression.
    if name == "global-state" && length == 2 && value.array_member(1).to_string().is_none() {
        ctx.error_at("Global state property must be a string literal.".to_owned(), 1);
        return None;
    }

    // Each failed overload parses its arguments, so two overloads of one arity
    // would make parsing a nested chain of them exponential in its depth.
    let mut overload_matched_arity = false;
    for signature in &definitions {
        let matches = match &signature.params {
            Params::Varargs(_) => true,
            Params::Fixed(p) => p.len() == length - 1,
        };
        if matches {
            debug_assert!(!overload_matched_arity);
            overload_matched_arity = true;

            // First parse all the args, potentially coercing to the types
            // expected by this overload.
            ctx.clear_errors();
            let mut arg_parse_failed = false;
            let mut args = Vec::with_capacity(length - 1);

            for i in 1..length {
                let expected = match &signature.params {
                    Params::Varargs(t) => t.clone(),
                    Params::Fixed(params) => params[i - 1].clone(),
                };

                let Some(parsed) = ctx.parse(&value.array_member(i), i, Some(expected), None) else {
                    arg_parse_failed = true;
                    break;
                };
                args.push(parsed);
            }
            if arg_parse_failed {
                // Couldn't coerce args of this overload to expected type,
                // move on to next one.
                continue;
            } else {
                let parse_with_args = create_compound_expression_from(&definitions, args, ctx);
                if parse_with_args.is_some() {
                    return parse_with_args;
                }
            }
        }
    }
    // The overload that takes this many arguments failed, and its errors say
    // why. Parsing the arguments again would double the work at every level.
    if overload_matched_arity {
        return None;
    }

    // No overload takes this many arguments. Parse the arguments without
    // expected types just for the error message
    ctx.clear_errors();
    let mut args = Vec::with_capacity(length - 1);

    for i in 1..length {
        let parsed = ctx.parse(&value.array_member(i), i, None, None)?;
        args.push(parsed);
    }

    ctx.error(expected_types_error(&definitions, &args));

    None
}

pub fn create_compound_expression(name: &str, args: Vec<Box<dyn Expression>>, ctx: &mut ParsingContext) -> ParseResult {
    create_compound_expression_from(&definitions(name), args, ctx)
}
