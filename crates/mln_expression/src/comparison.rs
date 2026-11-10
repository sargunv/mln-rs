// Ports include/mln/style/expression/comparison.hpp and
// src/mln/style/expression/comparison.cpp.

use mln_conversion::Convertible;

use crate::assertion::Assertion;
use crate::collator::Collator;
use crate::expression::{
    EvaluationContext, EvaluationError, EvaluationResult, Expression, ExpressionBase, Kind, downcast, evaluate_ref,
};
use crate::parsing_context::{ParseResult, ParsingContext};
use crate::type_::{self, Type};
use crate::value::{Value, type_of};

fn is_comparable_type(op: &str, t: &Type) -> bool {
    if op == "==" || op == "!=" {
        *t == Type::String || *t == Type::Number || *t == Type::Boolean || *t == Type::Null || *t == Type::Value
    } else {
        *t == Type::String || *t == Type::Number || *t == Type::Value
    }
}

type CompareFunctionType = fn(&Value, &Value) -> bool;
type CollatorCompareFunctionType = fn(&str, &str, &Collator) -> bool;

fn eq(a: &Value, b: &Value) -> bool {
    a == b
}
fn neq(a: &Value, b: &Value) -> bool {
    a != b
}

/// `lhs.match([&](const std::string& a) {...}, [&](double a) {...})` for the
/// ordering operators.
fn ordered(lhs: &Value, rhs: &Value, s: fn(&str, &str) -> bool, d: fn(f64, f64) -> bool) -> bool {
    match lhs {
        Value::String(a) => s(a, rhs.as_str().expect("string operand")),
        Value::Number(a) => d(*a, rhs.as_number().expect("number operand")),
        _ => {
            debug_assert!(false);
            false
        }
    }
}

fn lt(lhs: &Value, rhs: &Value) -> bool {
    ordered(lhs, rhs, |a, b| a < b, |a, b| a < b)
}
fn gt(lhs: &Value, rhs: &Value) -> bool {
    ordered(lhs, rhs, |a, b| a > b, |a, b| a > b)
}
fn lteq(lhs: &Value, rhs: &Value) -> bool {
    ordered(lhs, rhs, |a, b| a <= b, |a, b| a <= b)
}
fn gteq(lhs: &Value, rhs: &Value) -> bool {
    ordered(lhs, rhs, |a, b| a >= b, |a, b| a >= b)
}

fn eq_collate(a: &str, b: &str, c: &Collator) -> bool {
    c.compare(a, b) == 0
}
fn neq_collate(a: &str, b: &str, c: &Collator) -> bool {
    !eq_collate(a, b, c)
}
fn lt_collate(a: &str, b: &str, c: &Collator) -> bool {
    c.compare(a, b) < 0
}
fn gt_collate(a: &str, b: &str, c: &Collator) -> bool {
    c.compare(a, b) > 0
}
fn lteq_collate(a: &str, b: &str, c: &Collator) -> bool {
    c.compare(a, b) <= 0
}
fn gteq_collate(a: &str, b: &str, c: &Collator) -> bool {
    c.compare(a, b) >= 0
}

fn get_basic_compare_function(op: &str) -> CompareFunctionType {
    match op {
        "==" => eq,
        "!=" => neq,
        ">" => gt,
        "<" => lt,
        ">=" => gteq,
        "<=" => lteq,
        _ => unreachable!("unknown comparison {op}"),
    }
}

fn get_collator_comparison_function(op: &str) -> CollatorCompareFunctionType {
    match op {
        "==" => eq_collate,
        "!=" => neq_collate,
        ">" => gt_collate,
        "<" => lt_collate,
        ">=" => gteq_collate,
        "<=" => lteq_collate,
        _ => unreachable!("unknown comparison {op}"),
    }
}

pub struct BasicComparison {
    base: ExpressionBase,
    op: String,
    compare: CompareFunctionType,
    lhs: Box<dyn Expression>,
    rhs: Box<dyn Expression>,
    needs_runtime_type_check: bool,
}

