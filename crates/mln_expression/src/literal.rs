// Ports include/mln/style/expression/literal.hpp and
// src/mln/style/expression/literal.cpp.

use feature::Value as MbglValue;
use mln_conversion::Convertible;

use crate::expression::{Dependency, EvaluationContext, EvaluationResult, Expression, ExpressionBase, Kind, downcast};
use crate::parsing_context::{ParseResult, ParsingContext};
use crate::type_::{self, Type};
use crate::value::{ObjectType, Value, from_mbgl_value, to_mbgl_value, type_of};

pub struct Literal {
    base: ExpressionBase,
    value: Value,
}

fn check_number(n: f64) -> Option<Value> {
    if n > f64::MAX { Some(Value::Number(f64::INFINITY)) } else { Some(Value::Number(n)) }
}

/// `parseValue` of literal.cpp.
// C++'s signature: `ctx` is only passed on to the recursive calls.
#[allow(clippy::only_used_in_recursion)]
pub(crate) fn parse_value(value: &Convertible<'_>, ctx: &mut ParsingContext) -> Option<Value> {
    if value.is_undefined() {
        return Some(Value::Null);
    }
    if value.is_object() {
        let mut result = ObjectType::new();
        let mut error = false;
        value.each_member(|k, v| {
            if !error {
                if let Some(member_value) = parse_value(v, ctx) {
                    // unordered_map::emplace keeps the first value of a key.
                    result.entry(k.to_owned()).or_insert(member_value);
                } else {
                    error = true;
                }
            }
            None
        });
        return if error { None } else { Some(Value::Object(result)) };
    }

    if value.is_array() {
        let mut result = Vec::new();
        let length = value.array_length();
        for i in 0..length {
            let item = parse_value(&value.array_member(i), ctx)?;
            result.push(item);
        }
        return Some(Value::Array(result));
    }

    // since value represents a JSON value, if it's not undefined, object, or
    // array, it must be convertible to mln::Value
    let v = value.to_value().expect("to_value of a JSON scalar");

    match v {
        MbglValue::Uint(n) => check_number(n as f64),
        MbglValue::Int(n) => check_number(n as f64),
        MbglValue::Double(n) => check_number(n),
        v => Some(from_mbgl_value(&v)),
    }
}

impl Literal {
    pub fn new(value: Value) -> Self {
        Literal { base: ExpressionBase::new(Kind::Literal, type_of(&value), Dependency::NONE), value }
    }

    pub fn with_array_type(type_: type_::Array, value: Vec<Value>) -> Self {
        Literal { base: ExpressionBase::new(Kind::Literal, type_.into(), Dependency::NONE), value: Value::Array(value) }
    }

    pub fn get_value(&self) -> &Value {
        &self.value
    }

    pub fn parse(value: &Convertible<'_>, ctx: &mut ParsingContext) -> ParseResult {
        if value.is_object() {
            ctx.error(r#"Bare objects invalid. Use ["literal", {...}] instead."#.to_owned());
            None
        } else if value.is_array() {
            // object or array value, quoted with ["literal", value]
            if value.array_length() != 2 {
                ctx.error(format!(
                    "'literal' expression requires exactly one argument, but found {} instead.",
                    value.array_length() as u64 - 1
                ));
                return None;
            }
            let parsed_value = parse_value(&value.array_member(1), ctx)?;

            // special case: infer the item type if possible for zero-length
            // arrays
            if let (Some(Type::Array(expected)), Value::Array(items)) = (ctx.get_expected(), &parsed_value) {
                if let Type::Array(t) = type_of(&parsed_value) {
                    if t.n == Some(0) && (expected.n.is_none() || expected.n == Some(0)) {
                        return Some(Box::new(Literal::with_array_type(*expected, items.clone())));
                    }
                }
            }

            Some(Box::new(Literal::new(parsed_value)))
        } else {
            // bare primitive value (string, number, boolean, null)
            let parsed_value = parse_value(value, ctx).expect("primitive literal");
            Some(Box::new(Literal::new(parsed_value)))
        }
    }
}

impl Expression for Literal {
    fn base(&self) -> &ExpressionBase {
        &self.base
    }

    fn evaluate(&self, _: &EvaluationContext<'_>) -> EvaluationResult {
        Ok(self.value.clone())
    }

    fn literal_value(&self) -> Option<&Value> {
        Some(&self.value)
    }

    fn each_child<'a>(&'a self, _: &mut dyn FnMut(&'a dyn Expression)) {}

    fn equals(&self, e: &dyn Expression) -> bool {
        if e.get_kind() == Kind::Literal {
            let rhs = downcast::<Literal>(e).expect("Literal");
            return self.value == rhs.value;
        }
        false
    }

    fn possible_outputs(&self) -> Vec<Option<Value>> {
        vec![Some(self.value.clone())]
    }

    fn serialize(&self) -> MbglValue {
        // Port-only: C++ writes an image as `Image::toValue()`, an object
        // that doesn't parse back. The legacy property functions make
        // literal images (zoom functions of `*-pattern` and `icon-image`),
        // where an image is expected, so they serialize as the image's
        // name, which parses back as the implicit coercion to an image.
        // Like the literal, and unlike an "image" expression, that is
        // runtime constant, so the property evaluates the same.
        if let Value::Image(image) = &self.value {
            return MbglValue::String(image.id().to_owned());
        }
        // Port-only: C++ writes a collection as a bare array (`["top", [0,
        // 1]]`), which parses as an expression with an unknown operator. As
        // a literal, it converts back to the collection.
        if self.get_type().is_array()
            || *self.get_type() == Type::Object
            || *self.get_type() == Type::VariableAnchorOffsetCollection
        {
            MbglValue::Array(vec![MbglValue::String(self.get_operator()), to_mbgl_value(&self.value)])
        } else {
            to_mbgl_value(&self.value)
        }
    }

    fn get_operator(&self) -> String {
        "literal".to_owned()
    }
}
