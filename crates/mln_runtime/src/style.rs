//! The map handle's style commands: sources and their data, custom sources,
//! images, layers, the light, the global state and transitions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use geojson::GeoJSON;
use mln_actor::scheduler;
use mln_style::image::Impl as StyleImage;
use mln_style::source::{Source, same_source, set_volatile};
use mln_style::sources::custom_geometry_source::{self, TileOptions};
use mln_style::sources::geojson_source::GeoJSONData;
use mln_style::sources::image_source::ImageSource;
use mln_style::sources::{
    CustomGeometrySource, CustomVectorSource, GeoJSONOptions, GeoJSONSource, RasterDEMSource, RasterSource,
    TileDataFormat, TileSource, UrlOrTileset, VectorSource, custom_vector_source,
};
use mln_style::{Style, TransitionOptions};
use mln_tile_id::CanonicalTileID;
use mln_util::error::Error;
use mln_util::geo::{LatLng, LatLngBounds};
use mln_util::image::PremultipliedImage;
use mln_util::immutable::Immutable;
use mln_util::range::Range;
use mln_util::tileset::{RasterEncoding, Scheme, VectorEncoding};

use crate::completion::{Completion, Resolver};
use crate::map::{Applied, Map, MapEntry, MapId, apply_command, flush, missing_map, style_not_loaded};
use crate::runtime::{Outbox, RuntimeCore, post};

/// The kind of a style source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Vector,
    Raster,
    RasterDem,
    GeoJson,
    Image,
    CustomGeometry,
    CustomVector,
}

/// The tiles that a source lists in the style itself, rather than at a URL.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceTiles {
    pub urls: Vec<String>,
    pub zoom_range: Range<u8>,
    pub scheme: Scheme,
    pub bounds: Option<LatLngBounds>,
    pub vector_encoding: Option<VectorEncoding>,
    pub raster_encoding: Option<RasterEncoding>,
}

/// The options of a GeoJSON source, as a source reports them.
#[derive(Clone, Debug)]
pub struct GeoJsonSourceInfo {
    pub options: Immutable<GeoJSONOptions>,
    /// The `clusterProperties` object as JSON, or empty for none.
    pub cluster_properties: String,
}

/// A source of the map's style.
#[derive(Clone, Debug)]
pub struct SourceInfo {
    pub id: String,
    pub kind: SourceKind,
    pub is_volatile: bool,
    /// The source's attribution. A tile source with a URL has none until its
    /// TileJSON loads.
    pub attribution: Option<String>,
    pub url: Option<String>,
    /// The tiles a tile source lists in the style itself.
    pub tiles: Option<SourceTiles>,
    /// A tile source's tile size.
    pub tile_size: Option<u16>,
    /// A vector source's tile format.
    pub vector_encoding: Option<VectorEncoding>,
    pub geojson: Option<GeoJsonSourceInfo>,
}

/// A layer of the map's style.
#[derive(Clone, Debug, PartialEq)]
pub struct LayerInfo {
    pub id: String,
    /// The layer's [type](https://maplibre.org/maplibre-style-spec/layers/#type),
    /// such as `fill`.
    pub layer_type: &'static str,
    /// The source's ID, or empty for a layer that takes none.
    pub source_id: String,
    pub source_layer: String,
    /// Negative infinity when the layer doesn't set one.
    pub min_zoom: f64,
    /// Infinity when the layer doesn't set one.
    pub max_zoom: f64,
    pub visible: bool,
}

/// The callbacks of a custom source, which run on a library thread. A map
/// asks for a tile with `fetch_tile`, and answers come back through the
/// source's commands, such as
/// [`Map::set_custom_geometry_source_tile_data`]. `cancel_tile` reports that
/// the map no longer needs a tile.
///
/// The callbacks drop once the source has left the style and no call is
/// running, so the source's last call happens before then.
pub struct CustomSourceCallbacks {
    pub fetch_tile: Box<dyn Fn(CanonicalTileID) + Send + Sync>,
    pub cancel_tile: Option<Box<dyn Fn(CanonicalTileID) + Send + Sync>>,
}

impl std::fmt::Debug for CustomSourceCallbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomSourceCallbacks").field("cancel_tile", &self.cancel_tile.is_some()).finish()
    }
}

/// The core's tile functions over `callbacks`, which the map holds weakly,
/// so that they drop when the map lets go of the source rather than when
/// the core's background loader does.
fn tile_functions(
    callbacks: &Arc<CustomSourceCallbacks>,
) -> (Option<mln_style::custom_tile_loader::TileFunction>, Option<mln_style::custom_tile_loader::TileFunction>) {
    let fetch: Weak<CustomSourceCallbacks> = Arc::downgrade(callbacks);
    let fetch = Arc::new(move |tile: &CanonicalTileID| {
        if let Some(callbacks) = fetch.upgrade() {
            (callbacks.fetch_tile)(*tile);
        }
    });
    let cancel = callbacks.cancel_tile.as_ref().map(|_| {
        let cancel = Arc::downgrade(callbacks);
        let cancel: mln_style::custom_tile_loader::TileFunction = Arc::new(move |tile: &CanonicalTileID| {
            if let Some(callback) = cancel.upgrade().as_ref().and_then(|callbacks| callbacks.cancel_tile.as_ref()) {
                callback(*tile);
            }
        });
        cancel
    });
    (Some(fetch), cancel)
}

