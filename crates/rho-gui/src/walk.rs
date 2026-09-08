//! Deterministic, in-process GUI random walks over GPUI's real workspace scene.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use editor::Editor;
use gpui::{App, Entity, TestAppContext, TestDispatcher, WindowHandle, point, px, size};
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
    ComposerKey {
        character: char,
    },
    Resize {
        width: u16,
        height: u16,
    },
    AgentChunk {
        bytes: u16,
    },
    ToolBody {
        bytes: u16,
    },
    AdvanceTime {
        milliseconds: u16,
    },
    /// What `gg` does: put the reader at the top of the transcript.
    ScrollToTop,
    Idle,
}

/// What a seeded turn is made of.
///
/// The three shapes ask different questions of the same window. `Prose` is
/// settled but not elided. `Tools` puts working output in every turn, which
/// the elision policy folds, so the document carries one fold per turn.
/// `ShortTurns` makes each turn a line of question and a line of answer,
/// which is the ordinary document and the one that composes the most
/// buffers: a buffer is a run of blocks with the same markdown flag, an
/// answer is markdown and a question is not, so a transcript of short turns
/// composes one buffer per block where a transcript of long ones composes
/// one per four rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prefill {
    Prose,
    Tools,
    ShortTurns,
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
    /// Settled turns to seed the transcript with before anything is driven.
    ///
    /// Generated walks leave this at zero and grow their document from the
    /// events themselves, which never takes it past about fifty tab rows -
    /// the tool bodies that would take it further are concealed before the
    /// tab map sees them. A drive that means to reach the whole-buffer
    /// rewrap, or to ask what one keystroke costs on a document worth
    /// scrolling, has to start with the document already there.
    pub prefill_turns: usize,
    /// What each seeded turn is made of.
    pub prefill: Prefill,
    /// A fixed drive, run in place of a generated sequence.
    ///
    /// A generator cannot be asked for a particular shape of run. The two
    /// events that make the wrap map do whole-document work - a width
    /// change and a jump to the top - have to be named in order.
    pub script: Option<&'static [WalkEvent]>,
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

/// How many times the settled frame is drawn without the recorder before
/// the run starts. The first is the window's only cold draw; the rest are
/// what the bound is read off.
const DRAW_SAMPLES: usize = 5;

