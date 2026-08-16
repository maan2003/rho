//! Always-on, bounded GUI timing snapshot serialization.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

const MAX_SNAPSHOT_FRAMES: usize = 8_192;
const MAX_SNAPSHOT_EDITOR_EVENTS: usize = 4_096;
static STARTED: OnceLock<Instant> = OnceLock::new();

#[derive(Serialize)]
struct Snapshot<'a> {
    schema: &'static str,
    version: u32,
    captured_unix_ms: u64,
    application: &'static str,
    application_version: &'a str,
    frames: Vec<FrameRecord>,
    editor: Vec<EditorRecord>,
    browser: Vec<BrowserRecord>,
}

#[derive(Serialize)]
struct FrameRecord {
    window_id: u64,
    start_ns: u64,
    draw_ns: u64,
    dirty_to_draw_ns: Option<u64>,
    invalidations: u64,
}

#[derive(Serialize)]
struct EditorRecord {
    stage: &'static str,
    start_ns: u64,
    duration_ns: u64,
    tid: u64,
    input_edits: u64,
    input_start: u64,
    input_rows: u64,
    output_edits: u64,
    output_start: u64,
    output_rows: u64,
    old_rows: u64,
    new_rows: u64,
    pending_batches: u64,
    flags: u64,
}

#[derive(Serialize)]
struct BrowserRecord {
    stage: &'static str,
    scene_id: u64,
    barrier: u64,
    related_scene_id: Option<u64>,
    at_ns: u64,
    duration_ns: Option<u64>,
}

pub fn enable() {
    STARTED.get_or_init(Instant::now);
    gpui::profiler::set_frame_trace_enabled(true);
    gpui::profiler::set_editor_trace_enabled(true);
}

pub(crate) fn snapshot() -> anyhow::Result<Vec<u8>> {
    let started = *STARTED.get_or_init(Instant::now);
    let frames = gpui::profiler::snapshot_frame_timings();
    let editor = gpui::profiler::snapshot_editor_timings();
    let browser = rho_browser::snapshot_browser_timings();
    let frames = frames
        .into_iter()
        .rev()
        .take(MAX_SNAPSHOT_FRAMES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|timing| FrameRecord {
            window_id: timing.window_id.as_u64(),
            start_ns: duration_ns(timing.draw_start.saturating_duration_since(started)),
            draw_ns: duration_ns(timing.draw_duration()),
            dirty_to_draw_ns: timing.dirty_to_draw_duration().map(duration_ns),
            invalidations: timing.invalidations,
        })
        .collect();
    let editor = editor
        .into_iter()
        .rev()
        .take(MAX_SNAPSHOT_EDITOR_EVENTS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|timing| EditorRecord {
            stage: editor_stage_name(timing.kind),
            start_ns: duration_ns(timing.start.saturating_duration_since(started)),
            duration_ns: duration_ns(timing.end.saturating_duration_since(timing.start)),
            tid: timing.tid,
            input_edits: timing.input_edits,
            input_start: timing.input_start,
            input_rows: timing.input_rows,
            output_edits: timing.output_edits,
            output_start: timing.output_start,
            output_rows: timing.output_rows,
            old_rows: timing.old_rows,
            new_rows: timing.new_rows,
            pending_batches: timing.pending_batches,
            flags: timing.flags,
        })
        .collect();
    let browser = browser
        .into_iter()
        .map(|timing| BrowserRecord {
            stage: browser_stage_name(timing.kind),
            scene_id: timing.scene_id,
            barrier: timing.barrier,
            related_scene_id: timing.related_scene_id,
            at_ns: duration_ns(timing.at.saturating_duration_since(started)),
            duration_ns: timing.duration.map(duration_ns),
        })
        .collect();
    let bytes = serde_json::to_vec_pretty(&Snapshot {
        schema: "dev.rho.gui-performance-snapshot",
        version: 2,
        captured_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
        application: "rho-gui",
        application_version: env!("CARGO_PKG_VERSION"),
        frames,
        editor,
        browser,
    })?;
    anyhow::ensure!(
        bytes.len() <= rho_ui_proto::MAX_GUI_TELEMETRY_BYTES,
        "GUI performance snapshot exceeds the upload limit"
    );
    Ok(bytes)
}

fn browser_stage_name(kind: rho_browser::BrowserTimingKind) -> &'static str {
    use rho_browser::BrowserTimingKind::*;
    match kind {
        SceneProduced => "scene_produced",
        SceneCoalesced => "scene_coalesced",
        SceneReceived => "scene_received",
        SceneScheduled => "scene_scheduled",
        ScenePainted => "scene_painted",
        FrameAcknowledged => "frame_acknowledged",
    }
}

fn duration_ns(duration: std::time::Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn editor_stage_name(kind: gpui::profiler::EditorTimingKind) -> &'static str {
    use gpui::profiler::EditorTimingKind::*;
    match kind {
        BufferEdit => "buffer_edit",
        MultiBufferSync => "multi_buffer_sync",
        InlayMapSync => "inlay_map_sync",
        FoldMapSync => "fold_map_sync",
        TabMapSync => "tab_map_sync",
        WrapMapSync => "wrap_map_sync",
        BlockMapSync => "block_map_sync",
        WrapMapUpdate => "wrap_map_update",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn snapshot_is_versioned_and_bounded() {
        super::enable();
        rho_browser::record_browser_timing(
            rho_browser::BrowserTimingKind::ScenePainted,
            42,
            7,
            None,
            Some(std::time::Duration::from_millis(3)),
        );
        let bytes = super::snapshot().unwrap();
        assert!(bytes.len() <= rho_ui_proto::MAX_GUI_TELEMETRY_BYTES);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["schema"], "dev.rho.gui-performance-snapshot");
        assert_eq!(value["version"], 2);
        assert!(value["frames"].is_array());
        assert!(value["editor"].is_array());
        assert!(value["browser"].is_array());
        assert!(value["browser"].as_array().unwrap().iter().any(|record| {
            record["stage"] == "scene_painted" && record["scene_id"] == 42 && record["barrier"] == 7
        }));
    }
}
