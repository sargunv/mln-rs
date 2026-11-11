//! The events of a runtime and its maps as the C structs that an event
//! handler receives: a batch that owns its text and lends views of it.

use ::mln_map::{
    CameraChange, CameraChangeMode, CameraChangeSource, MapEvent, RenderMode, StyleLoadError, TileOperation,
};
use ::mln_runtime::{EventSource, MapId, RuntimeEvent, RuntimeEventKind};
use mln_storage::offline::{OfflineRegionDownloadState, OfflineRegionStatus};
use mln_storage::response::Reason;
use mln_style::sprite::Sprite;
use mln_text::glyph::{GlyphIDType, GlyphRange};

use crate::buffer::LentText;
use crate::generated::types::{
    MLN_CAMERA_CHANGE_MODE_ANIMATED, MLN_CAMERA_CHANGE_MODE_IMMEDIATE, MLN_CAMERA_CHANGE_SOURCE_COMMAND,
    MLN_CAMERA_CHANGE_SOURCE_GESTURE, MLN_CAMERA_CHANGE_SOURCE_MOMENTUM, MLN_MAP_LOAD_ERROR_STYLE_LOAD,
    MLN_MAP_LOAD_ERROR_STYLE_PARSE, MLN_OFFLINE_REGION_DOWNLOAD_ACTIVE, MLN_OFFLINE_REGION_DOWNLOAD_INACTIVE,
    MLN_RENDER_MODE_FULL, MLN_RENDER_MODE_PARTIAL, MLN_RESOURCE_ERROR_REASON_CONNECTION,
    MLN_RESOURCE_ERROR_REASON_NOT_FOUND, MLN_RESOURCE_ERROR_REASON_OTHER, MLN_RESOURCE_ERROR_REASON_RATE_LIMIT,
    MLN_RESOURCE_ERROR_REASON_SERVER, MLN_RUNTIME_EVENT_PAYLOAD_CAMERA_CHANGE, MLN_RUNTIME_EVENT_PAYLOAD_GLYPHS,
    MLN_RUNTIME_EVENT_PAYLOAD_LOADING_FAILED, MLN_RUNTIME_EVENT_PAYLOAD_NONE,
    MLN_RUNTIME_EVENT_PAYLOAD_OFFLINE_REGION_RESPONSE_ERROR, MLN_RUNTIME_EVENT_PAYLOAD_OFFLINE_REGION_STATUS,
    MLN_RUNTIME_EVENT_PAYLOAD_OFFLINE_REGION_TILE_COUNT_LIMIT, MLN_RUNTIME_EVENT_PAYLOAD_RENDER_FRAME,
    MLN_RUNTIME_EVENT_PAYLOAD_SOURCE_CHANGED, MLN_RUNTIME_EVENT_PAYLOAD_SPRITE, MLN_RUNTIME_EVENT_PAYLOAD_STYLE_IMAGE,
    MLN_RUNTIME_EVENT_PAYLOAD_TILE_ACTION, MLN_RUNTIME_EVENT_SOURCE_MAP, MLN_RUNTIME_EVENT_SOURCE_RUNTIME,
    MLN_TILE_OPERATION_CANCELLED, MLN_TILE_OPERATION_END_PARSE, MLN_TILE_OPERATION_ERROR,
    MLN_TILE_OPERATION_LOAD_FROM_CACHE, MLN_TILE_OPERATION_LOAD_FROM_NETWORK, MLN_TILE_OPERATION_NULL,
    MLN_TILE_OPERATION_REQUESTED_FROM_CACHE, MLN_TILE_OPERATION_REQUESTED_FROM_NETWORK, MLN_TILE_OPERATION_START_PARSE,
    mln_buffer_view, mln_camera_change_mode, mln_camera_change_source, mln_event_batch_view, mln_offline_region_status,
    mln_rendering_stats, mln_resource_error_reason, mln_runtime_event, mln_runtime_event_camera_change,
    mln_runtime_event_glyphs, mln_runtime_event_loading_failed, mln_runtime_event_offline_region_response_error,
    mln_runtime_event_offline_region_status, mln_runtime_event_offline_region_tile_count_limit,
    mln_runtime_event_payload, mln_runtime_event_payload_type, mln_runtime_event_render_frame,
    mln_runtime_event_source_changed, mln_runtime_event_sprite, mln_runtime_event_style_image,
    mln_runtime_event_tile_action, mln_tile_id, mln_tile_operation,
};

