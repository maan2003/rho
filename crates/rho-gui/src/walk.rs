//! Deterministic, in-process GUI random walks over GPUI's real workspace scene.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use editor::Editor;
use gpui::{App, Entity, TestAppContext, TestDispatcher, WindowHandle, px, size};
use rho_agents::state::{
    UiAgentState, UiAgentStatus, UiBlock, UiMessagePhase, UiTool, UiToolStatus,
};
use rho_core::UnixMs;
use rho_ui_proto::{AgentId, AgentIdDomain};
use serde::{Deserialize, Serialize};
use settings::SettingsStore;

use crate::workspace::{AttachTarget, HostSpec, Workspace};

/// One action understood by the generator, recorder, drive script and shrinker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalkEvent {
    ComposerKey { character: char },
    Resize { width: u16, height: u16 },
    AgentChunk { bytes: u16 },
    ToolBody { bytes: u16 },
    AdvanceTime { milliseconds: u16 },
    Idle,
}

/// Whether wall-clock draw cost is an oracle in addition to deterministic work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkMode {
    Debug,
    Profiling,
}

/// A deterministic run request.
#[derive(Clone, Copy, Debug)]
pub struct WalkConfig {
    pub seed: u64,
    pub steps: usize,
    pub mode: WalkMode,
}

/// Owns one deterministic random-walk request and its replay oracle.
#[derive(Clone, Copy, Debug)]
pub struct WalkHarness {
    config: WalkConfig,
}

impl WalkHarness {
    pub fn new(config: WalkConfig) -> Self {
        Self { config }
    }

    /// Runs, minimizes failures, then repeats the walk to prove scene
    /// stability.
    pub fn run(self) -> Result<WalkReport, WalkFailure> {
        deterministic(self.config)
    }
}

/// Numeric evidence retained from a successful run.
#[derive(Clone, Debug, PartialEq)]
pub struct WalkReport {
    pub seed: u64,
    pub steps: usize,
    pub frames: usize,
    pub distinct_scenes: usize,
    pub max_changed_primitives: usize,
    pub max_editor_rows: u64,
    pub max_draw_micros: u64,
    pub cold_draw_micros: u64,
    pub warm_draw_micros: u64,
    pub step_draw_micros: Vec<u64>,
    pub step_editor_rows: Vec<u64>,
    pub step_total_rows: Vec<u64>,
    pub scene_hashes: Vec<u64>,
    pub wall_clock_findings: Vec<WallClockFinding>,
    pub baseline_owners: Vec<OwnerFrameSummary>,
    pub step_owners: Vec<Vec<OwnerFrameSummary>>,
}

/// A profiling-only observation. It is reported by the gate but is not a
/// deterministic landing failure until the known slow cases are repaired.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WallClockFinding {
    pub step: usize,
    pub event: WalkEvent,
    pub draw_micros: u64,
    pub editor_work_rows: u64,
    pub sequence: Vec<WalkEvent>,
}

/// Numeric paint work attributed to one typed scene owner.
#[derive(Clone, Debug, PartialEq)]
pub struct OwnerFrameSummary {
    pub owner: String,
    pub primitives: usize,
    pub changed_primitives: usize,
    pub paint_nanos: u64,
    pub bounds: [f32; 4],
}

/// A shrunk drive script and the oracle which rejected it.
#[derive(Clone, Debug)]
pub struct WalkFailure {
    pub oracle: &'static str,
    pub step: usize,
    pub event: Option<WalkEvent>,
    pub events: Vec<WalkEvent>,
    pub draw_micros: u64,
    pub editor_rows: u64,
    pub cold_draw_micros: u64,
    pub warm_draw_micros: u64,
    pub scene_details: Vec<String>,
}

impl std::fmt::Display for WalkFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} after {} events",
            self.oracle,
            self.events.len()
        )
    }
}

impl std::error::Error for WalkFailure {}