/// A custom source that a command added, and the callbacks that the map
/// holds for it.
struct CallbackSource {
    id: String,
    source: Box<dyn Source>,
    /// Held only to be dropped: the core's tile functions hold the
    /// callbacks weakly.
    _callbacks: Arc<CustomSourceCallbacks>,
}

/// The custom sources of a map, whose callbacks it lets go of once their
/// source leaves the style.
#[derive(Default)]
pub(crate) struct CallbackSources(Vec<CallbackSource>);

/// GeoJSON data for one source: the data being built, and the newest data
/// that waits to follow it.
struct GeoJsonUpdate {
    source: GeoJSONSource,
    /// The build in progress, by its number, and its completion, which
    /// resolves when the data is installed or when something replaced the
    /// source's data first.
    building: Option<(u64, Resolver<Applied>)>,
    waiting: Option<(GeoJSON, Resolver<Applied>)>,
}

/// The GeoJSON sources whose new data is still being built, by source ID.
#[derive(Default)]
pub(crate) struct GeoJsonUpdates(HashMap<String, GeoJsonUpdate>);

/// Lets go of the callbacks of the sources that left the style, and cancels
/// the GeoJSON data that waits for sources that left. The callbacks drop
/// from the outbox, so before the completions that follow, and with the
/// runtime's state unborrowed.
pub(crate) fn reconcile(entry: &mut MapEntry, outbox: &mut Outbox) {
    let style = entry.core.get_style();
    let present = |id: &str, source: &dyn Source| style.get_source(id).is_some_and(|found| same_source(found, source));
    let (kept, gone): (Vec<_>, Vec<_>) =
        std::mem::take(&mut entry.callback_sources.0).into_iter().partition(|held| present(&held.id, &*held.source));
    entry.callback_sources.0 = kept;
    if !gone.is_empty() {
        outbox.push(move || drop(gone));
    }
    entry.geojson_updates.0.retain(|id, update| {
        if present(id, &update.source) {
            return true;
        }
        let cancelled = || Err(Error::cancelled("the source left the style before its data was ready"));
        if let Some((_, resolver)) = update.building.take() {
            outbox.resolve(resolver, cancelled());
        }
        if let Some((_, resolver)) = update.waiting.take() {
            outbox.resolve(resolver, cancelled());
        }
        false
    });
}

/// Lets go of everything a released map's sources hold, before its release
/// completes.
pub(crate) fn release(entry: &mut MapEntry, outbox: &mut Outbox) {
    let held = std::mem::take(&mut entry.callback_sources.0);
    if !held.is_empty() {
        outbox.push(move || drop(held));
    }
    for (_, mut update) in entry.geojson_updates.0.drain() {
        let cancelled = || Err(Error::cancelled("the map was released before the data was ready"));
        if let Some((_, resolver)) = update.building.take() {
            outbox.resolve(resolver, cancelled());
        }
        if let Some((_, resolver)) = update.waiting.take() {
            outbox.resolve(resolver, cancelled());
        }
    }
}

fn quote(id: &str) -> String {
    format!("\"{id}\"")
}

fn require_loaded(style: &Style) -> Result<(), Error> {
    if style.is_loaded() { Ok(()) } else { Err(style_not_loaded()) }
}

fn source_not_found(id: &str) -> Error {
    Error::not_found(format!("source {} does not exist", quote(id)))
}

fn find_source<'a>(style: &'a Style, id: &str) -> Result<&'a dyn Source, Error> {
    style.get_source(id).ok_or_else(|| source_not_found(id))
}

/// The source `id` as a `T`, which `kind` names in the error for a source of
/// another kind, as in `source "water" is not a GeoJSON source`.
fn find_source_as<'a, T: 'static>(style: &'a Style, id: &str, kind: &str) -> Result<&'a T, Error> {
    find_source(style, id)?
        .as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| Error::invalid_argument(format!("source {} is not {kind}", quote(id))))
}

fn require_new_source(style: &Style, id: &str) -> Result<(), Error> {
    require_loaded(style)?;
    if style.get_source(id).is_some() {
        return Err(Error::already_exists(format!("source {} already exists", quote(id))));
    }
    Ok(())
}

fn tile_source(source: &dyn Source) -> Option<&TileSource> {
    let any = source.as_any();
    if let Some(vector) = any.downcast_ref::<VectorSource>() {
        Some(vector.tile_source())
    } else if let Some(raster) = any.downcast_ref::<RasterSource>() {
        Some(raster.tile_source())
    } else {
        any.downcast_ref::<RasterDEMSource>().map(RasterDEMSource::tile_source)
    }
}