/// Events as C structs, with the text they lend. The structs point into
/// buffers that the batch owns on the heap, which stay where they are when
/// the batch moves.
pub(crate) struct EventBatch {
    events: Vec<mln_runtime_event>,
    _texts: Vec<LentText>,
    _font_stacks: Vec<Vec<mln_buffer_view>>,
}

// SAFETY: the raw pointers in `events` point only into the heap buffers of
// `_texts` and `_font_stacks`, which the batch owns and never changes, so the
// batch can move to the thread that delivers it as a whole.
unsafe impl Send for EventBatch {}

/// What a batch builds its events with.
#[derive(Default)]
struct Builder {
    texts: Vec<LentText>,
    font_stacks: Vec<Vec<mln_buffer_view>>,
}

impl Builder {
    /// Text that the batch keeps, and its view.
    fn text(&mut self, text: &str) -> mln_buffer_view {
        let text = LentText::new(text);
        let view = text.view();
        self.texts.push(text);
        view
    }

    fn font_stack(&mut self, fonts: &[String]) -> (*const mln_buffer_view, usize) {
        let views: Vec<_> = fonts.iter().map(|font| self.text(font)).collect();
        let pointer = if views.is_empty() { core::ptr::null() } else { views.as_ptr() };
        let count = views.len();
        self.font_stacks.push(views);
        (pointer, count)
    }
}

/// A payload of zeros, the C layer's empty value of every member.
fn zeroed_payload() -> mln_runtime_event_payload {
    // SAFETY: each member of the union is a struct of integers, floats,
    // booleans and nullable pointers, for which zeros are valid.
    unsafe { core::mem::zeroed() }
}

impl EventBatch {
    /// The batch of `events`, which the runtime `runtime` raised: a map event
    /// names its map by the handle that `map_handle` gives.
    pub(crate) fn new(events: &[RuntimeEvent], runtime: u64, map_handle: impl Fn(MapId) -> u64) -> Self {
        let mut builder = Builder::default();
        let events = events
            .iter()
            .map(|event| {
                let (source_type, source) = match event.source {
                    EventSource::Runtime => (MLN_RUNTIME_EVENT_SOURCE_RUNTIME, runtime),
                    EventSource::Map(id) => (MLN_RUNTIME_EVENT_SOURCE_MAP, map_handle(id)),
                };
                let (payload_type, payload, message) = payload(&event.kind, &mut builder);
                mln_runtime_event {
                    type_: event.kind.event_type() as u32,
                    source_type,
                    source,
                    generation: event.generation,
                    payload_type,
                    message: builder.text(message.unwrap_or("")),
                    payload,
                }
            })
            .collect();
        EventBatch { events, _texts: builder.texts, _font_stacks: builder.font_stacks }
    }

    /// Calls `f` with a view of the batch, valid until it returns.
    pub(crate) fn lend<R>(&self, f: impl FnOnce(&mln_event_batch_view) -> R) -> R {
        let view = mln_event_batch_view {
            size: size_of::<mln_event_batch_view>() as u32,
            event_size: size_of::<mln_runtime_event>() as u32,
            events: self.events.as_ptr(),
            event_count: self.events.len(),
        };
        f(&view)
    }
}