/// Runs one generated sequence and deletion-shrinks an oracle failure.
pub fn run(config: WalkConfig) -> Result<WalkReport, WalkFailure> {
    let events = generate(config.seed, config.steps);
    match run_events(config.seed, config.mode, &events) {
        Ok(report) => Ok(report),
        Err(rejection) => {
            let oracle = rejection.0;
            let mut final_rejection = rejection;
            let mut shrunk = events;
            let mut index = 0;
            while index < shrunk.len() {
                let mut candidate = shrunk.clone();
                candidate.remove(index);
                match run_events(config.seed, config.mode, &candidate) {
                    Err(rejection) if rejection.0 == oracle => {
                        shrunk = candidate;
                        final_rejection = rejection;
                    }
                    _ => index += 1,
                }
            }
            for index in 0..shrunk.len() {
                loop {
                    let mut reduced = false;
                    for replacement in smaller_values(&shrunk[index]) {
                        let mut candidate = shrunk.clone();
                        candidate[index] = replacement;
                        if let Err(rejection) = run_events(config.seed, config.mode, &candidate)
                            && rejection.0 == oracle
                        {
                            shrunk = candidate;
                            final_rejection = rejection;
                            reduced = true;
                            break;
                        }
                    }
                    if !reduced {
                        break;
                    }
                }
            }
            let (
                _,
                step,
                event,
                draw_micros,
                editor_rows,
                cold_draw_micros,
                warm_draw_micros,
                scene_details,
            ) = final_rejection;
            Err(WalkFailure {
                oracle,
                step,
                event,
                events: shrunk,
                draw_micros,
                editor_rows,
                cold_draw_micros,
                warm_draw_micros,
                scene_details,
            })
        }
    }
}

fn smaller_values(event: &WalkEvent) -> Vec<WalkEvent> {
    fn smaller(value: u16, minimum: u16) -> Vec<u16> {
        let half = minimum + value.saturating_sub(minimum) / 2;
        [minimum, half]
            .into_iter()
            .filter(|candidate| *candidate < value)
            .collect()
    }

    match event {
        WalkEvent::ComposerKey { character } if *character != 'a' => {
            vec![WalkEvent::ComposerKey { character: 'a' }]
        }
        WalkEvent::Resize { width, height } => {
            let mut candidates = Vec::new();
            for width in smaller(*width, 480) {
                candidates.push(WalkEvent::Resize {
                    width,
                    height: *height,
                });
            }
            for height in smaller(*height, 600) {
                candidates.push(WalkEvent::Resize {
                    width: *width,
                    height,
                });
            }
            candidates
        }
        WalkEvent::AgentChunk { bytes } => smaller(*bytes, 1)
            .into_iter()
            .map(|bytes| WalkEvent::AgentChunk { bytes })
            .collect(),
        WalkEvent::ToolBody { bytes } => smaller(*bytes, 1)
            .into_iter()
            .map(|bytes| WalkEvent::ToolBody { bytes })
            .collect(),
        WalkEvent::AdvanceTime { milliseconds } => smaller(*milliseconds, 1)
            .into_iter()
            .map(|milliseconds| WalkEvent::AdvanceTime { milliseconds })
            .collect(),
        _ => Vec::new(),
    }
}

fn generate(seed: u64, steps: usize) -> Vec<WalkEvent> {
    let mut random = seed;
    (0..steps)
        .map(|_| {
            random = random
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            match random % 6 {
                0 | 1 => WalkEvent::ComposerKey {
                    character: char::from(b'a' + ((random >> 8) % 26) as u8),
                },
                2 => WalkEvent::Resize {
                    width: 480 + ((random >> 16) % 801) as u16,
                    height: 600 + ((random >> 32) % 401) as u16,
                },
                3 => WalkEvent::AgentChunk {
                    bytes: 1 + ((random >> 12) % 256) as u16,
                },
                4 => WalkEvent::ToolBody {
                    bytes: generated_result_bytes(random),
                },
                _ if (random >> 48) & 1 == 0 => WalkEvent::AdvanceTime {
                    milliseconds: 1 + ((random >> 36) % 1000) as u16,
                },
                _ => WalkEvent::Idle,
            }
        })
        .collect()
}