fn source_kind(source: &dyn Source) -> Option<SourceKind> {
    let any = source.as_any();
    Some(if any.is::<VectorSource>() {
        SourceKind::Vector
    } else if any.is::<RasterSource>() {
        SourceKind::Raster
    } else if any.is::<RasterDEMSource>() {
        SourceKind::RasterDem
    } else if any.is::<GeoJSONSource>() {
        SourceKind::GeoJson
    } else if any.is::<ImageSource>() {
        SourceKind::Image
    } else if any.is::<CustomGeometrySource>() {
        SourceKind::CustomGeometry
    } else if any.is::<CustomVectorSource>() {
        SourceKind::CustomVector
    } else {
        return None;
    })
}

/// The `clusterProperties` of `options` as a JSON object, or empty for none.
fn cluster_properties_json(options: &GeoJSONOptions) -> String {
    if options.cluster_properties.is_empty() {
        return String::new();
    }
    let members = options
        .cluster_properties
        .iter()
        .map(|(key, (map, reduce))| {
            let pair = feature::Value::Array(vec![reduce.serialize(), map.serialize()]);
            (key.clone(), mln_conversion::value_to_json(&pair))
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::Value::Object(members).to_string()
}

fn source_info(source: &dyn Source) -> Option<SourceInfo> {
    let kind = source_kind(source)?;
    let any = source.as_any();
    let url = if let Some(tiles) = tile_source(source) {
        tiles.get_url()
    } else if let Some(geojson) = any.downcast_ref::<GeoJSONSource>() {
        geojson.get_url()
    } else {
        any.downcast_ref::<ImageSource>().and_then(ImageSource::get_url)
    };
    let geojson = any.downcast_ref::<GeoJSONSource>().map(|geojson| {
        let options = geojson.get_options();
        GeoJsonSourceInfo { cluster_properties: cluster_properties_json(&options), options }
    });
    let mut info = SourceInfo {
        id: source.get_id(),
        kind,
        is_volatile: source.is_volatile(),
        attribution: source.get_attribution().filter(|attribution| !attribution.is_empty()),
        url,
        tiles: None,
        tile_size: None,
        vector_encoding: None,
        geojson,
    };
    if let Some(tile_source) = tile_source(source) {
        info.tile_size = Some(tile_source.get_tile_size());
        info.vector_encoding = any.downcast_ref::<VectorSource>().map(VectorSource::get_encoding);
        if let UrlOrTileset::Tileset(tileset) = tile_source.get_url_or_tileset() {
            info.vector_encoding = tileset.vector_encoding.or(info.vector_encoding);
            info.tiles = Some(SourceTiles {
                urls: tileset.tiles.clone(),
                zoom_range: tileset.zoom_range,
                scheme: tileset.scheme,
                bounds: tileset.bounds,
                vector_encoding: tileset.vector_encoding,
                raster_encoding: tileset.raster_encoding,
            });
        }
    }
    Some(info)
}

fn layer_info(layer: &dyn mln_style::Layer) -> LayerInfo {
    LayerInfo {
        id: layer.get_id(),
        layer_type: layer.get_type_info().type_,
        source_id: layer.get_source_id(),
        source_layer: layer.get_source_layer(),
        min_zoom: layer.get_min_zoom().into(),
        max_zoom: layer.get_max_zoom().into(),
        visible: layer.get_visibility() != mln_style_types::VisibilityType::None,
    }
}

/// Parses GeoJSON text, on the runtime's thread.
fn parse_geojson(json: &str) -> Result<GeoJSON, Error> {
    let document: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| Error::invalid_argument(format!("GeoJSON data is not valid JSON: {error}")))?;
    let mut error = mln_conversion::Error::default();
    mln_conversion::to_geojson(&mln_conversion::Convertible::from(&document), &mut error)
        .ok_or_else(|| Error::invalid_argument(format!("GeoJSON data is invalid: {}", error.message)))
}

fn geometry_name(geometry: &geometry::Geometry<f64>) -> &'static str {
    match geometry {
        geometry::Geometry::Empty => "empty",
        geometry::Geometry::Point(_) => "point",
        geometry::Geometry::MultiPoint(_) => "multi-point",
        geometry::Geometry::LineString(_) => "line string",
        geometry::Geometry::MultiLineString(_) => "multi-line string",
        geometry::Geometry::Polygon(_) => "polygon",
        geometry::Geometry::MultiPolygon(_) => "multi-polygon",
        geometry::Geometry::GeometryCollection(_) => "geometry collection",
    }
}