/// An event's payload type, payload and message.
fn payload<'a>(
    kind: &'a RuntimeEventKind,
    builder: &mut Builder,
) -> (mln_runtime_event_payload_type, mln_runtime_event_payload, Option<&'a str>) {
    let mut payload = zeroed_payload();
    let (payload_type, message) = match kind {
        RuntimeEventKind::Map(event) => return map_payload(event, builder),
        RuntimeEventKind::OfflineRegionStatusChanged { region_id, status } => {
            payload.offline_region_status =
                mln_runtime_event_offline_region_status { region_id: *region_id, status: offline_status(status) };
            (MLN_RUNTIME_EVENT_PAYLOAD_OFFLINE_REGION_STATUS, None)
        }
        RuntimeEventKind::OfflineRegionResponseError { region_id, error } => {
            payload.offline_region_response_error = mln_runtime_event_offline_region_response_error {
                region_id: *region_id,
                reason: error_reason(error.reason),
            };
            (MLN_RUNTIME_EVENT_PAYLOAD_OFFLINE_REGION_RESPONSE_ERROR, Some(error.message.as_str()))
        }
        RuntimeEventKind::OfflineRegionTileCountLimitExceeded { region_id, limit } => {
            payload.offline_region_tile_count_limit =
                mln_runtime_event_offline_region_tile_count_limit { region_id: *region_id, limit: *limit };
            (MLN_RUNTIME_EVENT_PAYLOAD_OFFLINE_REGION_TILE_COUNT_LIMIT, None)
        }
    };
    (payload_type, payload, message)
}

fn map_payload<'a>(
    event: &'a MapEvent,
    builder: &mut Builder,
) -> (mln_runtime_event_payload_type, mln_runtime_event_payload, Option<&'a str>) {
    let mut payload = zeroed_payload();
    let (payload_type, message) = match event {
        MapEvent::CameraWillChange(change) | MapEvent::CameraDidChange(change) => {
            payload.camera_change = camera_change(*change);
            (MLN_RUNTIME_EVENT_PAYLOAD_CAMERA_CHANGE, None)
        }
        MapEvent::LoadingFailed(error) => {
            let error_kind = match error {
                StyleLoadError::Parse(_) => MLN_MAP_LOAD_ERROR_STYLE_PARSE,
                StyleLoadError::Load(_) => MLN_MAP_LOAD_ERROR_STYLE_LOAD,
            };
            payload.loading_failed = mln_runtime_event_loading_failed { error: error_kind };
            (MLN_RUNTIME_EVENT_PAYLOAD_LOADING_FAILED, Some(error.message()))
        }
        MapEvent::RenderError(error) => (MLN_RUNTIME_EVENT_PAYLOAD_NONE, Some(error.message.as_str())),
        MapEvent::RenderFrameFinished(status) => {
            let stats = &status.rendering_stats;
            payload.render_frame = mln_runtime_event_render_frame {
                mode: render_mode(status.mode),
                needs_repaint: status.needs_repaint,
                placement_changed: status.placement_changed,
                stats: mln_rendering_stats {
                    // The renderer measures these in seconds.
                    encoding_time_ms: stats.encoding_time * 1000.0,
                    rendering_time_ms: stats.rendering_time * 1000.0,
                    frame_count: i64::from(stats.num_frames),
                    draw_call_count: i64::from(stats.num_draw_calls),
                    total_draw_call_count: i64::from(stats.total_draw_calls),
                },
            };
            (MLN_RUNTIME_EVENT_PAYLOAD_RENDER_FRAME, None)
        }
        MapEvent::StyleImageMissing { id } => {
            payload.style_image = mln_runtime_event_style_image { image_id: builder.text(id) };
            (MLN_RUNTIME_EVENT_PAYLOAD_STYLE_IMAGE, None)
        }
        MapEvent::TileAction { op, id, source_id } => {
            payload.tile_action = mln_runtime_event_tile_action {
                source_id: builder.text(source_id),
                operation: tile_operation(*op),
                tile_id: mln_tile_id {
                    overscaled_z: u32::from(id.overscaled_z),
                    wrap: i32::from(id.wrap),
                    canonical_z: u32::from(id.canonical.z),
                    canonical_x: id.canonical.x,
                    canonical_y: id.canonical.y,
                },
            };
            (MLN_RUNTIME_EVENT_PAYLOAD_TILE_ACTION, None)
        }
        MapEvent::SourceChanged { source_id } => {
            payload.source_changed = mln_runtime_event_source_changed { source_id: builder.text(source_id) };
            (MLN_RUNTIME_EVENT_PAYLOAD_SOURCE_CHANGED, None)
        }
        MapEvent::GlyphsRequested { font_stack, range } | MapEvent::GlyphsLoaded { font_stack, range } => {
            payload.glyphs = glyphs(font_stack, *range, builder);
            (MLN_RUNTIME_EVENT_PAYLOAD_GLYPHS, None)
        }
        MapEvent::GlyphsError { font_stack, range, error } => {
            payload.glyphs = glyphs(font_stack, *range, builder);
            (MLN_RUNTIME_EVENT_PAYLOAD_GLYPHS, Some(error.message.as_str()))
        }
        MapEvent::SpriteRequested(sprite) | MapEvent::SpriteLoaded(sprite) => {
            payload.sprite = self::sprite(sprite.as_ref(), builder);
            (MLN_RUNTIME_EVENT_PAYLOAD_SPRITE, None)
        }
        MapEvent::SpriteError(sprite, error) => {
            payload.sprite = self::sprite(sprite.as_ref(), builder);
            (MLN_RUNTIME_EVENT_PAYLOAD_SPRITE, Some(error.message.as_str()))
        }
        MapEvent::CameraIsChanging
        | MapEvent::StyleLoaded
        | MapEvent::LoadingStarted
        | MapEvent::LoadingFinished
        | MapEvent::Idle
        | MapEvent::RenderFrameStarted
        | MapEvent::RenderMapStarted
        | MapEvent::RenderMapFinished(_) => (MLN_RUNTIME_EVENT_PAYLOAD_NONE, None),
    };
    (payload_type, payload, message)
}