// Generated tool results follow the measured corpus: p50 ≈ 227 B, mean ≈
// 4 KiB, p90 ≈ 13 KiB. Assistant stream deltas stay independently small.
fn generated_result_bytes(random: u64) -> u16 {
    let percentile = (random >> 8) % 100;
    if percentile < 70 {
        1 + ((random >> 20) % 454) as u16
    } else if percentile < 90 {
        455 + ((random >> 20) % 12_546) as u16
    } else {
        13_001 + ((random >> 20) % 16_999) as u16
    }
}

fn run_events(
    seed: u64,
    mode: WalkMode,
    events: &[WalkEvent],
) -> Result<
    WalkReport,
    (
        &'static str,
        usize,
        Option<WalkEvent>,
        u64,
        u64,
        u64,
        u64,
        Vec<String>,
    ),
> {
    run_events_with_detached_host(seed, mode, events, false)
}

fn run_events_with_detached_host(
    seed: u64,
    mode: WalkMode,
    events: &[WalkEvent],
    detached_host: bool,
) -> Result<
    WalkReport,
    (
        &'static str,
        usize,
        Option<WalkEvent>,
        u64,
        u64,
        u64,
        u64,
        Vec<String>,
    ),
> {
    gpui::profiler::set_editor_trace_enabled(true);
    gpui::profiler::set_frame_trace_enabled(true);
    let mut timings = gpui::profiler::EditorTimingCollector::new();
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(seed),
        None,
        Arc::new(gpui_wgpu::CosmicTextSystem::new_without_system_fonts(
            "Lilex",
        )),
    );
    init(&mut cx);
    let isolated = detached_host
        .then(tempfile::tempdir)
        .transpose()
        .map_err(|_| {
            (
                "create isolated walk state",
                0,
                None,
                0,
                0,
                0,
                0,
                Vec::new(),
            )
        })?;
    let specs = isolated
        .as_ref()
        .map(|isolated| HostSpec {
            name: "generated".to_owned(),
            target: AttachTarget::Unix(isolated.path().join("daemon.sock")),
        })
        .into_iter()
        .collect();
    let workspace = cx.add_window(|window, cx| Workspace::new(specs, window, cx));
    let agent = AgentId::from_counter(1, &AgentIdDomain(0)).expect("generated agent id");
    let mut state = initial_state();
    workspace
        .update(&mut cx, |workspace, window, cx| {
            workspace.select_agent(Some(agent), window, cx);
            workspace.seed_transcript_for_test(agent, state.clone(), window, cx);
        })
        .map_err(|_| ("workspace closed", 0, None, 0, 0, 0, 0, Vec::new()))?;
    cx.run_until_parked();
    let editor = active_editor(&workspace, &mut cx)
        .map_err(|_| ("workspace closed", 0, None, 0, 0, 0, 0, Vec::new()))?;
    let cold_started = Instant::now();
    cx.draw_window(*workspace);
    gpui::profiler::take_frame_work();
    let cold_draw_micros = cold_started.elapsed().as_micros() as u64;
    let warm_started = Instant::now();
    cx.draw_window(*workspace);
    gpui::profiler::take_frame_work();
    let warm_draw_micros = warm_started.elapsed().as_micros() as u64;
    let recorder = cx.record_scenes::<WalkEvent>(*workspace);
    cx.draw_window(*workspace);
    gpui::profiler::take_frame_work();
    timings.collect_unseen();

    let mut max_changed_primitives = 0;
    let mut max_editor_rows = 0;
    let mut max_draw_micros = 0;
    let mut step_draw_micros = Vec::with_capacity(events.len());
    let mut step_editor_rows = Vec::with_capacity(events.len());
    let mut step_total_rows = Vec::with_capacity(events.len());
    let mut work_exceeded = None;
    let mut work_baselines: [Option<(u64, u64)>; 6] = [None; 6];
    let mut wall_clock_findings = Vec::new();
    let mut virtual_time = Duration::ZERO;
    let mut live_last_changed = HashMap::new();
    let baseline_owners = summarize_owners(&recorder, &recorder.frames()[0]);
    let mut step_owners = Vec::with_capacity(events.len());
    for (step, event) in events.iter().enumerate() {
        if let WalkEvent::AdvanceTime { milliseconds } = event {
            virtual_time += Duration::from_millis(u64::from(*milliseconds));
        }
        let frame_start = recorder.frames().len();
        let allowed_y = match event {
            WalkEvent::ComposerKey { .. } => prompt_row(&workspace, &editor, &mut cx),
            _ => None,
        };
        recorder.precede(event.clone());
        apply(event, agent, &mut state, &workspace, &editor, &mut cx).map_err(|_| {
            (
                "event application",
                step,
                Some(event.clone()),
                0,
                0,
                cold_draw_micros,
                warm_draw_micros,
                Vec::new(),
            )
        })?;
        cx.run_until_parked();
        let draw_started = Instant::now();
        cx.draw_window(*workspace);
        let work = gpui::profiler::take_frame_work();
        let draw_micros = draw_started.elapsed().as_micros() as u64;
        max_draw_micros = max_draw_micros.max(draw_micros);
        step_draw_micros.push(draw_micros);
        let event_timings = timings.collect_unseen();
        let rows = event_timings
            .iter()
            .map(|timing| timing.old_rows.max(timing.new_rows))
            .sum::<u64>();
        max_editor_rows = max_editor_rows.max(rows);
        step_editor_rows.push(rows);
        let total_rows = work.total_rows.max(
            event_timings
                .iter()
                .map(|timing| timing.old_rows.max(timing.new_rows))
                .max()
                .unwrap_or(0),
        );
        step_total_rows.push(total_rows);
        if mode == WalkMode::Profiling && draw_micros > 4_000 {
            wall_clock_findings.push(WallClockFinding {
                step,
                event: event.clone(),
                draw_micros,
                editor_work_rows: rows,
                sequence: events[..=step].to_vec(),
            });
        }
        if mode == WalkMode::Profiling {
            let (class, scale) = row_work_scale(event, total_rows);
            match work_baselines[class] {
                Some((baseline_scale, baseline_rows)) => {
                    let expected = baseline_rows
                        .saturating_mul(scale)
                        .div_ceil(baseline_scale)
                        .saturating_mul(2)
                        .saturating_add(64);
                    if rows > expected && work_exceeded.is_none() {
                        work_exceeded = Some((step, event.clone(), draw_micros, rows));
                    }
                    if scale < baseline_scale {
                        work_baselines[class] = Some((scale, rows));
                    }
                }
                None => work_baselines[class] = Some((scale, rows)),
            }
        }

        let frames = recorder.frames();
        let produced = &frames[frame_start..];
        if produced.len() != 1 {
            return Err((
                "one event did not produce exactly one frame",
                step,
                Some(event.clone()),
                draw_micros,
                0,
                cold_draw_micros,
                warm_draw_micros,
                Vec::new(),
            ));
        }
        let frame = &produced[0];
        step_owners.push(summarize_owners(&recorder, frame));
        max_changed_primitives = max_changed_primitives.max(frame.changes.len());
        if matches!(event, WalkEvent::Idle) && !frame.changes.is_empty() {
            let scene_details = describe_changes(&recorder, frame);
            return Err((
                "idle sub-scene changed",
                step,
                Some(event.clone()),
                draw_micros,
                0,
                cold_draw_micros,
                warm_draw_micros,
                scene_details,
            ));
        }
        if matches!(event, WalkEvent::AdvanceTime { .. })
            && !frame.changes.is_empty()
            && !changes_follow_declared_cadence(
                &recorder,
                frame,
                virtual_time,
                &mut live_last_changed,
            )
        {
            return Err((
                "time changed outside a declared live cadence",
                step,
                Some(event.clone()),
                draw_micros,
                0,
                cold_draw_micros,
                warm_draw_micros,
                describe_changes(&recorder, frame),
            ));
        }
        if let (Some((top, bottom)), Some(changed)) = (allowed_y, frame.change_bounds)
            && (changed.origin.y.0 < top - 1. || changed.bottom().0 > bottom + 1.)
        {
            return Err((
                "composer damage escaped its row",
                step,
                Some(event.clone()),
                draw_micros,
                0,
                cold_draw_micros,
                warm_draw_micros,
                describe_changes(&recorder, frame),
            ));
        }
    }

    if let Some((step, event, draw_micros, rows)) = work_exceeded {
        return Err((
            "editor work exceeded changed rows plus log total",
            step,
            Some(event),
            draw_micros,
            rows,
            cold_draw_micros,
            warm_draw_micros,
            Vec::new(),
        ));
    }

    let frames = recorder.frames();
    let scenes = recorder.distinct_scenes();
    Ok(WalkReport {
        seed,
        steps: events.len(),
        frames: frames.len().saturating_sub(1),
        distinct_scenes: scenes.len(),
        max_changed_primitives,
        max_editor_rows,
        max_draw_micros,
        cold_draw_micros,
        warm_draw_micros,
        step_draw_micros,
        step_editor_rows,
        step_total_rows,
        scene_hashes: frames
            .iter()
            .skip(1)
            .map(|frame| scenes[frame.distinct_scene].hash)
            .collect(),
        wall_clock_findings,
        baseline_owners,
        step_owners,
    })
}

