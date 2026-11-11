//! The file sources of one owner, shared per (type, options).
//!
//! The lock is released while a factory runs, because the `ResourceLoader`
//! factory asks the same manager for its own file sources. If two threads
//! create the same file source at once, the first one registered wins and
//! the other is dropped.

use std::sync::{Arc, Mutex, Weak};

use mln_util::sync::MutexExt;
use mln_vec_map::VecMap;

use crate::asset_file_source::AssetFileSource;
#[cfg(feature = "sqlite")]
use crate::database_file_source::DatabaseFileSource;
use crate::file_source::{FileSource, FileSourceType};
use crate::local_file_source::LocalFileSource;
use crate::main_resource_loader::MainResourceLoader;
#[cfg(feature = "sqlite")]
use crate::mbtiles_file_source::MBTilesFileSource;
use crate::online_file_source::OnlineFileSource;
use crate::pmtiles_file_source::PMTilesFileSource;
use crate::resource_options::ResourceOptions;

/// Creates the file source of one type. It gets the manager that asked, so
/// a file source built on others (the `ResourceLoader`) takes them from the
/// same owner.
pub type FileSourceFactory = Arc<dyn Fn(&FileSourceManager, &ResourceOptions) -> Arc<dyn FileSource> + Send + Sync>;

struct FileSourceInfo {
    type_: FileSourceType,
    options: ResourceOptions,
    file_source: Weak<dyn FileSource>,
}

#[derive(Default)]
struct Registry {
    file_sources: Vec<FileSourceInfo>,
    factories: VecMap<FileSourceType, FileSourceFactory>,
}

/// The file sources of one owner (a map's host, a runtime).
///
/// [`get_file_source`](Self::get_file_source) shares one live file source
/// per (type, [`ResourceOptions`]), and drops its record once every user has
/// released it. Separate managers never share a file source, so a resource
/// transform, a header transform or a factory one owner installs never
/// reaches another. `Clone` is another handle to the same file sources.
#[derive(Clone)]
pub struct FileSourceManager {
    registry: Arc<Mutex<Registry>>,
}

impl Default for FileSourceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSourceManager {
    /// A manager with the factory of every built-in file source type.
    pub fn new() -> Self {
        let manager = Self::empty();
        manager.register_file_source_factory(
            FileSourceType::ResourceLoader,
            Arc::new(|manager: &FileSourceManager, options: &ResourceOptions| {
                Arc::new(MainResourceLoader::new(manager, options)) as Arc<dyn FileSource>
            }),
        );
        manager.register_file_source_factory(
            FileSourceType::Asset,
            Arc::new(|_: &FileSourceManager, options: &ResourceOptions| {
                Arc::new(AssetFileSource::new(options)) as Arc<dyn FileSource>
            }),
        );
        #[cfg(feature = "sqlite")]
        manager.register_file_source_factory(
            FileSourceType::Database,
            Arc::new(|manager: &FileSourceManager, options: &ResourceOptions| {
                Arc::new(DatabaseFileSource::new(manager, options)) as Arc<dyn FileSource>
            }),
        );
        manager.register_file_source_factory(
            FileSourceType::FileSystem,
            Arc::new(|_: &FileSourceManager, options: &ResourceOptions| {
                Arc::new(LocalFileSource::new(options)) as Arc<dyn FileSource>
            }),
        );
        #[cfg(feature = "sqlite")]
        manager.register_file_source_factory(
            FileSourceType::Mbtiles,
            Arc::new(|_: &FileSourceManager, options: &ResourceOptions| {
                Arc::new(MBTilesFileSource::new(options)) as Arc<dyn FileSource>
            }),
        );
        manager.register_file_source_factory(
            FileSourceType::Pmtiles,
            Arc::new(|manager: &FileSourceManager, options: &ResourceOptions| {
                Arc::new(PMTilesFileSource::new(manager, options)) as Arc<dyn FileSource>
            }),
        );
        manager.register_file_source_factory(
            FileSourceType::Network,
            Arc::new(|_: &FileSourceManager, options: &ResourceOptions| {
                Arc::new(OnlineFileSource::new(options)) as Arc<dyn FileSource>
            }),
        );
        manager
    }

    /// A manager without factories.
    pub fn empty() -> Self {
        FileSourceManager { registry: Arc::new(Mutex::new(Registry::default())) }
    }

    /// The live file source of this type for these options, or a new one
    /// from the type's factory. `None` when the type has no factory.
    pub fn get_file_source(&self, type_: FileSourceType, options: &ResourceOptions) -> Option<Arc<dyn FileSource>> {
        let find = |registry: &Registry| {
            registry
                .file_sources
                .iter()
                .find(|info| info.type_ == type_ && info.options == *options)
                .and_then(|info| info.file_source.upgrade())
        };

        let factory = {
            let mut registry = self.registry.lock_unpoisoned();
            registry.file_sources.retain(|info| info.file_source.strong_count() > 0);
            if let Some(file_source) = find(&registry) {
                return Some(file_source);
            }
            registry.factories.get(&type_)?.clone()
        };

        let file_source = factory(self, options);

        let existing = {
            let mut registry = self.registry.lock_unpoisoned();
            let existing = find(&registry);
            if existing.is_none() {
                registry.file_sources.push(FileSourceInfo {
                    type_,
                    options: options.clone(),
                    file_source: Arc::downgrade(&file_source),
                });
            }
            existing
        };
        // A losing file source drops here, outside the lock: dropping one
        // joins its thread, which may be asking this manager for another.
        Some(existing.unwrap_or(file_source))
    }

    /// Sets the factory of a file source type, replacing any earlier one.
    /// File sources the earlier factory made stay shared until released.
    pub fn register_file_source_factory(&self, type_: FileSourceType, factory: FileSourceFactory) {
        self.registry.lock_unpoisoned().factories.insert(type_, factory);
    }

    /// Removes the factory of a file source type and returns it.
    pub fn unregister_file_source_factory(&self, type_: FileSourceType) -> Option<FileSourceFactory> {
        self.registry.lock_unpoisoned().factories.remove(&type_)
    }
}