fn camera_change(change: CameraChange) -> mln_runtime_event_camera_change {
    let mode: mln_camera_change_mode = match change.mode {
        CameraChangeMode::Immediate => MLN_CAMERA_CHANGE_MODE_IMMEDIATE,
        CameraChangeMode::Animated => MLN_CAMERA_CHANGE_MODE_ANIMATED,
    };
    let source: mln_camera_change_source = match change.source {
        CameraChangeSource::Command => MLN_CAMERA_CHANGE_SOURCE_COMMAND,
        CameraChangeSource::Gesture => MLN_CAMERA_CHANGE_SOURCE_GESTURE,
        CameraChangeSource::Momentum => MLN_CAMERA_CHANGE_SOURCE_MOMENTUM,
    };
    mln_runtime_event_camera_change { mode, source }
}

fn render_mode(mode: RenderMode) -> u32 {
    match mode {
        RenderMode::Partial => MLN_RENDER_MODE_PARTIAL,
        RenderMode::Full => MLN_RENDER_MODE_FULL,
    }
}

fn tile_operation(operation: TileOperation) -> mln_tile_operation {
    match operation {
        TileOperation::RequestedFromCache => MLN_TILE_OPERATION_REQUESTED_FROM_CACHE,
        TileOperation::RequestedFromNetwork => MLN_TILE_OPERATION_REQUESTED_FROM_NETWORK,
        TileOperation::LoadFromNetwork => MLN_TILE_OPERATION_LOAD_FROM_NETWORK,
        TileOperation::LoadFromCache => MLN_TILE_OPERATION_LOAD_FROM_CACHE,
        TileOperation::StartParse => MLN_TILE_OPERATION_START_PARSE,
        TileOperation::EndParse => MLN_TILE_OPERATION_END_PARSE,
        TileOperation::Error => MLN_TILE_OPERATION_ERROR,
        TileOperation::Cancelled => MLN_TILE_OPERATION_CANCELLED,
        TileOperation::NullOp => MLN_TILE_OPERATION_NULL,
    }
}

fn glyphs(font_stack: &[String], range: GlyphRange, builder: &mut Builder) -> mln_runtime_event_glyphs {
    let (fonts, font_count) = builder.font_stack(font_stack);
    mln_runtime_event_glyphs {
        font_stack: fonts,
        font_count,
        first: u32::from(range.first),
        last: u32::from(range.second),
        font_face: range.type_ != GlyphIDType::FontPBF,
    }
}