fn summarize_owners(
    recorder: &gpui::SceneRecorder<WalkEvent>,
    frame: &gpui::SceneFrame<WalkEvent>,
) -> Vec<OwnerFrameSummary> {
    frame
        .subscenes
        .iter()
        .map(|subscene| OwnerFrameSummary {
            owner: format!("{:?}", recorder.owner(subscene.id)),
            primitives: subscene.primitive_count,
            changed_primitives: frame
                .changes
                .iter()
                .filter(|change| change.subscene == subscene.id)
                .count(),
            paint_nanos: subscene.paint_elapsed.as_nanos() as u64,
            bounds: [
                subscene.bounds.origin.x.0,
                subscene.bounds.origin.y.0,
                subscene.bounds.size.width.0,
                subscene.bounds.size.height.0,
            ],
        })
        .collect()
}

fn describe_changes(
    recorder: &gpui::SceneRecorder<WalkEvent>,
    frame: &gpui::SceneFrame<WalkEvent>,
) -> Vec<String> {
    let mut details = vec![format!("primitive_changes_total={}", frame.changes.len())];
    details.extend(frame.changes.iter().take(32).map(|change| {
        format!(
            "owner={:?} primitive={} before={:?} after={:?}",
            recorder.owner(change.subscene),
            change.index,
            change
                .before
                .as_ref()
                .map(|primitive| (primitive.fingerprint, primitive.bounds)),
            change
                .after
                .as_ref()
                .map(|primitive| (primitive.fingerprint, primitive.bounds)),
        )
    }));
    details
}