impl BasicComparison {
    pub fn new(op: String, lhs: Box<dyn Expression>, rhs: Box<dyn Expression>) -> Self {
        debug_assert!(is_comparable_type(&op, lhs.get_type()) && is_comparable_type(&op, rhs.get_type()));
        debug_assert!(
            lhs.get_type() == rhs.get_type() || *lhs.get_type() == Type::Value || *rhs.get_type() == Type::Value
        );

        let needs_runtime_type_check =
            (op != "==" && op != "!=") && (*lhs.get_type() == Type::Value || *rhs.get_type() == Type::Value);
        BasicComparison {
            base: ExpressionBase::new(Kind::Comparison, Type::Boolean, lhs.dependencies() | rhs.dependencies()),
            compare: get_basic_compare_function(&op),
            op,
            lhs,
            rhs,
            needs_runtime_type_check,
        }
    }
}

impl Expression for BasicComparison {
    fn base(&self) -> &ExpressionBase {
        &self.base
    }

    fn evaluate(&self, params: &EvaluationContext<'_>) -> EvaluationResult {
        let lhs_result = evaluate_ref(&*self.lhs, params)?;
        let rhs_result = evaluate_ref(&*self.rhs, params)?;

        if self.needs_runtime_type_check {
            let lhs_type = type_of(&lhs_result);
            let rhs_type = type_of(&rhs_result);
            // check that type is string or number, and equal
            if lhs_type != rhs_type || !(lhs_type == Type::String || lhs_type == Type::Number) {
                return Err(EvaluationError::new(format!(
                    r#"Expected arguments for "{}" to be (string, string) or (number, number), but found ({}, {}) instead."#,
                    self.op,
                    type_::to_string(&lhs_type),
                    type_::to_string(&rhs_type)
                )));
            }
        }

        Ok(Value::Bool((self.compare)(&lhs_result, &rhs_result)))
    }

    fn each_child<'a>(&'a self, visit: &mut dyn FnMut(&'a dyn Expression)) {
        visit(&*self.lhs);
        visit(&*self.rhs);
    }

    fn equals(&self, e: &dyn Expression) -> bool {
        // C++ static_casts any Kind::Comparison to BasicComparison.
        if e.get_kind() == Kind::Comparison {
            if let Some(comp) = downcast::<BasicComparison>(e) {
                return comp.op == self.op && comp.lhs.equals(&*self.lhs) && comp.rhs.equals(&*self.rhs);
            }
        }
        false
    }

    fn possible_outputs(&self) -> Vec<Option<Value>> {
        vec![Some(Value::Bool(true)), Some(Value::Bool(false))]
    }

    fn get_operator(&self) -> String {
        self.op.clone()
    }
}

pub struct CollatorComparison {
    base: ExpressionBase,
    op: String,
    compare: CollatorCompareFunctionType,
    lhs: Box<dyn Expression>,
    rhs: Box<dyn Expression>,
    collator: Box<dyn Expression>,
    needs_runtime_type_check: bool,
}

impl CollatorComparison {
    pub fn new(op: String, lhs: Box<dyn Expression>, rhs: Box<dyn Expression>, collator: Box<dyn Expression>) -> Self {
        debug_assert!(is_comparable_type(&op, lhs.get_type()) && is_comparable_type(&op, rhs.get_type()));
        let needs_runtime_type_check =
            (op == "==" || op == "!=") && (*lhs.get_type() == Type::Value || *rhs.get_type() == Type::Value);
        CollatorComparison {
            base: ExpressionBase::new(
                Kind::Comparison,
                Type::Boolean,
                lhs.dependencies() | rhs.dependencies() | collator.dependencies(),
            ),
            compare: get_collator_comparison_function(&op),
            op,
            lhs,
            rhs,
            collator,
            needs_runtime_type_check,
        }
    }
}

impl Expression for CollatorComparison {
    fn base(&self) -> &ExpressionBase {
        &self.base
    }

