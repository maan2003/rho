//! A live jj diff rendered with Zed's editor primitives.
//!
//! jj owns the persistent repository snapshot and immutable parent text. Zed
//! owns current-side buffers, including unsaved edits and external-file
//! conflict state. Every split observes one [`DiffModel`], so manifest
//! refreshes reconcile the existing surface instead of replacing editors.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use buffer_diff::{BufferDiff, BufferDiffEvent};
use camino::{Utf8Path, Utf8PathBuf};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement, Render, Styled, Subscription, Task, Window, div, px,
};
use language::{Buffer, BufferEvent, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use rho_ui_proto::{
    WorkspaceDiffContent, WorkspaceDiffSnapshot, WorkspaceDiffTarget, WorkspaceInfo,
};
use text::OffsetRangeExt as _;
use theme::ActiveTheme as _;
use util::rel_path::RelPath;

use crate::connection::DiffClient;
use crate::zed_remote::RemoteProject;

const MAX_LIVE_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_LIVE_BYTES: usize = 48 * 1024 * 1024;

/// A manifest whose text buffers and initial diffs have been loaded. This is
/// prepared asynchronously before it is applied to the shared model.
pub struct PreparedDiff {
    snapshot: WorkspaceDiffSnapshot,
    entries: Vec<PreparedEntry>,
    omitted: usize,
    metadata_only: Vec<String>,
    live_paths: Vec<Utf8PathBuf>,
}

struct PreparedEntry {
    path: Utf8PathBuf,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
}

impl PreparedDiff {
    pub async fn load(
        remote: &RemoteProject,
        snapshot: WorkspaceDiffSnapshot,
        live_paths: Vec<Utf8PathBuf>,
        cx: &mut AsyncApp,
    ) -> Result<Self> {
        let mut entries = Vec::new();
        let mut omitted = 0;
        let mut metadata_only = Vec::new();
        let mut live_budget = MAX_LIVE_BYTES;

        for file in &snapshot.files {
            let base_text = match &file.base {
                WorkspaceDiffContent::Text(text) => Some(Arc::<str>::from(text.as_str())),
                WorkspaceDiffContent::Absent => None,
                _ => {
                    omitted += 1;
                    continue;
                }
            };
            if !matches!(
                file.target,
                WorkspaceDiffTarget::Text { .. } | WorkspaceDiffTarget::Absent
            ) {
                omitted += 1;
                continue;
            }

            // Opening an absent path can still recover an already-open dirty
            // buffer whose file was deleted externally. Only fall back to an
            // empty read-only buffer when no live project buffer exists.
            let buffer = match file.target {
                WorkspaceDiffTarget::Text { bytes } => {
                    let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
                    if bytes > MAX_LIVE_FILE_BYTES || bytes > live_budget {
                        omitted += 1;
                        continue;
                    }
                    match crate::zed_remote::open_file_buffer(remote, file.path.clone(), cx).await {
                        Ok(buffer) => buffer,
                        Err(error) => {
                            tracing::warn!(path = %file.path, %error, "open live diff buffer");
                            omitted += 1;
                            continue;
                        }
                    }
                }
                WorkspaceDiffTarget::Absent => {
                    match crate::zed_remote::opened_dirty_file_buffer(remote, file.path.clone(), cx)
                        .await
                        .with_context(|| format!("find deleted file buffer {}", file.path))?
                    {
                        Some(buffer) => buffer,
                        None => cx.update(|cx| {
                            cx.new(|cx| {
                                let mut buffer = Buffer::local("", cx);
                                buffer.set_capability(Capability::ReadOnly, cx);
                                buffer
                            })
                        }),
                    }
                }
                _ => unreachable!("non-file targets were filtered above"),
            };
            let live_bytes = buffer.read_with(cx, |buffer, _| buffer.len());
            if live_bytes > MAX_LIVE_FILE_BYTES || live_bytes > live_budget {
                omitted += 1;
                continue;
            }
            live_budget -= live_bytes;

            let (diff, update) = cx.update(|cx| {
                let buffer_snapshot = buffer.read(cx).snapshot();
                let language = buffer_snapshot.language().cloned();
                let language_registry = buffer.read(cx).language_registry();
                let diff = cx.new(|cx| {
                    BufferDiff::new(&buffer_snapshot.text, language, language_registry, cx)
                });
                let update = diff.update(cx, |diff, cx| {
                    diff.set_base_text(base_text, buffer_snapshot.text.clone(), cx)
                });
                (diff, update)
            });
            update.await;

            let has_hunks = diff.read_with(cx, |diff, cx| {
                let snapshot = buffer.read(cx).snapshot();
                diff.snapshot(cx).hunks(&snapshot).next().is_some()
            });
            if !has_hunks && file.base_executable != file.target_executable {
                metadata_only.push(format!(
                    "{}  executable {} → {}",
                    file.path,
                    mode_label(file.base_executable),
                    mode_label(file.target_executable)
                ));
            }
            entries.push(PreparedEntry {
                path: file.path.clone(),
                buffer,
                diff,
            });
        }

        Ok(Self {
            snapshot,
            entries,
            omitted,
            metadata_only,
            live_paths,
        })
    }
}

struct DiffEntry {
    path_key: PathKey,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    _subscriptions: Vec<Subscription>,
    recalculate: Option<Task<()>>,
}

/// Shared state for every pane displaying the same workspace diff.
pub struct DiffModel {
    remote: RemoteProject,
    client: DiffClient,
    workspace: WorkspaceInfo,
    multibuffer: Entity<MultiBuffer>,
    entries: HashMap<Utf8PathBuf, DiffEntry>,
    commit_id: String,
    live_paths: Vec<Utf8PathBuf>,
    status: String,
    visible: bool,
    stale: bool,
    refreshing: bool,
    refresh_again: bool,
    refresh_generation: u64,
    refresh_task: Option<Task<()>>,
    debounce_task: Option<Task<()>>,
    _project_subscription: Subscription,
}

impl DiffModel {
    pub fn new(
        remote: RemoteProject,
        client: DiffClient,
        workspace: WorkspaceInfo,
        prepared: PreparedDiff,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let project_subscription =
            cx.subscribe(&remote.project, |this, _, event, cx| match event {
                project::Event::WorktreeUpdatedEntries(_, _) => this.schedule_refresh(cx),
                project::Event::BufferEdited { .. }
                    if dirty_paths(&this.remote, cx)
                        .iter()
                        .any(|path| !this.entries.contains_key(path)) =>
                {
                    this.schedule_refresh(cx)
                }
                _ => {}
            });
        let mut model = Self {
            remote,
            client,
            workspace,
            multibuffer,
            entries: HashMap::new(),
            commit_id: String::new(),
            live_paths: Vec::new(),
            status: String::new(),
            visible: false,
            stale: false,
            refreshing: false,
            refresh_again: false,
            refresh_generation: 0,
            refresh_task: None,
            debounce_task: None,
            _project_subscription: project_subscription,
        };
        model.apply(prepared, cx);
        model
    }

    pub fn multibuffer(&self) -> Entity<MultiBuffer> {
        self.multibuffer.clone()
    }

    pub fn project(&self) -> Entity<project::Project> {
        self.remote.project.clone()
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    /// A semantic barrier: persist a fresh jj snapshot of this workspace and
    /// its descendants, returning only a new manifest revision.
    pub fn refresh_now(&mut self, cx: &mut Context<Self>) {
        self.stale = true;
        self.refresh_generation = self.refresh_generation.wrapping_add(1);
        if self.visible {
            self.start_refresh(cx);
        }
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            if self.stale {
                self.start_refresh(cx);
            }
        } else {
            self.refresh_generation = self.refresh_generation.wrapping_add(1);
            self.debounce_task = None;
        }
    }

    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        self.stale = true;
        if !self.visible {
            return;
        }
        self.refresh_generation = self.refresh_generation.wrapping_add(1);
        let generation = self.refresh_generation;
        self.debounce_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.refresh_generation == generation {
                    this.start_refresh(cx);
                }
            });
        }));
    }

    fn start_refresh(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        if self.refreshing {
            self.refresh_again = true;
            return;
        }
        self.stale = false;
        self.refreshing = true;
        self.refresh_again = false;
        let live_paths = dirty_paths(&self.remote, cx);
        let has_missing_live_path = live_paths
            .iter()
            .any(|path| !self.entries.contains_key(path));
        let known_commit_id = (live_paths == self.live_paths && !has_missing_live_path)
            .then(|| self.commit_id.clone());
        let request = self.client.snapshot(
            self.workspace.clone(),
            known_commit_id,
            live_paths.clone(),
            cx,
        );
        let remote = self.remote.clone();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result: Result<Option<PreparedDiff>> = async {
                let snapshot = request.await.context("diff refresh task failed")??;
                match snapshot {
                    Some(snapshot) => Ok(Some(
                        PreparedDiff::load(&remote, snapshot, live_paths, cx).await?,
                    )),
                    None => Ok(None),
                }
            }
            .await;

            let _ = this.update(cx, |this, cx| {
                this.refreshing = false;
                match result {
                    Ok(Some(prepared)) => this.apply(prepared, cx),
                    Ok(None) => {}
                    Err(error) => {
                        let base = this
                            .status
                            .split(" · refresh failed:")
                            .next()
                            .unwrap_or(&this.status);
                        this.status = format!("{base} · refresh failed: {error:#}");
                        cx.notify();
                    }
                }
                if this.refresh_again {
                    this.start_refresh(cx);
                }
            });
        }));
        cx.notify();
    }

    fn apply(&mut self, prepared: PreparedDiff, cx: &mut Context<Self>) {
        let status = status_text(&prepared);
        let commit_id = prepared.snapshot.commit_id.clone();
        let live_paths = prepared.live_paths.clone();
        let next_paths = prepared
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<HashSet<_>>();
        let removed = self
            .entries
            .keys()
            .filter(|path| !next_paths.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        for path in removed {
            if self
                .entries
                .get(&path)
                .is_some_and(|entry| entry.buffer.read(cx).is_dirty())
            {
                // The request raced a new local edit. Keep its existing base
                // and let the already-scheduled refresh union the path.
                continue;
            }
            if let Some(entry) = self.entries.remove(&path) {
                self.multibuffer.update(cx, |multibuffer, cx| {
                    multibuffer.remove_excerpts(entry.path_key, cx)
                });
            }
        }

        for prepared_entry in prepared.entries {
            let path = prepared_entry.path;
            let path_key = diff_path_key(&path);
            let buffer = prepared_entry.buffer;
            let diff = prepared_entry.diff;
            let buffer_subscription = cx.subscribe(&buffer, {
                let path = path.clone();
                move |this, _, event, cx| match event {
                    BufferEvent::Edited { .. } => this.recalculate(&path, cx),
                    BufferEvent::Saved
                    | BufferEvent::Reloaded
                    | BufferEvent::FileHandleChanged
                    | BufferEvent::ReloadNeeded
                    | BufferEvent::DirtyChanged => {
                        this.recalculate(&path, cx);
                        this.schedule_refresh(cx);
                    }
                    _ => {}
                }
            });
            let diff_subscription = cx.subscribe(&diff, {
                let path = path.clone();
                move |this, _, event, cx| {
                    if matches!(event, BufferDiffEvent::DiffChanged(_)) {
                        this.update_excerpts(&path, cx);
                    }
                }
            });
            self.entries.insert(
                path.clone(),
                DiffEntry {
                    path_key,
                    buffer,
                    diff,
                    _subscriptions: vec![buffer_subscription, diff_subscription],
                    recalculate: None,
                },
            );
            self.update_excerpts(&path, cx);
            // Loading the manifest and its remote buffers is asynchronous;
            // always recalculate after subscribing so edits during preparation
            // cannot install stale hunks.
            self.recalculate(&path, cx);
        }

        self.commit_id = commit_id;
        self.live_paths = live_paths;
        self.status = status;
        if dirty_paths(&self.remote, cx) != self.live_paths {
            // The dirty set can change while the manifest and remote buffers
            // are loading. Reconcile again rather than leaving an obsolete
            // unsaved-only header installed until the next user event.
            self.schedule_refresh(cx);
        }
        cx.notify();
    }

    fn recalculate(&mut self, path: &Utf8Path, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(path) else {
            return;
        };
        let buffer = entry.buffer.clone();
        let diff = entry.diff.clone();
        let buffer_snapshot = buffer.read(cx).snapshot();
        let (base_snapshot, base_text) = {
            let diff = diff.read(cx);
            let base_snapshot = diff.base_text(cx);
            let base_text = diff
                .base_text_exists()
                .then(|| Arc::from(base_snapshot.text()));
            (base_snapshot, base_text)
        };
        let update =
            diff.read(cx)
                .update_diff(buffer_snapshot.text.clone(), &base_snapshot, base_text, cx);
        let task = cx.spawn(async move |_, cx| {
            let update = update.await;
            diff.update(cx, |diff, cx| {
                diff.set_snapshot(update, cx);
            });
        });
        if let Some(entry) = self.entries.get_mut(path) {
            // Dropping the previous task cancels a now-stale calculation.
            entry.recalculate = Some(task);
        }
    }

    fn update_excerpts(&mut self, path: &Utf8Path, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(path) else {
            return;
        };
        let path_key = entry.path_key.clone();
        let buffer = entry.buffer.clone();
        let diff = entry.diff.clone();
        let buffer_snapshot = buffer.read(cx).snapshot();
        let mut ranges = diff
            .read(cx)
            .snapshot(cx)
            .hunks(&buffer_snapshot)
            .map(|hunk| hunk.buffer_range.to_point(&buffer_snapshot))
            .collect::<Vec<_>>();
        if ranges.is_empty() {
            // Keep a stable file header for executable-only changes and for
            // live buffers temporarily edited back to their base.
            ranges.push(Point::zero()..Point::zero());
        }
        let context_lines = editor::multibuffer_context_lines(cx);
        self.multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.update_excerpts_for_path(path_key, buffer, ranges, context_lines, cx);
            multibuffer.add_diff(diff, cx);
        });
    }
}