/// Time may only change owners that declare their own cadence in the scene.
fn changes_follow_declared_cadence(
    recorder: &gpui::SceneRecorder<WalkEvent>,
    frame: &gpui::SceneFrame<WalkEvent>,
    now: Duration,
    last_changed: &mut HashMap<gpui::SceneOwner, Duration>,
) -> bool {
    let Some(owners) = frame
        .changes
        .iter()
        .map(|change| recorder.owner(change.subscene))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    if owners.iter().any(|owner| {
        let Some(live) = owner.live_owner() else {
            return true;
        };
        now.saturating_sub(last_changed.get(owner).copied().unwrap_or_default()) < live.cadence
    }) {
        return false;
    }
    for owner in owners {
        last_changed.insert(owner, now);
    }
    true
}

fn row_work_scale(event: &WalkEvent, total_rows: u64) -> (usize, u64) {
    let changed_rows = match event {
        WalkEvent::ComposerKey { .. } => 1,
        WalkEvent::AgentChunk { bytes } | WalkEvent::ToolBody { bytes } => {
            u64::from(*bytes).div_ceil(64).max(1)
        }
        WalkEvent::Resize { .. } => total_rows,
        WalkEvent::AdvanceTime { .. } | WalkEvent::Idle => 0,
    };
    let log_total = u64::from(u64::BITS - total_rows.max(1).leading_zeros());
    let class = match event {
        WalkEvent::ComposerKey { .. } => 0,
        WalkEvent::Resize { .. } => 1,
        WalkEvent::AgentChunk { .. } => 2,
        WalkEvent::ToolBody { .. } => 3,
        WalkEvent::AdvanceTime { .. } => 4,
        WalkEvent::Idle => 5,
    };
    (class, changed_rows + log_total + 2)
}