    fn evaluate(&self, params: &EvaluationContext<'_>) -> EvaluationResult {
        let lhs_result = self.lhs.evaluate(params);
        let Ok(lhs_value) = &lhs_result else {
            return lhs_result;
        };

        let rhs_result = self.rhs.evaluate(params);
        let Ok(rhs_value) = &rhs_result else {
            // Quirk kept from the C++: returns the (successful) lhs result.
            return lhs_result;
        };

        if self.needs_runtime_type_check && (type_of(lhs_value) != Type::String || type_of(rhs_value) != Type::String) {
            return Ok(Value::Bool(get_basic_compare_function(&self.op)(lhs_value, rhs_value)));
        }

        let collator_result = self.collator.evaluate(params)?;

        let Value::Collator(c) = &collator_result else { panic!("collator operand") };
        // TODO(port): util::LocaleException from the platform collator.
        Ok(Value::Bool((self.compare)(
            lhs_value.as_str().expect("string operand"),
            rhs_value.as_str().expect("string operand"),
            c,
        )))
    }

    fn each_child<'a>(&'a self, visit: &mut dyn FnMut(&'a dyn Expression)) {
        visit(&*self.lhs);
        visit(&*self.rhs);
        visit(&*self.collator);
    }

    fn equals(&self, e: &dyn Expression) -> bool {
        if e.get_kind() == Kind::Comparison {
            if let Some(comp) = downcast::<CollatorComparison>(e) {
                return comp.op == self.op
                    && comp.collator.equals(&*self.collator)
                    && comp.lhs.equals(&*self.lhs)
                    && comp.rhs.equals(&*self.rhs);
            }
        }
        false
    }

    fn possible_outputs(&self) -> Vec<Option<Value>> {
        vec![Some(Value::Bool(true)), Some(Value::Bool(false))]
    }

    fn get_operator(&self) -> String {
        self.op.clone()
    }
}

pub fn parse_comparison(value: &Convertible<'_>, ctx: &mut ParsingContext) -> ParseResult {
    let length = value.array_length();

    if length != 3 && length != 4 {
        ctx.error("Expected two or three arguments.".to_owned());
        return None;
    }

    let op = value.array_member(0).to_string().expect("operator name");

    let mut lhs = ctx.parse(&value.array_member(1), 1, Some(Type::Value), None)?;
    let lhs_type = lhs.get_type().clone();
    if !is_comparable_type(&op, &lhs_type) {
        ctx.error_at(
            format!(r#""{}" comparisons are not supported for type '{}'."#, op, type_::to_string(&lhs_type)),
            1,
        );
        return None;
    }

    let mut rhs = ctx.parse(&value.array_member(2), 2, Some(Type::Value), None)?;
    let rhs_type = rhs.get_type().clone();
    if !is_comparable_type(&op, &rhs_type) {
        ctx.error_at(
            format!(r#""{}" comparisons are not supported for type '{}'."#, op, type_::to_string(&rhs_type)),
            2,
        );
        return None;
    }

    if lhs_type != rhs_type && lhs_type != Type::Value && rhs_type != Type::Value {
        ctx.error(format!(
            "Cannot compare types '{}' and '{}'.",
            type_::to_string(&lhs_type),
            type_::to_string(&rhs_type)
        ));
        return None;
    }

    if op != "==" && op != "!=" {
        // typing rules specific to less/greater than operators
        if lhs_type == Type::Value && rhs_type != Type::Value {
            // (value, T)
            lhs = Box::new(Assertion::new(rhs_type.clone(), vec![lhs]));
        } else if lhs_type != Type::Value && rhs_type == Type::Value {
            // (T, value)
            rhs = Box::new(Assertion::new(lhs_type.clone(), vec![rhs]));
        }
    }

    if length == 4 {
        if lhs_type != Type::String && rhs_type != Type::String && lhs_type != Type::Value && rhs_type != Type::Value {
            ctx.error("Cannot use collator to compare non-string types.".to_owned());
            return None;
        }
        let collator_parse_result = ctx.parse(&value.array_member(3), 3, Some(Type::Collator), None)?;
        return Some(Box::new(CollatorComparison::new(op, lhs, rhs, collator_parse_result)));
    }

    Some(Box::new(BasicComparison::new(op, lhs, rhs)))
}