/// Clustering reads a feature collection of points, so a clustered source
/// refuses other data up front, naming what it found. An empty collection
/// holds nothing to cluster.
fn check_clusterable(geojson: &GeoJSON) -> Result<(), Error> {
    let features = match geojson {
        GeoJSON::FeatureCollection(features) => features,
        other => {
            let found = if matches!(other, GeoJSON::Feature(_)) { "a single feature" } else { "a bare geometry" };
            return Err(Error::invalid_argument(format!(
                "clustered GeoJSON data requires a feature collection; the data is {found}"
            )));
        }
    };
    match features.iter().position(|feature| !matches!(feature.geometry, geometry::Geometry::Point(_))) {
        None => Ok(()),
        Some(index) => Err(Error::invalid_argument(format!(
            "clustered GeoJSON data requires point geometry on every feature; feature {index} has {} geometry",
            geometry_name(&features[index].geometry)
        ))),
    }
}

/// The features a custom geometry tile draws: its core draws only a feature
/// collection, so a lone feature or geometry becomes one.
fn feature_collection(geojson: GeoJSON) -> GeoJSON {
    match geojson {
        GeoJSON::FeatureCollection(_) => geojson,
        GeoJSON::Feature(feature) => GeoJSON::FeatureCollection(vec![feature]),
        GeoJSON::Geometry(geometry) => GeoJSON::FeatureCollection(vec![feature::Feature::new(geometry)]),
    }
}

impl Map {
    // MARK: - Global state

    /// Sets the value `name` that the style's `global-state` expressions
    /// read, from JSON. See `Style::set_global_state_property_json`.
    pub fn set_global_state_property(
        &self,
        name: impl Into<String>,
        json: impl Into<String>,
    ) -> Result<Completion<Applied>, Error> {
        let (name, json) = (name.into(), json.into());
        self.command(move |map| map.get_style_mut().set_global_state_property_json(&name, &json))
    }

    /// The style's global state as a JSON object.
    pub fn global_state(&self) -> Result<Completion<String>, Error> {
        self.query(|map| Ok(map.get_style().global_state_json()))
    }

    // MARK: - Sources

    /// A source of the style, or `None` when the style has no such source.
    pub fn style_source(&self, source_id: impl Into<String>) -> Result<Completion<Option<SourceInfo>>, Error> {
        let source_id = source_id.into();
        self.query(move |map| Ok(map.get_style().get_source(&source_id).and_then(source_info)))
    }

    /// The style's sources, in style order.
    pub fn style_sources(&self) -> Result<Completion<Vec<SourceInfo>>, Error> {
        self.query(|map| Ok(map.get_style().get_sources().into_iter().filter_map(source_info).collect()))
    }

    /// Sets whether a source keeps its tiles out of the persistent cache.
    pub fn set_style_source_volatile(
        &self,
        source_id: impl Into<String>,
        is_volatile: bool,
    ) -> Result<Completion<Applied>, Error> {
        let source_id = source_id.into();
        self.command(move |map| {
            let style = map.get_style_mut();
            require_loaded(style)?;
            set_volatile(find_source(style, &source_id)?, is_volatile);
            Ok(())
        })
    }

    // MARK: - GeoJSON sources

    /// Adds a GeoJSON source with no data and `options`, which stay fixed
    /// for as long as the style holds it.
    pub fn add_geojson_source(
        &self,
        source_id: impl Into<String>,
        options: GeoJSONOptions,
    ) -> Result<Completion<Applied>, Error> {
        let source_id = source_id.into();
        self.command(move |map| {
            let style = map.get_style_mut();
            require_new_source(style, &source_id)?;
            style.add_source(Box::new(GeoJSONSource::new(source_id, Arc::new(options))))
        })
    }