fn apply(
    event: &WalkEvent,
    agent: AgentId,
    state: &mut UiAgentState,
    workspace: &WindowHandle<Workspace>,
    editor: &Entity<Editor>,
    cx: &mut TestAppContext,
) -> anyhow::Result<()> {
    match event {
        WalkEvent::ComposerKey { character } => workspace.update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.insert(&character.to_string(), window, cx)
            });
        })?,
        WalkEvent::Resize { width, height } => {
            cx.simulate_window_resize(**workspace, size(px(*width as f32), px(*height as f32)));
        }
        WalkEvent::AgentChunk { bytes } => {
            let text = generated_text(usize::from(*bytes));
            let UiBlock::AssistantMessage { text: body, .. } =
                Arc::make_mut(state.blocks.last_mut().context("assistant block")?)
            else {
                anyhow::bail!("last block is not assistant text")
            };
            body.push_str(&text);
            seed(agent, state, workspace, cx)?;
        }
        WalkEvent::ToolBody { bytes } => {
            let UiBlock::Tool(tool) = Arc::make_mut(&mut state.blocks[1]) else {
                anyhow::bail!("generated tool block moved")
            };
            tool.output = Some(generated_text(usize::from(*bytes)));
            seed(agent, state, workspace, cx)?;
        }
        WalkEvent::AdvanceTime { milliseconds } => {
            cx.dispatcher
                .advance_clock(std::time::Duration::from_millis(u64::from(*milliseconds)));
        }
        WalkEvent::Idle => {}
    }
    Ok(())
}

fn generated_text(bytes: usize) -> String {
    const TEXT: &str = "generated result: compiled 12 targets in 0.42s\nnext line has ordinary prose and wrapped words\n";
    TEXT.chars().cycle().take(bytes).collect()
}

fn seed(
    agent: AgentId,
    state: &UiAgentState,
    workspace: &WindowHandle<Workspace>,
    cx: &mut TestAppContext,
) -> anyhow::Result<()> {
    workspace.update(cx, |workspace, window, cx| {
        workspace.seed_transcript_for_test(agent, state.clone(), window, cx)
    })?;
    Ok(())
}

fn active_editor(
    workspace: &WindowHandle<Workspace>,
    cx: &mut TestAppContext,
) -> anyhow::Result<Entity<Editor>> {
    workspace.update(cx, |workspace, _, cx| workspace.active_editor(cx))
}

fn prompt_row(
    workspace: &WindowHandle<Workspace>,
    editor: &Entity<Editor>,
    cx: &mut TestAppContext,
) -> Option<(f32, f32)> {
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                let point = editor.selections.newest_display(&snapshot).head();
                let position =
                    editor.window_position_for_display_point(point, &snapshot, window, cx)?;
                let height = editor
                    .style(cx)
                    .text
                    .line_height_in_pixels(window.rem_size());
                let scale = window.scale_factor();
                Some((
                    f32::from(position.y) * scale,
                    (f32::from(position.y) + f32::from(height)) * scale,
                ))
            })
        })
        .ok()
        .flatten()
}

