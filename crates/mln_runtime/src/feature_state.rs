//! The feature state that a map keeps for its sources' features.

use std::collections::BTreeMap;

use feature::{Identifier, ValueObject};
use mln_util::error::Error;

/// The state of one feature: keys and their values.
pub type FeatureState = ValueObject;

/// The features whose state to set, read or remove.
#[derive(Clone, Debug, PartialEq)]
pub struct FeatureStateSelector {
    pub source_id: String,
    /// The source layer, for a vector source. `None`, like an empty string,
    /// names no source layer.
    pub source_layer_id: Option<String>,
    /// The feature. Setting and reading state need it. Without it, a removal
    /// removes the state of every feature in the source layer.
    ///
    /// The map compares IDs as text, so a number names the same feature as
    /// the string of its shortest decimal form: 7, 7.0 and "7" name one
    /// feature.
    pub feature_id: Option<Identifier>,
    /// The one key that a removal removes. It needs `feature_id`. `None`,
    /// like an empty string, removes every key.
    pub state_key: Option<String>,
}

impl FeatureStateSelector {
    /// Selects the feature `feature_id` of the source `source_id`.
    pub fn feature(source_id: impl Into<String>, feature_id: Identifier) -> Self {
        FeatureStateSelector {
            source_id: source_id.into(),
            source_layer_id: None,
            feature_id: Some(feature_id),
            state_key: None,
        }
    }

    pub fn with_source_layer(mut self, source_layer_id: impl Into<String>) -> Self {
        self.source_layer_id = Some(source_layer_id.into());
        self
    }

    pub fn with_state_key(mut self, state_key: impl Into<String>) -> Self {
        self.state_key = Some(state_key.into());
        self
    }

    /// The selector's keys in the store, checked: `needs_feature` for a set
    /// or a read.
    pub(crate) fn key(&self, needs_feature: bool) -> Result<SelectorKey, Error> {
        if self.source_id.is_empty() {
            return Err(Error::invalid_argument("feature state source_id must not be empty"));
        }
        let feature_id = match &self.feature_id {
            None => None,
            Some(Identifier::Null) => {
                return Err(Error::invalid_argument("feature state feature_id must not be null"));
            }
            Some(Identifier::Double(value)) if !value.is_finite() => {
                return Err(Error::invalid_argument("feature_id double_value must be finite"));
            }
            Some(id) => mln_util::feature::feature_id_to_string(id),
        };
        let state_key = self.state_key.clone().filter(|key| !key.is_empty());
        if feature_id.is_none() {
            if needs_feature {
                return Err(Error::invalid_argument("feature state selector requires feature_id"));
            }
            if state_key.is_some() {
                return Err(Error::invalid_argument("feature state selector state_key requires feature_id"));
            }
        }
        Ok(SelectorKey {
            source_id: self.source_id.clone(),
            source_layer_id: self.source_layer_id.clone().unwrap_or_default(),
            feature_id,
            state_key,
        })
    }
}

/// A checked selector: the empty source layer stands for none.
#[derive(Clone, Debug)]
pub(crate) struct SelectorKey {
    source_id: String,
    source_layer_id: String,
    feature_id: Option<String>,
    state_key: Option<String>,
}

type Features = BTreeMap<String, FeatureState>;
type Layers = BTreeMap<String, Features>;

/// A map's feature state, by source, source layer and feature ID. It keeps
/// no empty entry, so a removal leaves no trace.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FeatureStateStore {
    sources: BTreeMap<String, Layers>,
}

impl FeatureStateStore {
    /// Merges `state` into the feature's state. Returns whether anything
    /// changed.
    pub(crate) fn set(&mut self, key: SelectorKey, state: FeatureState) -> bool {
        let Some(feature_id) = key.feature_id else { return false };
        let feature = self
            .sources
            .entry(key.source_id)
            .or_default()
            .entry(key.source_layer_id)
            .or_default()
            .entry(feature_id)
            .or_default();
        let mut changed = false;
        for (name, value) in state {
            if feature.get(&name) != Some(&value) {
                feature.insert(name, value);
                changed = true;
            }
        }
        changed
    }

    /// The feature's state, empty when it has none.
    pub(crate) fn get(&self, key: &SelectorKey) -> FeatureState {
        key.feature_id
            .as_ref()
            .and_then(|feature_id| self.sources.get(&key.source_id)?.get(&key.source_layer_id)?.get(feature_id))
            .cloned()
            .unwrap_or_default()
    }

    /// Removes the state that `key` selects. Returns whether anything
    /// changed.
    pub(crate) fn remove(&mut self, key: &SelectorKey) -> bool {
        let Some(layers) = self.sources.get_mut(&key.source_id) else { return false };
        let changed = match &key.feature_id {
            None => layers.remove(&key.source_layer_id).is_some(),
            Some(feature_id) => {
                let Some(features) = layers.get_mut(&key.source_layer_id) else { return false };
                let changed = match &key.state_key {
                    None => features.remove(feature_id).is_some(),
                    Some(state_key) => {
                        features.get_mut(feature_id).is_some_and(|feature| feature.remove(state_key).is_some())
                    }
                };
                if features.get(feature_id).is_some_and(FeatureState::is_empty) {
                    features.remove(feature_id);
                }
                if features.is_empty() {
                    layers.remove(&key.source_layer_id);
                }
                changed
            }
        };
        if layers.is_empty() {
            self.sources.remove(&key.source_id);
        }
        changed
    }