/// Numeric evidence retained from a successful run.
#[derive(Clone, Debug, PartialEq)]
pub struct WalkReport {
    pub seed: u64,
    pub steps: usize,
    pub frames: usize,
    pub distinct_scenes: usize,
    pub max_changed_primitives: usize,
    pub max_touched_rows: u64,
    pub max_walked_items: u64,
    pub max_drawn_rows: u64,
    pub max_draw_micros: u64,
    pub cold_draw_micros: u64,
    pub warm_draw_micros: u64,
    /// Every recorder-free draw of the settled frame, in the order taken.
    ///
    /// One draw is a measurement of the machine as much as of the frame: a
    /// build on the same host took the same frame from 1290 us to 5922 at
    /// load 71 on 96 cores. The frame's own cost is the smallest of several
    /// draws of it - a minimum under load is still work the frame did, a
    /// maximum is the scheduler - so the bound is read off the best and the
    /// whole spread is printed, which is what makes a loaded machine
    /// visible rather than mistaken for a regression.
    pub draw_samples: Vec<u64>,
    pub step_draw_micros: Vec<u64>,
    pub step_touched_rows: Vec<u64>,
    pub step_walked_items: Vec<u64>,
    /// What each step's walk was spent on, by stage, largest first, for the
    /// stages that walked anything. A step's `walked_items` is the sum of
    /// every stage's, so a step that walks a document says nothing about
    /// which layer walked it until this is read.
    pub step_stage_walks: Vec<Vec<(&'static str, u64)>>,
    pub step_drawn_rows: Vec<u64>,
    pub step_total_rows: Vec<u64>,
    pub scene_hashes: Vec<u64>,
    pub wall_clock_findings: Vec<WallClockFinding>,
    pub baseline_owners: Vec<OwnerFrameSummary>,
    pub step_owners: Vec<Vec<OwnerFrameSummary>>,
}

/// A profiling-only observation. It is reported by the gate but is not a
/// deterministic landing failure until the known slow cases are repaired.
///
/// Its `draw_micros` is measured with the scene recorder attached, so it
/// carries what recording a primitive costs and is not the reader's frame.
/// Read it against the other steps of the same run, not against a bound;
/// the bound is asked of the recorder-free cold and warm draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WallClockFinding {
    pub step: usize,
    pub event: WalkEvent,
    pub draw_micros: u64,
    pub touched_rows: u64,
    pub walked_items: u64,
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
    pub touched_rows: u64,
    pub walked_items: u64,
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

type WalkRejection = (
    &'static str,
    usize,
    Option<WalkEvent>,
    u64,
    u64,
    u64,
    u64,
    u64,
    Vec<String>,
);

/// Runs one generated sequence and deletion-shrinks an oracle failure.
pub fn run(config: WalkConfig) -> Result<WalkReport, WalkFailure> {
    let events = events_for(config);
    match run_events(config, &events) {
        Ok(report) => Ok(report),
        Err(rejection) => {
            let oracle = rejection.0;
            let mut final_rejection = rejection;
            let mut shrunk = events;
            let mut index = 0;
            while index < shrunk.len() {
                let mut candidate = shrunk.clone();
                candidate.remove(index);
                match run_events(config, &candidate) {
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
                        if let Err(rejection) = run_events(config, &candidate)
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
                touched_rows,
                walked_items,
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
                touched_rows,
                walked_items,
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

/// The drive a config asks for: its own script, or its generated sequence.
fn events_for(config: WalkConfig) -> Vec<WalkEvent> {
    match config.script {
        Some(script) => script.to_vec(),
        None => generate(config.seed, config.steps),
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

fn run_events(config: WalkConfig, events: &[WalkEvent]) -> Result<WalkReport, WalkRejection> {
    run_events_with_detached_host(config, events, false)
}

fn run_events_with_detached_host(
    config: WalkConfig,
    events: &[WalkEvent],
    detached_host: bool,
) -> Result<WalkReport, WalkRejection> {
    let WalkConfig {
        seed,
        mode,
        prefill_turns,
        prefill,
        ..
    } = config;
    gpui::profiler::set_editor_trace_enabled(true);
    gpui::profiler::set_frame_trace_enabled(true);
    let mut timings = gpui::profiler::EditorTimingCollector::new();
    let mut frame_timings = gpui::profiler::FrameTimingCollector::new();
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
    let mut state = initial_state(prefill_turns, prefill);
    workspace
        .update(&mut cx, |workspace, window, cx| {
            workspace.select_agent(Some(agent), window, cx);
            workspace.seed_transcript_for_test(agent, state.clone(), window, cx);
        })
        .map_err(|_| ("workspace closed", 0, None, 0, 0, 0, 0, 0, Vec::new()))?;
    cx.run_until_parked();
    let editor = active_editor(&workspace, &mut cx)
        .map_err(|_| ("workspace closed", 0, None, 0, 0, 0, 0, 0, Vec::new()))?;
    let mut draw_samples = Vec::with_capacity(DRAW_SAMPLES);
    for _ in 0..DRAW_SAMPLES {
        let started = Instant::now();
        cx.draw_window(*workspace);
        frame_timings.collect_unseen();
        draw_samples.push(started.elapsed().as_micros() as u64);
    }
    let cold_draw_micros = draw_samples[0];
    let warm_draw_micros = draw_samples[1..]
        .iter()
        .copied()
        .min()
        .unwrap_or(cold_draw_micros);
    let recorder = cx.record_scenes::<WalkEvent>(*workspace);
    cx.draw_window(*workspace);
    frame_timings.collect_unseen();
    timings.collect_unseen();

    let mut max_changed_primitives = 0;
    let mut max_touched_rows = 0;
    let mut max_walked_items = 0;
    let mut max_drawn_rows = 0;
    let mut max_draw_micros = 0;
    let mut step_draw_micros = Vec::with_capacity(events.len());
    let mut step_touched_rows = Vec::with_capacity(events.len());
    let mut step_walked_items = Vec::with_capacity(events.len());
    let mut step_stage_walks = Vec::with_capacity(events.len());
    let mut step_drawn_rows = Vec::with_capacity(events.len());
    let mut step_total_rows = Vec::with_capacity(events.len());
    let mut work_exceeded = None;
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
                0,
                cold_draw_micros,
                warm_draw_micros,
                Vec::new(),
            )
        })?;
        cx.run_until_parked();
        let draw_started = Instant::now();
        cx.draw_window(*workspace);
        let work = frame_timings
            .collect_unseen()
            .last()
            .map(|timing| timing.work)
            .unwrap_or_default();
        let draw_micros = draw_started.elapsed().as_micros() as u64;
        max_draw_micros = max_draw_micros.max(draw_micros);
        step_draw_micros.push(draw_micros);
        let event_timings = timings.collect_unseen();
        let touched_rows = event_timings
            .iter()
            .map(|timing| timing.touched_rows)
            .max()
            .unwrap_or(0);
        let walked_items = event_timings
            .iter()
            .map(|timing| timing.walked_items)
            .sum::<u64>();
        max_touched_rows = max_touched_rows.max(touched_rows);
        max_walked_items = max_walked_items.max(walked_items);
        step_touched_rows.push(touched_rows);
        step_walked_items.push(walked_items);
        step_stage_walks.push(stage_walks(&event_timings));
        max_drawn_rows = max_drawn_rows.max(work.visible_rows);
        step_drawn_rows.push(work.visible_rows);
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
                touched_rows,
                walked_items,
                sequence: events[..=step].to_vec(),
            });
        }
        if mode == WalkMode::Profiling {
            let expected = editor_work_limit(&event_timings);
            if walked_items > expected && work_exceeded.is_none() {
                work_exceeded =
                    Some((step, event.clone(), draw_micros, touched_rows, walked_items));
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
                0,
                cold_draw_micros,
                warm_draw_micros,
                describe_changes(&recorder, frame),
            ));
        }
    }