fn initial_state() -> UiAgentState {
    let tool = UiTool {
        id: "generated-tool".to_owned(),
        name: "shell_command".to_owned(),
        arguments: "generated arguments ".repeat(32),
        preview: None,
        status: UiToolStatus::Success,
        output: Some("generated result ".repeat(64)),
        error: None,
        started_at: Some(UnixMs(10)),
        finished_at: Some(UnixMs(20)),
        result_at: None,
        metadata: None,
    };
    UiAgentState {
        blocks: vec![
            Arc::new(UiBlock::UserMessage {
                text: "generated request near a wrapping boundary ".repeat(8),
            }),
            Arc::new(UiBlock::Tool(tool)),
            Arc::new(UiBlock::AssistantMessage {
                text: "generated streaming tail ".repeat(8),
                phase: Some(UiMessagePhase::FinalAnswer),
            }),
        ],
        status: UiAgentStatus::Streaming,
        context_used: None,
        usage: Default::default(),
    }
}

fn init(cx: &mut TestAppContext) {
    cx.update(|cx: &mut App| {
        gpui_tokio::init(cx);
        assets::Assets.load_test_fonts(cx);
        let settings = SettingsStore::new(cx, crate::rho_assets::RHO_DEFAULT_SETTINGS);
        cx.set_global(settings);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        editor::init(cx);
        command_palette::init(cx);
        search::init(cx);
        vim::init(cx);
    });
}

/// Verifies that a seed is byte-stable at the scene-hash boundary.
pub fn deterministic(config: WalkConfig) -> Result<WalkReport, WalkFailure> {
    let first = run(config)?;
    let second = run(config)?;
    if first.scene_hashes != second.scene_hashes {
        return Err(WalkFailure {
            oracle: "same seed produced different scenes",
            step: config.steps.saturating_sub(1),
            event: generate(config.seed, config.steps).last().cloned(),
            events: generate(config.seed, config.steps),
            draw_micros: first.max_draw_micros,
            editor_rows: first.max_editor_rows,
            cold_draw_micros: first.cold_draw_micros,
            warm_draw_micros: first.warm_draw_micros,
            scene_details: Vec::new(),
        });
    }
    if first.frames != config.steps {
        return Err(WalkFailure {
            oracle: "frame count",
            step: config.steps.saturating_sub(1),
            event: generate(config.seed, config.steps).last().cloned(),
            events: generate(config.seed, config.steps),
            draw_micros: first.max_draw_micros,
            editor_rows: first.max_editor_rows,
            cold_draw_micros: first.cold_draw_micros,
            warm_draw_micros: first.warm_draw_micros,
            scene_details: Vec::new(),
        });
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_scene_sequence_is_stable() {
        let report = deterministic(WalkConfig {
            seed: 0,
            steps: 8,
            mode: WalkMode::Debug,
        })
        .expect("generated scene sequence");
        assert_eq!(report.frames, 8);
        assert_eq!(report.scene_hashes.len(), 8);
    }

    #[test]
    fn detached_host_retry_is_the_declared_live_status_owner() {
        run_events_with_detached_host(
            1,
            WalkMode::Debug,
            &[
                WalkEvent::Resize {
                    width: 480,
                    height: 600,
                },
                WalkEvent::Resize {
                    width: 708,
                    height: 600,
                },
                WalkEvent::AdvanceTime { milliseconds: 812 },
                WalkEvent::AdvanceTime { milliseconds: 671 },
                WalkEvent::AdvanceTime { milliseconds: 397 },
                WalkEvent::AdvanceTime { milliseconds: 126 },
            ],
            true,
        )
        .expect("only the connection label's declared cadence changes");
    }
}
