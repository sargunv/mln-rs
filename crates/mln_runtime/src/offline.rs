//! The offline regions in a runtime's cache database.

use std::path::Path;

use mln_actor::RunLoopHandle;
use mln_storage::ResourceError;
use mln_storage::offline::{
    OfflineRegion, OfflineRegionDefinition, OfflineRegionDownloadState, OfflineRegionMetadata, OfflineRegionObserver,
    OfflineRegionStatus,
};
use mln_storage::sqlite3::{Database, OpenFlag};
use mln_storage_offline::DatabaseFileSourceExt;
use mln_util::error::Error;

use crate::completion::Completion;
use crate::event::{EventMask, EventSource, RuntimeEvent, RuntimeEventKind};
use crate::runtime::{Runtime, post, reply_on};

/// The offline regions in a runtime's cache database, from
/// [`Runtime::offline`]. Each call is a command of the runtime, and the
/// regions download on the database's thread.
pub struct Offline<'a> {
    runtime: &'a Runtime,
}

impl<'a> Offline<'a> {
    pub(crate) fn new(runtime: &'a Runtime) -> Self {
        Offline { runtime }
    }

    /// Creates a region. It doesn't download until
    /// [`set_region_download_state`](Self::set_region_download_state) starts
    /// it.
    pub fn create_region(
        &self,
        definition: OfflineRegionDefinition,
        metadata: OfflineRegionMetadata,
    ) -> Result<Completion<OfflineRegion>, Error> {
        self.runtime.offline_command(move |database, core, resolver| {
            database.create_offline_region(&definition, &metadata, reply_on(core.run_loop.clone(), resolver));
        })
    }

    /// The region `region_id`, or `None` when the database has none.
    pub fn region(&self, region_id: i64) -> Result<Completion<Option<OfflineRegion>>, Error> {
        self.runtime.offline_command(move |database, core, resolver| {
            database.get_offline_region(region_id, reply_on(core.run_loop.clone(), resolver));
        })
    }

    /// Every region in the database.
    pub fn regions(&self) -> Result<Completion<Vec<OfflineRegion>>, Error> {
        self.runtime.offline_command(move |database, core, resolver| {
            database.list_offline_regions(reply_on(core.run_loop.clone(), resolver));
        })
    }

    /// Copies the regions of another cache database into this one, and gives
    /// this database's copy of each. A region that this database has already
    /// isn't copied again. The other database only needs to be readable.
    ///
    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) when
    /// `side_database_path` names no database file that opens for reading.
    /// That check opens the file on the calling thread.
    pub fn merge_regions(
        &self,
        side_database_path: impl Into<String>,
    ) -> Result<Completion<Vec<OfflineRegion>>, Error> {
        let side_database_path = side_database_path.into();
        check_side_database(&side_database_path)?;
        self.runtime.offline_command(move |database, core, resolver| {
            database.merge_offline_regions(&side_database_path, reply_on(core.run_loop.clone(), resolver));
        })
    }

    /// Replaces a region's metadata, and gives the updated region.
    pub fn update_region_metadata(
        &self,
        region_id: i64,
        metadata: OfflineRegionMetadata,
    ) -> Result<Completion<OfflineRegion>, Error> {
        self.runtime.offline_command(move |database, core, resolver| {
            database.update_offline_metadata(region_id, &metadata, reply_on(core.run_loop.clone(), resolver));
        })
    }

    /// A region's download progress.
    pub fn region_status(&self, region_id: i64) -> Result<Completion<OfflineRegionStatus>, Error> {
        self.runtime.offline_command(move |database, core, resolver| {
            database.get_offline_region_status(region_id, reply_on(core.run_loop.clone(), resolver));
        })
    }

    /// Selects the events that the runtime reports about a region's
    /// download, from [`EventMask::ALL_RUNTIME_EVENTS`]. It replaces the
    /// region's earlier selection, and [`EventMask::NONE`] stops them.
    ///
    /// Fails with [`ErrorKind::InvalidArgument`](mln_util::ErrorKind) for a
    /// mask with map events.
    pub fn set_region_observed(&self, region_id: i64, events: EventMask) -> Result<Completion<()>, Error> {
        if events.bits() & !EventMask::ALL_RUNTIME_EVENTS.bits() != 0 {
            return Err(Error::invalid_argument("an offline region reports only runtime events"));
        }
        self.runtime.offline_command(move |database, core, resolver| {
            let observer = (events != EventMask::NONE).then(|| -> Box<dyn OfflineRegionObserver + Send> {
                Box::new(RegionObserver { region_id, events, run_loop: core.run_loop.clone() })
            });
            database.set_offline_region_observer(region_id, observer, reply_on(core.run_loop.clone(), resolver));
        })
    }

    /// Starts or pauses a region's download.
    pub fn set_region_download_state(
        &self,
        region_id: i64,
        state: OfflineRegionDownloadState,
    ) -> Result<Completion<()>, Error> {
        self.runtime.offline_command(move |database, core, resolver| {
            database.set_offline_region_download_state(region_id, state, reply_on(core.run_loop.clone(), resolver));
        })
    }

    /// Makes maps check each resource of a region with its server before
    /// they use it. The region keeps its resources.
    pub fn invalidate_region(&self, region_id: i64) -> Result<Completion<()>, Error> {
        self.runtime.offline_command(move |database, core, resolver| {
            database.invalidate_offline_region(region_id, reply_on(core.run_loop.clone(), resolver));
        })
    }

    /// Deletes a region, with the resources that no other region uses.
    pub fn delete_region(&self, region_id: i64) -> Result<Completion<()>, Error> {
        self.runtime.offline_command(move |database, core, resolver| {
            database.delete_offline_region(region_id, reply_on(core.run_loop.clone(), resolver));
        })
    }
}

/// Checks that `path` names an existing database file that opens for
/// reading. SQLite would otherwise take an empty path, `:memory:` or a
/// `file:` URI for an in-memory database, and create a missing file.
fn check_side_database(path: &str) -> Result<(), Error> {
    let unreadable = || Error::invalid_argument("side database path must name an existing readable database file");
    if path.is_empty() || path.starts_with(':') || path.starts_with("file:") || !Path::new(path).is_file() {
        return Err(unreadable());
    }
    Database::try_open(path, OpenFlag::ReadOnly).map(drop).map_err(|_| unreadable())
}

/// Sends an observed region's download events to the runtime's handler, on
/// the runtime's thread.
struct RegionObserver {
    region_id: i64,
    events: EventMask,
    run_loop: RunLoopHandle,
}

impl RegionObserver {
    fn send(&self, kind: RuntimeEventKind) {
        if !self.events.contains(kind.event_type()) {
            return;
        }
        let event = RuntimeEvent { source: EventSource::Runtime, generation: 0, kind };
        post(&self.run_loop, move |core, outbox| core.send_events(vec![event], outbox));
    }
}

impl OfflineRegionObserver for RegionObserver {
    fn status_changed(&self, status: OfflineRegionStatus) {
        self.send(RuntimeEventKind::OfflineRegionStatusChanged { region_id: self.region_id, status });
    }

    fn response_error(&self, error: ResourceError) {
        self.send(RuntimeEventKind::OfflineRegionResponseError { region_id: self.region_id, error });
    }

    fn mapbox_tile_count_limit_exceeded(&self, limit: u64) {
        self.send(RuntimeEventKind::OfflineRegionTileCountLimitExceeded { region_id: self.region_id, limit });
    }
}