/// One pane's cursor, scroll, and folds over a shared [`DiffModel`].
pub struct DiffView {
    editor: Entity<editor::Editor>,
    model: Entity<DiffModel>,
    _subscriptions: Vec<Subscription>,
}

impl DiffView {
    pub fn new(model: Entity<DiffModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (multibuffer, project) = {
            let model = model.read(cx);
            (model.multibuffer(), model.project())
        };
        let editor = build_editor(multibuffer, project, window, cx);
        let model_changed = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            editor,
            model,
            _subscriptions: vec![model_changed],
        }
    }

    pub fn model(&self) -> Entity<DiffModel> {
        self.model.clone()
    }

    pub fn editor(&self) -> &Entity<editor::Editor> {
        &self.editor
    }

    fn save(&mut self, _: &crate::FileSave, window: &mut Window, cx: &mut Context<Self>) {
        let buffers = self.editor.read(cx).buffer().read(cx).all_buffers();
        crate::zed_remote::save_buffers(self.model.read(cx).project(), buffers, window, cx);
    }
}

/// Sorted repository paths for every dirty buffer already owned by the shared
/// Zed project. These paths are unioned into the jj manifest so unsaved-only
/// edits appear and survive reconciliation.
pub fn dirty_paths(remote: &RemoteProject, cx: &App) -> Vec<Utf8PathBuf> {
    let mut paths = remote
        .project
        .read(cx)
        .opened_buffers(cx)
        .into_iter()
        .filter_map(|buffer| {
            let buffer = buffer.read(cx);
            if !buffer.is_dirty() {
                return None;
            }
            buffer
                .file()
                .map(|file| Utf8PathBuf::from(file.path().as_unix_str()))
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

impl Render for DiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        div()
            .key_context("RhoDiffView")
            .on_action(cx.listener(Self::save))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.editor_background)
            .child(
                div()
                    .flex_none()
                    .px(px(10.))
                    .py(px(4.))
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .text_color(colors.text_muted)
                    .child(self.model.read(cx).status().to_owned()),
            )
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}

fn build_editor(
    multibuffer: Entity<MultiBuffer>,
    project: Entity<project::Project>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<editor::Editor> {
    cx.new(|cx| {
        let mut editor = editor::Editor::for_multibuffer(multibuffer, Some(project), window, cx);
        crate::editor_config::configure_file(&mut editor, window, cx);
        editor.start_temporary_diff_override();
        editor.set_expand_all_diff_hunks(cx);
        editor.set_render_diff_hunks_as_unstaged(true, cx);
        editor.set_render_diff_hunk_controls(
            Arc::new(|_, _, _, _, _, _, _, _| gpui::Empty.into_any_element()),
            cx,
        );
        editor
    })
}

fn diff_path_key(path: &Utf8Path) -> PathKey {
    let path = RelPath::unix(path.as_str())
        .expect("jj repository paths are valid relative paths")
        .into_arc();
    // All entries share a prefix so lexical repository path determines order;
    // unlike a manifest index, this key remains stable across refreshes.
    PathKey::with_sort_prefix(0, path)
}

fn status_text(prepared: &PreparedDiff) -> String {
    let short = &prepared.snapshot.commit_id[..prepared.snapshot.commit_id.len().min(12)];
    let mut parts = vec![format!(
        "{} changed files · jj {short}",
        prepared.snapshot.files.len()
    )];
    if prepared.omitted != 0 {
        parts.push(format!("{} non-text/oversized omitted", prepared.omitted));
    }
    if prepared.snapshot.truncated {
        parts.push("snapshot truncated".to_owned());
    }
    if !prepared.metadata_only.is_empty() {
        parts.push(prepared.metadata_only.join(", "));
    }
    parts.join(" · ")
}

fn mode_label(mode: Option<bool>) -> &'static str {
    match mode {
        Some(true) => "yes",
        Some(false) => "no",
        None => "absent",
    }
}
