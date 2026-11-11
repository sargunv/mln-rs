// Port-only: a front end of Convertible over `mln::Value` itself, so that
// what `Layer::serialize()` returns converts back without a detour through
// JSON text. A tile worker in another wasm instance gets its layers this way
// (mln_style's serialization.rs). It reads a value
// as the JSON front end reads the JSON that the value stringifies to, except
// that a non-finite number stays a number (JSON has no form for it).

use feature::Value;

use crate::conversion::{ConversionTraits, Convertible, Error};

/// What a missing member reads as, as in the JSON front end.
static NULL: Value = Value::Null;

impl ConversionTraits for Value {
    fn is_undefined(&self) -> bool {
        matches!(self, Value::Null)
    }

    fn is_array(&self) -> bool {
        matches!(self, Value::Array(_))
    }

    fn array_length(&self) -> usize {
        match self {
            Value::Array(a) => a.len(),
            _ => 0,
        }
    }

    fn array_member(&self, i: usize) -> Convertible<'_> {
        match self {
            Value::Array(a) => Convertible::new(a.get(i).unwrap_or(&NULL)),
            _ => Convertible::new(&NULL),
        }
    }

    fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }

    fn object_member(&self, name: &str) -> Option<Convertible<'_>> {
        match self {
            Value::Object(o) => o.get(name).map(|m| Convertible::new(m)),
            _ => None,
        }
    }

    fn each_member(&self, f: &mut dyn FnMut(&str, &Convertible<'_>) -> Option<Error>) -> Option<Error> {
        let Value::Object(o) = self else {
            return None;
        };
        for (name, value) in o {
            let result = f(name, &Convertible::new(value));
            if result.is_some() {
                return result;
            }
        }
        None
    }

    fn to_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    fn to_number(&self) -> Option<f32> {
        self.to_double().map(|d| d as f32)
    }

    fn to_double(&self) -> Option<f64> {
        match self {
            Value::Uint(u) => Some(*u as f64),
            Value::Int(i) => Some(*i as f64),
            Value::Double(d) => Some(*d),
            _ => None,
        }
    }

    fn to_string(&self) -> Option<String> {
        match self {
            Value::String(s) => Some(s.clone()),
            _ => None,
        }
    }

    fn to_value(&self) -> Option<Value> {
        match self {
            // As the JSON front end: null converts to `false`.
            Value::Null => Some(Value::Bool(false)),
            Value::Bool(_) | Value::Uint(_) | Value::Int(_) | Value::Double(_) | Value::String(_) => Some(self.clone()),
            Value::Array(_) | Value::Object(_) => None,
        }
    }

    /// The JSON front end's conversion of the JSON the value stringifies to
    /// (the GeoJSON argument of "within" and "distance").
    fn to_geojson(&self, error: &mut Error) -> Option<geojson::GeoJSON> {
        crate::json_conversion::to_geojson(&value_to_json(self), error)
    }
}

/// The JSON form of a value. A non-finite double has no JSON form and
/// becomes `null`.
pub fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Uint(u) => serde_json::Value::from(*u),
        Value::Int(i) => serde_json::Value::from(*i),
        Value::Double(d) => serde_json::Number::from_f64(*d).map_or(serde_json::Value::Null, serde_json::Value::Number),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Array(a) => serde_json::Value::Array(a.iter().map(value_to_json).collect()),
        Value::Object(o) => serde_json::Value::Object(o.iter().map(|(k, v)| (k.clone(), value_to_json(v))).collect()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A value reads as the JSON it stringifies to.
    #[test]
    fn reads_as_its_json() {
        let json = json!({"a": [1, -2, 3.5, "s", true, null], "b": {"c": 1}});
        let value = Value::Object(
            [
                (
                    "a".to_owned(),
                    Value::Array(vec![
                        Value::Uint(1),
                        Value::Int(-2),
                        Value::Double(3.5),
                        Value::String("s".to_owned()),
                        Value::Bool(true),
                        Value::Null,
                    ]),
                ),
                ("b".to_owned(), Value::Object([("c".to_owned(), Value::Uint(1))].into())),
            ]
            .into(),
        );
        let (j, v) = (Convertible::from(&json), Convertible::from(&value));
        assert_eq!(j.is_object(), v.is_object());
        assert!(v.object_member("missing").is_none());
        let (ja, va) = (j.object_member("a").unwrap(), v.object_member("a").unwrap());
        assert_eq!(ja.array_length(), va.array_length());
        for i in 0..ja.array_length() {
            let (jm, vm) = (ja.array_member(i), va.array_member(i));
            assert_eq!(jm.is_undefined(), vm.is_undefined());
            assert_eq!(jm.to_value(), vm.to_value());
            assert_eq!(jm.to_number(), vm.to_number());
            assert_eq!(jm.to_double(), vm.to_double());
            assert_eq!(jm.to_string(), vm.to_string());
            assert_eq!(jm.to_bool(), vm.to_bool());
        }
        let mut names = Vec::new();
        v.each_member(|name, _| {
            names.push(name.to_owned());
            None
        });
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn keeps_non_finite_numbers() {
        assert!(Convertible::from(&Value::Double(f64::NAN)).to_number().unwrap().is_nan());
        assert_eq!(Convertible::from(&Value::Double(f64::INFINITY)).to_double(), Some(f64::INFINITY));
    }
}