    /// Removes the state of every feature of the source `source_id`.
    pub(crate) fn remove_source(&mut self, source_id: &str) -> bool {
        self.sources.remove(source_id).is_some()
    }

    /// Removes every feature's state.
    pub(crate) fn clear(&mut self) -> bool {
        let changed = !self.sources.is_empty();
        self.sources.clear();
        changed
    }
}

#[cfg(test)]
mod tests {
    use feature::Value;

    use super::*;

    fn state(entries: &[(&str, Value)]) -> FeatureState {
        entries.iter().map(|(key, value)| ((*key).to_owned(), value.clone())).collect()
    }

    fn text(id: &str) -> Identifier {
        Identifier::String(id.to_owned())
    }

    fn key(selector: &FeatureStateSelector) -> SelectorKey {
        selector.key(false).unwrap()
    }

    #[test]
    fn state_merges_on_set_and_prunes_on_remove() {
        let mut store = FeatureStateStore::default();
        let feature = FeatureStateSelector::feature("points", text("feature-1"));
        assert!(store.set(key(&feature), state(&[("hover", Value::Bool(true))])));
        assert!(store.set(key(&feature), state(&[("count", Value::Uint(3))])));
        assert!(!store.set(key(&feature), state(&[("count", Value::Uint(3))])));
        assert_eq!(state(&[("hover", Value::Bool(true)), ("count", Value::Uint(3))]), store.get(&key(&feature)));

        assert!(store.remove(&key(&feature.clone().with_state_key("hover"))));
        assert!(!store.remove(&key(&feature.clone().with_state_key("hover"))));
        assert!(store.remove(&key(&feature.clone().with_state_key("count"))));
        assert_eq!(FeatureStateStore::default(), store);
    }

    #[test]
    fn a_removal_without_a_feature_takes_its_source_layer() {
        let mut store = FeatureStateStore::default();
        let roads = FeatureStateSelector::feature("tiles", text("1")).with_source_layer("roads");
        let water = FeatureStateSelector::feature("tiles", text("1")).with_source_layer("water");
        let bare = FeatureStateSelector::feature("tiles", text("1"));
        for selector in [&roads, &water, &bare] {
            store.set(key(selector), state(&[("n", Value::Uint(1))]));
        }
        // The empty source layer is no source layer.
        assert_eq!(store.get(&key(&bare)), store.get(&key(&bare.clone().with_source_layer(""))));

        let all_roads = FeatureStateSelector { feature_id: None, ..roads.clone() };
        assert!(store.remove(&key(&all_roads)));
        assert!(store.get(&key(&roads)).is_empty());
        assert!(!store.get(&key(&water)).is_empty());
        assert!(store.remove_source("tiles"));
        assert!(!store.clear());
    }

    #[test]
    fn numbers_name_the_feature_of_their_text() {
        let mut store = FeatureStateStore::default();
        store.set(key(&FeatureStateSelector::feature("points", Identifier::Uint(7))), state(&[("n", Value::Uint(1))]));
        for id in [Identifier::String("7".into()), Identifier::Int(7), Identifier::Double(7.0)] {
            assert!(!store.get(&key(&FeatureStateSelector::feature("points", id))).is_empty());
        }
        assert!(store.get(&key(&FeatureStateSelector::feature("points", text("7.0")))).is_empty());
        store.set(
            key(&FeatureStateSelector::feature("points", Identifier::Double(1e21))),
            state(&[("n", Value::Uint(2))]),
        );
        assert!(!store.get(&key(&FeatureStateSelector::feature("points", text("1e21")))).is_empty());
    }

    #[test]
    fn selectors_check_what_each_use_needs() {
        let message = |selector: FeatureStateSelector, needs_feature| {
            selector.key(needs_feature).unwrap_err().message.to_string()
        };
        let feature = FeatureStateSelector::feature("points", text("a"));
        assert_eq!(
            "feature state source_id must not be empty",
            message(FeatureStateSelector { source_id: String::new(), ..feature.clone() }, false)
        );
        let source = FeatureStateSelector { feature_id: None, ..feature.clone() };
        assert_eq!("feature state selector requires feature_id", message(source.clone(), true));
        assert_eq!("feature state selector state_key requires feature_id", message(source.with_state_key("k"), false));
        assert!(FeatureStateSelector { feature_id: None, ..feature.clone() }.with_state_key("").key(false).is_ok());
        assert_eq!(
            "feature_id double_value must be finite",
            message(FeatureStateSelector::feature("points", Identifier::Double(f64::NAN)), true)
        );
    }
}
