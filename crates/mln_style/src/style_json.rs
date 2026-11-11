//! The style's JSON-text entry points: they take and return style-spec JSON
//! as text, so a caller on another thread hands over text and the style's
//! own thread parses it.

use feature::Value;
use mln_conversion::{Convertible, value_to_json};
use mln_util::error::Error;

use crate::conversion::{Converter, convert_layer, convert_source};
use crate::filter::Filter;
use crate::layer::layer_type_info;
use crate::light::Light;
use crate::style::Style;
use crate::style_property::Kind;

impl Style {
    /// Adds the [source](https://maplibre.org/maplibre-style-spec/sources/)
    /// that `json` describes, with the ID `id`.
    ///
    /// Fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind) while the
    /// style isn't loaded, with `AlreadyExists` if the style has a source
    /// named `id`, and with `InvalidArgument` if `json` isn't a valid source.
    pub fn add_source_json(&mut self, id: &str, json: &str) -> Result<(), Error> {
        self.require_loaded()?;
        if self.get_source(id).is_some() {
            return Err(Error::already_exists(format!("source {} already exists", quote(id))));
        }
        let context = format!("style source {}", quote(id));
        let value = parse(json, &context)?;
        let mut error = mln_conversion::Error::default();
        let Some(source) = convert_source(&Convertible::from(&value), &mut error, id) else {
            return Err(conversion_error(&context, error));
        };
        self.add_source(source)
    }

    /// Adds the [layer](https://maplibre.org/maplibre-style-spec/layers/)
    /// that `json` describes, below the layer named `before_layer_id`, or
    /// above every layer when it is `None`.
    ///
    /// Fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind) while the
    /// style isn't loaded, with `NotFound` if the style has no layer named
    /// `before_layer_id` or no source with the ID that the layer uses, with
    /// `AlreadyExists` if the style has a layer with the new layer's ID, and
    /// with `InvalidArgument` if `json` isn't a valid layer or its source
    /// can't feed a layer of its type.
    pub fn add_layer_json(&mut self, json: &str, before_layer_id: Option<&str>) -> Result<(), Error> {
        self.require_loaded()?;
        if let Some(before) = before_layer_id
            && self.get_layer(before).is_none()
        {
            return Err(Error::not_found(format!("before_layer_id {} does not exist", quote(before))));
        }

        let value = parse(json, "style layer")?;
        let mut error = mln_conversion::Error::default();
        let Some(layer) = convert_layer(&Convertible::from(&value), &mut error) else {
            return Err(conversion_error("style layer", error));
        };

        let id = layer.get_id();
        if self.get_layer(&id).is_some() {
            return Err(Error::already_exists(format!("layer {} already exists", quote(&id))));
        }
        let type_info = layer.get_type_info();
        if type_info.source == layer_type_info::Source::Required {
            let source_id = layer.get_source_id();
            let Some(source) = self.get_source(&source_id) else {
                return Err(Error::not_found(format!(
                    "layer {} uses source {}, which does not exist",
                    quote(&id),
                    quote(&source_id)
                )));
            };
            if !source.supports_layer_type(type_info) {
                return Err(Error::invalid_argument(format!(
                    "source {} does not support {} layer {}",
                    quote(&source_id),
                    type_info.type_,
                    quote(&id)
                )));
            }
        }
        self.add_layer(layer, before_layer_id)
    }

    /// Moves the layer named `id` below the layer named `before_layer_id`,
    /// or above every layer when it is `None`. A move below itself changes
    /// nothing.
    ///
    /// Fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind) while the
    /// style isn't loaded, and with `NotFound` if the style has no layer
    /// named `id` or `before_layer_id`.
    pub fn move_layer(&mut self, id: &str, before_layer_id: Option<&str>) -> Result<(), Error> {
        self.require_loaded()?;
        if self.get_layer(id).is_none() {
            return Err(layer_not_found(id));
        }
        if let Some(before) = before_layer_id {
            if before == id {
                return Ok(());
            }
            if self.get_layer(before).is_none() {
                return Err(Error::not_found(format!("before_layer_id {} does not exist", quote(before))));
            }
        }
        self.impl_.mutated = true;
        self.impl_.move_layer(id, before_layer_id);
        Ok(())
    }

    /// The layer named `id` as a
    /// [layer](https://maplibre.org/maplibre-style-spec/layers/) object, or
    /// `None` if the style has no such layer. Adding the JSON back gives the
    /// same layer.
    pub fn layer_json(&self, id: &str) -> Option<String> {
        self.get_layer(id).map(|layer| json_text(&layer.serialize()))
    }

    /// Sets the property `name` of the layer named `layer_id` to the JSON
    /// value `json`.
    ///
    /// `name` is one of the layer's layout or paint
    /// [properties](https://maplibre.org/maplibre-style-spec/layers/), or
    /// `filter`, `minzoom`, `maxzoom`, `source` or `source-layer`. A `null`
    /// resets any of these but `source` and `source-layer` to its default.
    ///
    /// Fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind) while the
    /// style isn't loaded, with `NotFound` if the style has no layer named
    /// `layer_id`, and with `InvalidArgument` if `json` isn't valid JSON,
    /// the layer has no property `name` or the value doesn't fit it.
    pub fn set_layer_property_json(&mut self, layer_id: &str, name: &str, json: &str) -> Result<(), Error> {
        self.require_loaded()?;
        let context = format!("property {} of layer {}", quote(name), quote(layer_id));
        let value = parse(json, &context)?;
        let Some(mut layer) = self.get_layer_mut(layer_id) else {
            return Err(layer_not_found(layer_id));
        };

        let type_info = layer.get_type_info();
        if (name == "source" || name == "source-layer") && type_info.source != layer_type_info::Source::Required {
            // A layer that takes no source would ignore the value.
            return Err(Error::invalid_argument(format!(
                "{} layer {} does not take a {name}",
                type_info.type_,
                quote(layer_id)
            )));
        }
        match name {
            "filter" if value.is_null() => layer.set_filter(&Filter::default()),
            "minzoom" if value.is_null() => layer.set_min_zoom(f32::NEG_INFINITY),
            "maxzoom" if value.is_null() => layer.set_max_zoom(f32::INFINITY),
            _ => {
                if let Some(error) = layer.set_property(name, &Convertible::from(&value), None) {
                    return Err(conversion_error(&context, error));
                }
            }
        }
        Ok(())
    }