    /// Makes a GeoJSON source load its data from `url`. Data that a
    /// [`set_geojson_source_data`](Self::set_geojson_source_data) sent before
    /// and that isn't ready yet is cancelled.
    pub fn set_geojson_source_url(
        &self,
        source_id: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<Completion<Applied>, Error> {
        let (source_id, url) = (source_id.into(), url.into());
        self.send(
            move |entry, outbox| {
                let style = entry.core.get_style();
                require_loaded(style)?;
                find_source_as::<GeoJSONSource>(style, &source_id, "a GeoJSON source")?.set_url(&url);
                if let Some((_, resolver)) =
                    entry.geojson_updates.0.get_mut(&source_id).and_then(|update| update.waiting.take())
                {
                    outbox.resolve(resolver, Err(Error::cancelled("the source's URL replaced its data")));
                }
                Ok(())
            },
            |(), entry| entry.applied(),
        )
    }

    /// Replaces the data of a GeoJSON source with the GeoJSON text `json`, a
    /// feature collection, feature or geometry.
    ///
    /// The runtime's thread parses the data, and a background thread
    /// prepares it with the source's options; the command completes once
    /// the source holds it. Later commands don't wait for it. While the
    /// source is still preparing earlier data, only the newest data waits
    /// to follow it. With synchronous tiling on, the command prepares the
    /// data before later commands run.
    ///
    /// Completes with [`ErrorKind::NotFound`](mln_util::ErrorKind) when the
    /// style has no such source, with `InvalidArgument` when it isn't a
    /// GeoJSON source, when `json` isn't GeoJSON, or when the source clusters
    /// and the data isn't a feature collection of points, and with
    /// `Cancelled` when newer data replaces it while it waits, or when the
    /// source's URL, its removal, a style load or the map's release replaces
    /// the source's data before it is ready.
    pub fn set_geojson_source_data(
        &self,
        source_id: impl Into<String>,
        json: impl Into<String>,
    ) -> Result<Completion<Applied>, Error> {
        let (source_id, json) = (source_id.into(), json.into());
        self.shared.send(move |core, outbox, id, sequence, resolver| {
            if !core.maps.contains_key(&id) {
                return outbox.resolve(resolver, Err(missing_map()));
            }
            let run_loop = core.run_loop.clone();
            apply_command(
                core,
                id,
                sequence,
                outbox,
                move |entry, outbox| match queue_geojson(entry, id, &source_id, &json, resolver, run_loop, outbox) {
                    Ok(installed) => installed,
                    Err((error, resolver)) => {
                        outbox.resolve(resolver, Err(error));
                        None
                    }
                },
                |_, installed, applied, outbox| {
                    if let Some(resolver) = installed {
                        outbox.resolve(resolver, Ok(applied));
                    }
                },
            );
        })
    }

    /// Sets whether a GeoJSON source builds its tiles while the map renders,
    /// in place of the `synchronousUpdate` of the source's JSON.
    pub fn set_geojson_source_synchronous_tiling(
        &self,
        source_id: impl Into<String>,
        enabled: bool,
    ) -> Result<Completion<Applied>, Error> {
        let source_id = source_id.into();
        self.command(move |map| {
            let style = map.get_style_mut();
            require_loaded(style)?;
            find_source_as::<GeoJSONSource>(style, &source_id, "a GeoJSON source")?
                .set_override_synchronous_update(enabled);
            Ok(())
        })
    }

    // MARK: - Custom sources

    /// Adds a source whose GeoJSON tiles come from `callbacks`. The map
    /// holds the callbacks until the source leaves the style, by its
    /// removal, a style load or the map's release, and drops them before
    /// the command that removed it completes. A command that fails drops
    /// them before it completes.
    pub fn add_custom_geometry_source(
        &self,
        source_id: impl Into<String>,
        callbacks: CustomSourceCallbacks,
        zoom_range: Range<u8>,
        tile_options: TileOptions,
    ) -> Result<Completion<Applied>, Error> {
        let (source_id, callbacks) = (source_id.into(), Arc::new(callbacks));
        self.send(
            move |entry, outbox| {
                add_callback_source(entry, outbox, source_id, callbacks, |id, callbacks| {
                    let (fetch_tile_function, cancel_tile_function) = tile_functions(callbacks);
                    let options = custom_geometry_source::Options {
                        fetch_tile_function,
                        cancel_tile_function,
                        zoom_range,
                        tile_options,
                    };
                    let source = CustomGeometrySource::new(id, options)
                        .map_err(|error| Error::invalid_argument(error.to_string()))?;
                    Ok(Box::new(source))
                })
            },
            |(), entry| entry.applied(),
        )
    }

    /// Adds a source whose vector tiles come from `callbacks`, which the map
    /// holds as [`add_custom_geometry_source`](Self::add_custom_geometry_source)
    /// says.
    pub fn add_custom_vector_source(
        &self,
        source_id: impl Into<String>,
        callbacks: CustomSourceCallbacks,
        zoom_range: Range<u8>,
    ) -> Result<Completion<Applied>, Error> {
        let (source_id, callbacks) = (source_id.into(), Arc::new(callbacks));
        self.send(
            move |entry, outbox| {
                add_callback_source(entry, outbox, source_id, callbacks, |id, callbacks| {
                    let (fetch_tile_function, cancel_tile_function) = tile_functions(callbacks);
                    let options =
                        custom_vector_source::Options { fetch_tile_function, cancel_tile_function, zoom_range };
                    Ok(Box::new(CustomVectorSource::new(id, options)))
                })
            },
            |(), entry| entry.applied(),
        )
    }

    /// Sets the data of a tile of a custom geometry source from GeoJSON text:
    /// a feature collection, feature or geometry. The source ignores data
    /// for a tile it hasn't asked for, or has since cancelled.
    pub fn set_custom_geometry_source_tile_data(
        &self,
        source_id: impl Into<String>,
        tile_id: CanonicalTileID,
        json: impl Into<String>,
    ) -> Result<Completion<Applied>, Error> {
        let (source_id, json) = (source_id.into(), json.into());
        self.command(move |map| {
            let geojson = feature_collection(parse_geojson(&json)?);
            custom_geometry(map.get_style(), &source_id)?.set_tile_data(tile_id, geojson);
            Ok(())
        })
    }

    /// Reports that a tile of a custom geometry source failed to load.
    pub fn set_custom_geometry_source_tile_error(
        &self,
        source_id: impl Into<String>,
        tile_id: CanonicalTileID,
        message: impl Into<String>,
    ) -> Result<Completion<Applied>, Error> {
        let (source_id, message) = (source_id.into(), message.into());
        self.command(move |map| {
            custom_geometry(map.get_style(), &source_id)?.set_tile_error(tile_id, Error::native(message));
            Ok(())
        })
    }

    /// Makes a custom geometry source ask for a tile again when the map needs
    /// it.
    pub fn invalidate_custom_geometry_source_tile(
        &self,
        source_id: impl Into<String>,
        tile_id: CanonicalTileID,
    ) -> Result<Completion<Applied>, Error> {
        let source_id = source_id.into();
        self.command(move |map| {
            custom_geometry(map.get_style(), &source_id)?.invalidate_tile(tile_id);
            Ok(())
        })
    }

    /// Makes a custom geometry source ask again for the tiles in `bounds`
    /// that the map still needs.
    pub fn invalidate_custom_geometry_source_region(
        &self,
        source_id: impl Into<String>,
        bounds: LatLngBounds,
    ) -> Result<Completion<Applied>, Error> {
        let source_id = source_id.into();
        self.command(move |map| {
            custom_geometry(map.get_style(), &source_id)?.invalidate_region(bounds);
            Ok(())
        })
    }

    /// Sets the data of a tile of a custom vector source: its bytes in
    /// `format`, or `None` for a tile without data. The source ignores data
    /// for a tile it hasn't asked for, or has since cancelled.
    pub fn set_custom_vector_source_tile_data(
        &self,
        source_id: impl Into<String>,
        tile_id: CanonicalTileID,
        data: Option<Vec<u8>>,
        format: TileDataFormat,
    ) -> Result<Completion<Applied>, Error> {
        let source_id = source_id.into();
        self.command(move |map| {
            custom_vector(map.get_style(), &source_id)?.set_tile_data(tile_id, data.map(Arc::new), format);
            Ok(())
        })
    }

    /// Reports that a tile of a custom vector source failed to load.
    pub fn set_custom_vector_source_tile_error(
        &self,
        source_id: impl Into<String>,
        tile_id: CanonicalTileID,
        message: impl Into<String>,
    ) -> Result<Completion<Applied>, Error> {
        let (source_id, message) = (source_id.into(), message.into());
        self.command(move |map| {
            custom_vector(map.get_style(), &source_id)?.set_tile_error(tile_id, Error::native(message));
            Ok(())
        })
    }

    /// Makes a custom vector source ask for a tile again when the map needs
    /// it.
    pub fn invalidate_custom_vector_source_tile(
        &self,
        source_id: impl Into<String>,
        tile_id: CanonicalTileID,
    ) -> Result<Completion<Applied>, Error> {
        let source_id = source_id.into();
        self.command(move |map| {
            custom_vector(map.get_style(), &source_id)?.invalidate_tile(tile_id);
            Ok(())
        })
    }

    /// Makes a custom vector source ask again for the tiles in `bounds` that
    /// the map still needs.
    pub fn invalidate_custom_vector_source_region(
        &self,
        source_id: impl Into<String>,
        bounds: LatLngBounds,
    ) -> Result<Completion<Applied>, Error> {
        let source_id = source_id.into();
        self.command(move |map| {
            custom_vector(map.get_style(), &source_id)?.invalidate_region(bounds);
            Ok(())
        })
    }

    // MARK: - Images

    /// Adds `image` to the style, or replaces the image with its ID.
    pub fn set_style_image(&self, image: StyleImage) -> Result<Completion<Applied>, Error> {
        self.command(move |map| {
            let style = map.get_style_mut();
            require_loaded(style)?;
            style.add_image(Arc::new(image));
            Ok(())
        })
    }

    /// Removes an image from the style. Completes with
    /// [`ErrorKind::NotFound`](mln_util::ErrorKind) when the style has no
    /// such image.
    pub fn remove_style_image(&self, image_id: impl Into<String>) -> Result<Completion<Applied>, Error> {
        let image_id = image_id.into();
        self.command(move |map| {
            let style = map.get_style_mut();
            require_loaded(style)?;
            if style.get_image(&image_id).is_none() {
                return Err(Error::not_found(format!("style image {} does not exist", quote(&image_id))));
            }
            style.remove_image(&image_id);
            Ok(())
        })
    }

    /// An image of the style, or `None` when the style has no such image.
    pub fn style_image(&self, image_id: impl Into<String>) -> Result<Completion<Option<Immutable<StyleImage>>>, Error> {
        let image_id = image_id.into();
        self.query(move |map| Ok(map.get_style().get_image(&image_id)))
    }

    // MARK: - Image sources

    /// Adds an image source that shows `image` at `coordinates`, its four
    /// corners clockwise from the top left.
    pub fn add_image_source(
        &self,
        source_id: impl Into<String>,
        coordinates: [LatLng; 4],
        image: PremultipliedImage,
    ) -> Result<Completion<Applied>, Error> {
        let source_id = source_id.into();
        self.command(move |map| {
            let style = map.get_style_mut();
            require_new_source(style, &source_id)?;
            let source = ImageSource::new(source_id, coordinates);
            source.set_image(image);
            style.add_source(Box::new(source))
        })
    }

    /// Makes an image source load its image from `url`.
    pub fn set_image_source_url(
        &self,
        source_id: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<Completion<Applied>, Error> {
        let (source_id, url) = (source_id.into(), url.into());
        self.command(move |map| {
            let style = map.get_style();
            require_loaded(style)?;
            find_source_as::<ImageSource>(style, &source_id, "an image source")?.set_url(&url);
            Ok(())
        })
    }

    /// Replaces the image of an image source.
    pub fn set_image_source_image(
        &self,
        source_id: impl Into<String>,
        image: PremultipliedImage,
    ) -> Result<Completion<Applied>, Error> {
        let source_id = source_id.into();
        self.command(move |map| {
            let style = map.get_style();
            require_loaded(style)?;
            find_source_as::<ImageSource>(style, &source_id, "an image source")?.set_image(image);
            Ok(())
        })
    }

    /// Moves the image of an image source to `coordinates`, its four corners
    /// clockwise from the top left.
    pub fn set_image_source_coordinates(
        &self,
        source_id: impl Into<String>,
        coordinates: [LatLng; 4],
    ) -> Result<Completion<Applied>, Error> {
        let source_id = source_id.into();
        self.command(move |map| {
            let style = map.get_style();
            require_loaded(style)?;
            find_source_as::<ImageSource>(style, &source_id, "an image source")?.set_coordinates(coordinates);
            Ok(())
        })
    }

    /// The corners of an image source's image, clockwise from the top left.
    pub fn image_source_coordinates(&self, source_id: impl Into<String>) -> Result<Completion<[LatLng; 4]>, Error> {
        let source_id = source_id.into();
        self.query(move |map| {
            Ok(find_source_as::<ImageSource>(map.get_style(), &source_id, "an image source")?.get_coordinates())
        })
    }

    // MARK: - Layers

    /// A layer of the style, or `None` when the style has no such layer.
    pub fn style_layer(&self, layer_id: impl Into<String>) -> Result<Completion<Option<LayerInfo>>, Error> {
        let layer_id = layer_id.into();
        self.query(move |map| Ok(map.get_style().get_layer(&layer_id).map(layer_info)))
    }

    /// The style's layers, from bottom to top.
    pub fn style_layers(&self) -> Result<Completion<Vec<LayerInfo>>, Error> {
        self.query(|map| Ok(map.get_style().get_layers().into_iter().map(layer_info).collect()))
    }

    /// Moves a layer below `before_layer_id`, or above every layer. See
    /// `Style::move_layer` for the errors.
    pub fn move_style_layer(
        &self,
        layer_id: impl Into<String>,
        before_layer_id: Option<String>,
    ) -> Result<Completion<Applied>, Error> {
        let layer_id = layer_id.into();
        self.command(move |map| map.get_style_mut().move_layer(&layer_id, before_layer_id.as_deref()))
    }

    // MARK: - Transitions

    /// Sets the default transition of the style's paint properties.
    pub fn set_style_transition_options(&self, options: TransitionOptions) -> Result<Completion<Applied>, Error> {
        self.command(move |map| {
            let style = map.get_style_mut();
            require_loaded(style)?;
            style.set_transition_options(&options);
            Ok(())
        })
    }

    /// The default transition of the style's paint properties.
    pub fn style_transition_options(&self) -> Result<Completion<TransitionOptions>, Error> {
        self.query(|map| Ok(map.get_style().get_transition_options()))
    }
}

/// Adds the custom source that `make` makes over `callbacks`, and holds the
/// callbacks for as long as the style holds the source. When the add fails,
/// the callbacks drop from the outbox, with the runtime's state unborrowed.
fn add_callback_source(
    entry: &mut MapEntry,
    outbox: &mut Outbox,
    id: String,
    callbacks: Arc<CustomSourceCallbacks>,
    make: impl FnOnce(String, &Arc<CustomSourceCallbacks>) -> Result<Box<dyn Source>, Error>,
) -> Result<(), Error> {
    let style = entry.core.get_style_mut();
    let added = require_new_source(style, &id).and_then(|()| {
        let source = make(id.clone(), &callbacks)?;
        // The style takes a handle of the source, and the map keeps another
        // to know it by.
        let held = clone_source(&*source)?;
        style.add_source(source)?;
        Ok(held)
    });
    match added {
        Ok(source) => {
            entry.callback_sources.0.push(CallbackSource { id, source, _callbacks: callbacks });
            Ok(())
        }
        Err(error) => {
            outbox.push(move || drop(callbacks));
            Err(error)
        }
    }
}

/// Another handle to a custom source.
fn clone_source(source: &dyn Source) -> Result<Box<dyn Source>, Error> {
    let any = source.as_any();
    if let Some(geometry) = any.downcast_ref::<CustomGeometrySource>() {
        Ok(Box::new(geometry.clone()))
    } else if let Some(vector) = any.downcast_ref::<CustomVectorSource>() {
        Ok(Box::new(vector.clone()))
    } else {
        Err(Error::invalid_argument("not a custom source"))
    }
}

fn custom_geometry<'a>(style: &'a Style, id: &str) -> Result<&'a CustomGeometrySource, Error> {
    find_source_as(style, id, "a custom geometry source")
}

