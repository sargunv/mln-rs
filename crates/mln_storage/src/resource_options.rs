// Ports include/mln/storage/resource_options.hpp and
// src/mln/storage/resource_options.cpp.

use mln_util::constants::DEFAULT_MAX_CACHE_SIZE;
use mln_util::tile_server_options::TileServerOptions;

/// `ResourceOptions`. `Clone` is `clone()`.
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceOptions {
    api_key: String,
    tile_server_options: TileServerOptions,
    cache_path: String,
    asset_path: String,
    maximum_size: u64,
}

impl Default for ResourceOptions {
    /// `ResourceOptions()`: default values.
    fn default() -> Self {
        ResourceOptions {
            api_key: String::new(),
            tile_server_options: TileServerOptions::default(),
            cache_path: ":memory:".to_owned(),
            asset_path: ".".to_owned(),
            maximum_size: DEFAULT_MAX_CACHE_SIZE,
        }
    }
}

impl ResourceOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the Mapbox access token.
    pub fn with_api_key(mut self, token: impl Into<String>) -> Self {
        self.api_key = token.into();
        self
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn with_tile_server_options(mut self, tile_server_options: TileServerOptions) -> Self {
        self.tile_server_options = tile_server_options;
        self
    }

    pub fn tile_server_options(&self) -> TileServerOptions {
        self.tile_server_options.clone()
    }

    pub fn with_cache_path(mut self, path: impl Into<String>) -> Self {
        self.cache_path = path.into();
        self
    }

    pub fn cache_path(&self) -> &str {
        &self.cache_path
    }

    /// Sets the asset path, which is the root directory from where the
    /// asset:// scheme gets resolved in a style.
    pub fn with_asset_path(mut self, path: impl Into<String>) -> Self {
        self.asset_path = path.into();
        self
    }

    pub fn asset_path(&self) -> &str {
        &self.asset_path
    }

    pub fn with_maximum_cache_size(mut self, size: u64) -> Self {
        self.maximum_size = size;
        self
    }

    pub fn maximum_cache_size(&self) -> u64 {
        self.maximum_size
    }

    /// `ResourceOptions::Default()`.
    pub fn default_options() -> Self {
        ResourceOptions::new().with_tile_server_options(TileServerOptions::default_configuration())
    }
}