fn sprite(sprite: Option<&Sprite>, builder: &mut Builder) -> mln_runtime_event_sprite {
    let (id, url) = sprite.map_or(("", ""), |sprite| (sprite.id.as_str(), sprite.sprite_url.as_str()));
    mln_runtime_event_sprite { id: builder.text(id), url: builder.text(url) }
}

pub(crate) fn error_reason(reason: Reason) -> mln_resource_error_reason {
    match reason {
        Reason::NotFound => MLN_RESOURCE_ERROR_REASON_NOT_FOUND,
        Reason::Server => MLN_RESOURCE_ERROR_REASON_SERVER,
        Reason::Connection => MLN_RESOURCE_ERROR_REASON_CONNECTION,
        Reason::RateLimit => MLN_RESOURCE_ERROR_REASON_RATE_LIMIT,
        Reason::Success | Reason::Other => MLN_RESOURCE_ERROR_REASON_OTHER,
    }
}

pub(crate) fn offline_status(status: &OfflineRegionStatus) -> mln_offline_region_status {
    mln_offline_region_status {
        download_state: match status.download_state {
            OfflineRegionDownloadState::Inactive => MLN_OFFLINE_REGION_DOWNLOAD_INACTIVE,
            OfflineRegionDownloadState::Active => MLN_OFFLINE_REGION_DOWNLOAD_ACTIVE,
        },
        completed_resource_count: status.completed_resource_count,
        completed_resource_size: status.completed_resource_size,
        completed_tile_count: status.completed_tile_count,
        required_tile_count: status.required_tile_count,
        completed_tile_size: status.completed_tile_size,
        required_resource_count: status.required_resource_count,
        required_resource_count_is_precise: status.required_resource_count_is_precise,
        complete: status.complete(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ::mln_map::{RenderFrameStatus, RenderingStats};
    use ::mln_tile_id::{CanonicalTileID, OverscaledTileID};

    use super::*;
    use crate::generated::types::{
        MLN_RUNTIME_EVENT_MAP_CAMERA_WILL_CHANGE, MLN_RUNTIME_EVENT_MAP_GLYPHS_ERROR,
        MLN_RUNTIME_EVENT_MAP_RENDER_FRAME_FINISHED, MLN_RUNTIME_EVENT_MAP_TILE_ACTION,
        MLN_RUNTIME_EVENT_OFFLINE_REGION_TILE_COUNT_LIMIT_EXCEEDED,
    };

    fn text(view: mln_buffer_view) -> (String, bool) {
        // SAFETY: a view of lent text, and the NUL after it.
        let bytes = unsafe { core::slice::from_raw_parts(view.data.cast::<u8>(), view.size + 1) };
        (String::from_utf8(bytes[..view.size].to_vec()).unwrap(), bytes[view.size] == 0)
    }

    fn event(source: EventSource, kind: RuntimeEventKind) -> RuntimeEvent {
        RuntimeEvent { source, generation: 9, kind }
    }

    #[test]
    fn a_batch_names_its_sources_and_lends_its_text() {
        let events = [
            event(
                EventSource::Map(MapId(3)),
                RuntimeEventKind::Map(MapEvent::CameraWillChange(CameraChange {
                    mode: CameraChangeMode::Animated,
                    source: CameraChangeSource::Gesture,
                })),
            ),
            event(
                EventSource::Map(MapId(3)),
                RuntimeEventKind::Map(MapEvent::TileAction {
                    op: TileOperation::EndParse,
                    id: OverscaledTileID::new(4, -1, CanonicalTileID::new(3, 1, 2)),
                    source_id: "tiles".into(),
                }),
            ),
            event(
                EventSource::Map(MapId(3)),
                RuntimeEventKind::Map(MapEvent::GlyphsError {
                    font_stack: vec!["Open Sans".into(), "Noto".into()],
                    range: GlyphRange::new(256, 511, GlyphIDType::FontPBF),
                    error: mln_util::Error::native("no glyphs"),
                }),
            ),
            event(
                EventSource::Runtime,
                RuntimeEventKind::OfflineRegionTileCountLimitExceeded { region_id: 5, limit: 6 },
            ),
        ];
        let batch = EventBatch::new(&events, 100, |id| 200 + id.0);
        // The views stay valid when the batch moves to another thread.
        std::thread::spawn(move || {
            batch.lend(|view| {
                assert_eq!((view.event_count, view.event_size as usize), (4, size_of::<mln_runtime_event>()));
                // SAFETY: the batch's events.
                let events = unsafe { core::slice::from_raw_parts(view.events, view.event_count) };
                let [camera, tile, glyphs, limit] = events else { unreachable!() };

                assert_eq!(
                    (camera.type_, camera.source_type, camera.source),
                    (MLN_RUNTIME_EVENT_MAP_CAMERA_WILL_CHANGE, MLN_RUNTIME_EVENT_SOURCE_MAP, 203)
                );
                assert_eq!(camera.generation, 9);
                // SAFETY: the member that the payload type names.
                let change = unsafe { camera.payload.camera_change };
                assert_eq!(
                    (change.mode, change.source),
                    (MLN_CAMERA_CHANGE_MODE_ANIMATED, MLN_CAMERA_CHANGE_SOURCE_GESTURE)
                );
                assert_eq!(text(camera.message), (String::new(), true));

                assert_eq!(tile.type_, MLN_RUNTIME_EVENT_MAP_TILE_ACTION);
                // SAFETY: as above.
                let action = unsafe { tile.payload.tile_action };
                assert_eq!(text(action.source_id).0, "tiles");
                assert_eq!((action.tile_id.overscaled_z, action.tile_id.wrap, action.tile_id.canonical_y), (4, -1, 2));

                assert_eq!(glyphs.type_, MLN_RUNTIME_EVENT_MAP_GLYPHS_ERROR);
                assert_eq!(text(glyphs.message), ("no glyphs".to_owned(), true));
                // SAFETY: as above.
                let range = unsafe { glyphs.payload.glyphs };
                assert_eq!((range.first, range.last, range.font_face, range.font_count), (256, 511, false, 2));
                // SAFETY: the payload's fonts.
                let fonts = unsafe { core::slice::from_raw_parts(range.font_stack, range.font_count) };
                assert_eq!(text(fonts[1]).0, "Noto");

                assert_eq!(
                    (limit.type_, limit.source_type, limit.source),
                    (MLN_RUNTIME_EVENT_OFFLINE_REGION_TILE_COUNT_LIMIT_EXCEEDED, MLN_RUNTIME_EVENT_SOURCE_RUNTIME, 100)
                );
                // SAFETY: as above.
                assert_eq!(unsafe { limit.payload.offline_region_tile_count_limit }.limit, 6);
            });
        })
        .join()
        .unwrap();
    }

    #[test]
    fn frame_stats_arrive_in_milliseconds() {
        let stats = RenderingStats { encoding_time: 0.5, num_frames: 3, ..RenderingStats::default() };
        let status = RenderFrameStatus {
            mode: RenderMode::Full,
            needs_repaint: true,
            placement_changed: false,
            rendering_stats: Arc::new(stats),
        };
        let events = [event(EventSource::Map(MapId(1)), RuntimeEventKind::Map(MapEvent::RenderFrameFinished(status)))];
        EventBatch::new(&events, 0, |_| 1).lend(|view| {
            // SAFETY: the batch's one event.
            let event = unsafe { &*view.events };
            assert_eq!(event.type_, MLN_RUNTIME_EVENT_MAP_RENDER_FRAME_FINISHED);
            // SAFETY: the member that the payload type names.
            let frame = unsafe { event.payload.render_frame };
            assert_eq!((frame.mode, frame.needs_repaint, frame.stats.frame_count), (MLN_RENDER_MODE_FULL, true, 3));
            assert_eq!(frame.stats.encoding_time_ms, 500.0);
        });
    }
}