fn custom_vector<'a>(style: &'a Style, id: &str) -> Result<&'a CustomVectorSource, Error> {
    find_source_as(style, id, "a custom vector source")
}

/// Takes new data for the GeoJSON source `source_id`: builds it now with
/// synchronous tiling on, and returns its resolver for the command to
/// resolve once it ran; otherwise starts building it, or leaves it waiting
/// behind the data being built. An error returns the resolver with it.
#[allow(clippy::too_many_arguments)]
fn queue_geojson(
    entry: &mut MapEntry,
    map_id: MapId,
    source_id: &str,
    json: &str,
    resolver: Resolver<Applied>,
    run_loop: mln_actor::RunLoopHandle,
    outbox: &mut Outbox,
) -> Result<Option<Resolver<Applied>>, (Error, Resolver<Applied>)> {
    let found = (|| {
        let style = entry.core.get_style();
        require_loaded(style)?;
        let source = find_source_as::<GeoJSONSource>(style, source_id, "a GeoJSON source")?.clone();
        let geojson = parse_geojson(json)?;
        if source.get_options().cluster {
            check_clusterable(&geojson)?;
        }
        Ok((source, geojson))
    })();
    let (source, geojson) = match found {
        Ok(found) => found,
        Err(error) => return Err((error, resolver)),
    };
    if source.is_update_synchronous() {
        return match source.data_builder().build(&geojson) {
            Ok(data) => {
                source.set_geojson_data(Some(data));
                Ok(Some(resolver))
            }
            Err(error) => Err((error, resolver)),
        };
    }
    let update = entry.geojson_updates.0.entry(source_id.to_owned()).or_insert_with(|| GeoJsonUpdate {
        source: source.clone(),
        building: None,
        waiting: None,
    });
    if update.building.is_none() {
        update.source = source;
        start_geojson(update, map_id, source_id.to_owned(), geojson, resolver, run_loop);
    } else if let Some((_, replaced)) = update.waiting.replace((geojson, resolver)) {
        outbox.resolve(replaced, Err(Error::cancelled("newer data for the source replaced it")));
    }
    Ok(None)
}