    /// The JSON value of the property `name` of the layer named `layer_id`,
    /// or `None` if the layer doesn't set it. `name` is one of the names
    /// [`set_layer_property_json`](Self::set_layer_property_json) takes. The
    /// value can differ in form from the value that was set: a color set as
    /// `"#ff0000"` reads back as `["rgba",255.0,0.0,0.0,1.0]`.
    ///
    /// Fails with [`ErrorKind::NotFound`](mln_util::ErrorKind) if the style
    /// has no layer named `layer_id`.
    pub fn layer_property_json(&self, layer_id: &str, name: &str) -> Result<Option<String>, Error> {
        let Some(layer) = self.get_layer(layer_id) else {
            return Err(layer_not_found(layer_id));
        };
        let value = match name {
            "filter" => {
                let filter = layer.get_filter();
                filter.is_set().then(|| filter.serialize())
            }
            "source" | "source-layer" => {
                let source = if name == "source" { layer.get_source_id() } else { layer.get_source_layer() };
                (!source.is_empty()).then_some(Value::String(source))
            }
            "minzoom" => {
                let zoom = layer.get_min_zoom();
                (zoom != f32::NEG_INFINITY).then_some(Value::Double(zoom.into()))
            }
            "maxzoom" => {
                let zoom = layer.get_max_zoom();
                (zoom != f32::INFINITY).then_some(Value::Double(zoom.into()))
            }
            _ => {
                let property = layer.get_property(name);
                (property.get_kind() != Kind::Undefined).then(|| property.get_value().clone())
            }
        };
        Ok(value.as_ref().map(json_text))
    }

    /// Replaces the style's [light](https://maplibre.org/maplibre-style-spec/light/)
    /// with the one `json` describes.
    ///
    /// Fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind) while the
    /// style isn't loaded, and with `InvalidArgument` if `json` isn't a valid
    /// light.
    pub fn set_light_json(&mut self, json: &str) -> Result<(), Error> {
        self.require_loaded()?;
        let value = parse(json, "style light")?;
        let mut error = mln_conversion::Error::default();
        let Some(light) = Light::convert(&Convertible::from(&value), &mut error) else {
            return Err(conversion_error("style light", error));
        };
        self.set_light(light);
        Ok(())
    }

    /// Sets the property `name` of the style's light to the JSON value
    /// `json`. A `null` resets it to its default.
    ///
    /// Fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind) while the
    /// style isn't loaded, and with `InvalidArgument` if `json` isn't valid
    /// JSON, the light has no property `name` or the value doesn't fit it.
    pub fn set_light_property_json(&mut self, name: &str, json: &str) -> Result<(), Error> {
        self.require_loaded()?;
        let context = format!("style light property {}", quote(name));
        let value = parse(json, &context)?;
        match self.get_light_mut().set_property(name, &Convertible::from(&value)) {
            None => Ok(()),
            Some(error) => Err(conversion_error(&context, error)),
        }
    }

    /// The JSON value of the property `name` of the style's light, or `None`
    /// if the light doesn't set it. The value can differ in form from the
    /// value that was set, as with
    /// [`layer_property_json`](Self::layer_property_json).
    pub fn light_property_json(&self, name: &str) -> Option<String> {
        let property = self.get_light().get_property(name);
        (property.get_kind() != Kind::Undefined).then(|| json_text(property.get_value()))
    }

    /// Sets the value `name` that the style's `global-state` expressions
    /// read to the JSON value `json`. A `null` resets it to the default that
    /// the style's `state` gives it.
    ///
    /// Fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind) while the
    /// style isn't loaded, and with `InvalidArgument` if `json` isn't valid
    /// JSON.
    pub fn set_global_state_property_json(&mut self, name: &str, json: &str) -> Result<(), Error> {
        self.require_loaded()?;
        let value = parse(json, &format!("global state property {}", quote(name)))?;
        self.set_global_state_property(name, &mln_conversion::json_to_value(&value))
    }

    /// The values that the style's `global-state` expressions read, as a
    /// JSON object.
    pub fn global_state_json(&self) -> String {
        json_text(&Value::Object(self.get_global_state()))
    }

    /// Fails with [`ErrorKind::InvalidState`](mln_util::ErrorKind) while the
    /// style isn't loaded: the parse that ends a load replaces the whole
    /// style, so an edit made before it would be lost.
    fn require_loaded(&self) -> Result<(), Error> {
        if self.is_loaded() { Ok(()) } else { Err(Error::invalid_state("style is not loaded")) }
    }
}

fn quote(id: &str) -> String {
    format!("\"{id}\"")
}

fn parse(json: &str, context: &str) -> Result<serde_json::Value, Error> {
    serde_json::from_str(json).map_err(|error| Error::invalid_argument(format!("{context} is not valid JSON: {error}")))
}

fn conversion_error(context: &str, error: mln_conversion::Error) -> Error {
    Error::invalid_argument(format!("{context}: {}", error.message))
}

fn layer_not_found(id: &str) -> Error {
    Error::not_found(format!("layer {} does not exist", quote(id)))
}

fn json_text(value: &Value) -> String {
    value_to_json(value).to_string()
}