    if let Some((step, event, draw_micros, touched_rows, walked_items)) = work_exceeded {
        return Err((
            "editor cursor work exceeded touched rows plus log total",
            step,
            Some(event),
            draw_micros,
            touched_rows,
            walked_items,
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
        max_touched_rows,
        max_walked_items,
        max_drawn_rows,
        max_draw_micros,
        cold_draw_micros,
        warm_draw_micros,
        draw_samples,
        step_draw_micros,
        step_touched_rows,
        step_walked_items,
        step_stage_walks,
        step_drawn_rows,
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

/// One step's walk, split by the stage that did it.
fn stage_walks(timings: &[gpui::profiler::EditorTiming]) -> Vec<(&'static str, u64)> {
    let mut by_stage: Vec<(&'static str, u64)> = Vec::new();
    for timing in timings {
        if timing.walked_items == 0 {
            continue;
        }
        let name = stage_name(timing.kind);
        match by_stage.iter_mut().find(|(stage, _)| *stage == name) {
            Some((_, walked)) => *walked += timing.walked_items,
            None => by_stage.push((name, timing.walked_items)),
        }
    }
    by_stage.sort_by_key(|(_, walked)| std::cmp::Reverse(*walked));
    by_stage
}

fn stage_name(kind: gpui::profiler::EditorTimingKind) -> &'static str {
    use gpui::profiler::EditorTimingKind::*;
    match kind {
        BufferEdit => "buffer",
        MultiBufferSync => "multibuffer",
        FoldMapSync => "fold",
        TabMapSync => "tab",
        WrapMapSync => "wrap",
        BlockMapSync => "block",
        InlayMapSync => "inlay",
        WrapMapUpdate => "wrap_update",
        SyncTree => "tree",
        SpliceInlays => "splice_inlays",
        MultiBufferBufferScan => "buffers",
    }
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

fn editor_work_limit(timings: &[gpui::profiler::EditorTiming]) -> u64 {
    timings
        .iter()
        .map(|timing| {
            let total_rows = timing.old_rows.max(timing.new_rows).max(1);
            let log_total = u64::from(u64::BITS - total_rows.leading_zeros());
            timing
                .touched_rows
                .saturating_add(log_total)
                .saturating_add(2)
                .saturating_mul(2)
        })
        .sum::<u64>()
        .saturating_add(64)
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
            let tool_block = state
                .blocks
                .iter_mut()
                .find(|block| matches!(***block, UiBlock::Tool(_)))
                .context("generated tool block")?;
            let UiBlock::Tool(tool) = Arc::make_mut(tool_block) else {
                unreachable!("found by discriminant")
            };
            tool.output = Some(generated_text(usize::from(*bytes)));
            seed(agent, state, workspace, cx)?;
        }
        WalkEvent::AdvanceTime { milliseconds } => {
            cx.dispatcher
                .advance_clock(std::time::Duration::from_millis(u64::from(*milliseconds)));
        }
        WalkEvent::ScrollToTop => workspace.update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_scroll_position(point(0., 0.), window, cx);
            });
        })?,
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

/// The transcript a walk starts from: one settled turn with a tool, and
/// `prefill_turns` settled turns in front of it.
///
/// The prefilled turns are assistant text with newlines in it rather than
/// tool output, because tool output is concealed and concealed rows are
/// gone before the tab map counts them. Rows that survive to the tab map
/// are the only ones a rewrap has to do.
fn initial_state(prefill_turns: usize, prefill: Prefill) -> UiAgentState {
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
    let mut blocks = Vec::with_capacity(prefill_turns * 3 + 3);
    if prefill == Prefill::ShortTurns {
        for turn in 0..prefill_turns {
            blocks.push(Arc::new(UiBlock::UserMessage {
                text: format!("question {turn}"),
            }));
            // One settled tool, in the oldest turn, so that a body arriving
            // for it is a change under whatever the window has composed:
            // that is what makes the screen open again on its tail.
            if turn == 0 {
                blocks.push(Arc::new(UiBlock::Tool(UiTool {
                    id: "settled-tool-0".to_owned(),
                    ..tool.clone()
                })));
            }
            blocks.push(Arc::new(UiBlock::AssistantMessage {
                text: format!("answer {turn}\n"),
                phase: Some(UiMessagePhase::FinalAnswer),
            }));
        }
        blocks.extend([
            Arc::new(UiBlock::UserMessage {
                text: "generated request near a wrapping boundary ".repeat(8),
            }),
            Arc::new(UiBlock::AssistantMessage {
                text: "generated streaming tail ".repeat(8),
                phase: Some(UiMessagePhase::FinalAnswer),
            }),
        ]);
        return UiAgentState {
            blocks,
            status: UiAgentStatus::Streaming,
            context_used: None,
            usage: Default::default(),
        };
    }
    for turn in 0..prefill_turns {
        blocks.push(Arc::new(UiBlock::UserMessage {
            text: format!("settled question {turn} about a wrapping boundary"),
        }));
        if prefill == Prefill::Tools {
            blocks.push(Arc::new(UiBlock::Tool(UiTool {
                id: format!("settled-tool-{turn}"),
                ..tool.clone()
            })));
        }
        blocks.push(Arc::new(UiBlock::AssistantMessage {
            text: format!(
                "settled answer {turn}: compiled 12 targets in 0.42s\nand a second line of ordinary prose that wraps\nand a third that does not\n"
            ),
            phase: Some(UiMessagePhase::FinalAnswer),
        }));
    }
    blocks.extend([
        Arc::new(UiBlock::UserMessage {
            text: "generated request near a wrapping boundary ".repeat(8),
        }),
        Arc::new(UiBlock::Tool(tool)),
        Arc::new(UiBlock::AssistantMessage {
            text: "generated streaming tail ".repeat(8),
            phase: Some(UiMessagePhase::FinalAnswer),
        }),
    ]);
    UiAgentState {
        blocks,
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
    let steps = events_for(config).len();
    if first.scene_hashes != second.scene_hashes {
        return Err(WalkFailure {
            oracle: "same seed produced different scenes",
            step: steps.saturating_sub(1),
            event: events_for(config).last().cloned(),
            events: events_for(config),
            draw_micros: first.max_draw_micros,
            touched_rows: first.max_touched_rows,
            walked_items: first.max_walked_items,
            cold_draw_micros: first.cold_draw_micros,
            warm_draw_micros: first.warm_draw_micros,
            scene_details: Vec::new(),
        });
    }
    if first.frames != steps {
        return Err(WalkFailure {
            oracle: "frame count",
            step: steps.saturating_sub(1),
            event: events_for(config).last().cloned(),
            events: events_for(config),
            draw_micros: first.max_draw_micros,
            touched_rows: first.max_touched_rows,
            walked_items: first.max_walked_items,
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
            prefill_turns: 0,
            prefill: Prefill::Prose,
            script: None,
        })
        .expect("generated scene sequence");
        assert_eq!(report.frames, 8);
        assert_eq!(report.scene_hashes.len(), 8);
    }

    #[test]
    fn detached_host_retry_is_the_declared_live_status_owner() {
        run_events_with_detached_host(
            WalkConfig {
                seed: 1,
                steps: 6,
                mode: WalkMode::Debug,
                prefill_turns: 0,
                prefill: Prefill::Prose,
                script: None,
            },
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