/// Builds `geojson` for `update`'s source on a background thread, then
/// installs it from the runtime's thread.
fn start_geojson(
    update: &mut GeoJsonUpdate,
    map_id: MapId,
    source_id: String,
    geojson: GeoJSON,
    resolver: Resolver<Applied>,
    run_loop: mln_actor::RunLoopHandle,
) {
    // Numbers each build, so a build that something replaced finds no
    // completion of its own.
    static BUILDS: AtomicU64 = AtomicU64::new(0);
    let build = BUILDS.fetch_add(1, Ordering::Relaxed);
    update.building = Some((build, resolver));
    let builder = update.source.data_builder();
    let generation = update.source.data_generation();
    let next = run_loop.clone();
    scheduler::get_background().schedule(Box::new(move || {
        let data = builder.build(&geojson);
        post(&run_loop, move |core, outbox| {
            install_geojson(core, map_id, &source_id, (build, generation), data, next, outbox);
        });
    }));
}

/// Installs data that a background thread built, unless something replaced
/// the source's data while it was built, then starts the data that waits.
fn install_geojson(
    core: &mut RuntimeCore,
    map_id: MapId,
    source_id: &str,
    (build, generation): (u64, u64),
    data: Result<Arc<dyn GeoJSONData>, Error>,
    run_loop: mln_actor::RunLoopHandle,
    outbox: &mut Outbox,
) {
    flush(core, map_id, outbox);
    let Some(entry) = core.maps.get_mut(&map_id) else { return };
    let Some(update) = entry.geojson_updates.0.get_mut(source_id) else { return };
    let Some((_, resolver)) = update.building.take_if(|(building, _)| *building == build) else { return };
    let current = entry.core.get_style().get_source(source_id).is_some_and(|found| same_source(found, &update.source));
    let installed = match data {
        Err(error) => Err(error),
        Ok(_) if !current || update.source.data_generation() != generation => {
            Err(Error::cancelled("the source's data was replaced before this data was ready"))
        }
        Ok(data) => {
            update.source.set_geojson_data(Some(data));
            Ok(())
        }
    };
    match update.waiting.take() {
        Some((geojson, next)) if current => {
            start_geojson(update, map_id, source_id.to_owned(), geojson, next, run_loop);
        }
        Some((_, next)) => outbox.resolve(next, Err(Error::cancelled("the source left the style"))),
        None => {
            entry.geojson_updates.0.remove(source_id);
        }
    }
    flush(core, map_id, outbox);
    let Some(entry) = core.maps.get(&map_id) else { return };
    outbox.resolve(resolver, installed.map(|()| entry.applied()));
}
