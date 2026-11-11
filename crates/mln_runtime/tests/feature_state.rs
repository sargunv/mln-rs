// The feature state that a map keeps. They follow maplibre-native-ffi's ABI
// tests (tests/native/abi/style/feature_state.c at b20632a) where those test
// the same rules.

mod support;

use feature::{Identifier, Value};
use mln_runtime::{ErrorKind, FeatureState, FeatureStateSelector};
use support::{EMPTY_STYLE, map, runtime, wait, wait_err};

fn state(entries: &[(&str, Value)]) -> FeatureState {
    entries.iter().map(|(key, value)| ((*key).to_owned(), value.clone())).collect()
}

fn feature(id: &str) -> FeatureStateSelector {
    FeatureStateSelector::feature("points", Identifier::String(id.to_owned()))
}

#[test]
fn state_merges_on_set_and_needs_no_style() {
    let runtime = runtime();
    let map = map(&runtime);
    assert!(wait(map.feature_state(feature("a"))).is_empty());
    wait(map.set_feature_state(feature("a"), state(&[("hover", Value::Bool(true))])));
    wait(map.set_feature_state(feature("a"), state(&[("count", Value::Uint(4))])));
    assert_eq!(
        state(&[("hover", Value::Bool(true)), ("count", Value::Uint(4))]),
        wait(map.feature_state(feature("a")))
    );

    wait(map.remove_feature_state(feature("a").with_state_key("hover")));
    assert_eq!(state(&[("count", Value::Uint(4))]), wait(map.feature_state(feature("a"))));
    // A number names the feature of its text.
    let seven = FeatureStateSelector::feature("points", Identifier::Uint(7));
    wait(map.set_feature_state(seven, state(&[("n", Value::Uint(1))])));
    assert!(!wait(map.feature_state(feature("7"))).is_empty());
    let source = FeatureStateSelector { feature_id: None, ..feature("") };
    wait(map.remove_feature_state(source));
    assert!(wait(map.feature_state(feature("a"))).is_empty());
}

#[test]
fn a_style_load_and_a_removed_source_clear_the_state() {
    let runtime = runtime();
    let map = map(&runtime);
    wait(map.set_style_json(EMPTY_STYLE));
    wait(
        map.add_style_source_json("points", r#"{"type":"geojson","data":{"type":"FeatureCollection","features":[]}}"#),
    );
    let other = FeatureStateSelector::feature("other", Identifier::Uint(1));
    wait(map.set_feature_state(feature("a"), state(&[("n", Value::Uint(1))])));
    wait(map.set_feature_state(other.clone(), state(&[("n", Value::Uint(2))])));

    wait(map.remove_style_source("points"));
    assert!(wait(map.feature_state(feature("a"))).is_empty());
    assert!(!wait(map.feature_state(other.clone())).is_empty());

    wait(map.set_style_json(EMPTY_STYLE));
    assert!(wait(map.feature_state(other)).is_empty());
}

#[test]
fn selectors_are_checked_when_sent() {
    let runtime = runtime();
    let map = map(&runtime);
    let source = FeatureStateSelector { feature_id: None, ..feature("") };
    fn kind<T>(result: Result<T, mln_runtime::Error>) -> ErrorKind {
        result.map(drop).unwrap_err().kind
    }
    assert_eq!(ErrorKind::InvalidArgument, kind(map.set_feature_state(source.clone(), state(&[("n", Value::Null)]))));
    assert_eq!(ErrorKind::InvalidArgument, kind(map.feature_state(source.clone())));
    assert_eq!(ErrorKind::InvalidArgument, kind(map.remove_feature_state(source.with_state_key("n"))));
    assert_eq!(ErrorKind::InvalidArgument, kind(map.set_feature_state(feature("a"), FeatureState::new())));
    let nan = FeatureStateSelector::feature("points", Identifier::Double(f64::NAN));
    assert_eq!(ErrorKind::InvalidArgument, kind(map.feature_state(nan)));
    // The map still takes the commands after the rejected ones.
    assert_eq!(ErrorKind::InvalidState, wait_err(map.remove_style_layer("missing")).kind);
}
