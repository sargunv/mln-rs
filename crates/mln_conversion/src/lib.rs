//! Ports the `mln::style::conversion` core: `Error` and `Convertible`
//! (include/mln/style/conversion.hpp, include/mln/style/conversion_impl.hpp),
//! the JSON front end (src/mln/style/rapidjson_conversion.hpp, on serde_json
//! instead of rapidjson) and `getJSONType`
//! (src/mln/style/conversion/get_json_type.cpp), plus a port-only front end
//! over `mln::Value` (value_conversion.rs).
//!
//! The typed converters (`Converter<T>`, style/conversion/*.cpp) live in
//! `mln_style`.

pub mod conversion;
pub mod get_json_type;
pub mod json_conversion;
pub mod value_conversion;

pub use conversion::{
    ConversionTraits, Convertible, Error, array_length, array_member, each_member, is_array, is_object, is_undefined,
    object_member, to_bool, to_double, to_geojson, to_number, to_string, to_value,
};
pub use get_json_type::get_json_type;
pub use value_conversion::value_to_json;

#[cfg(test)]
mod tests;
