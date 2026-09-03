//! Daemon-backed workspace files using rho's bounded file protocol.
//!
//! Buffers and unsaved edits live only in the GUI. The daemon owns disk IO,
//! checked-save revisions, and workspace-scoped filesystem notifications.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use camino::Utf8PathBuf;
use futures::StreamExt as _;
use futures::channel::mpsc::Sender;
use futures::channel::oneshot;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, PromptLevel, Render, Styled as _, Subscription, Task, WeakEntity, Window,
    div,
};
use language::{Buffer, BufferEvent, Capability};
use rho_ui_proto::{
    FileReadResult, FileSaveResult, WorkspaceClientFrame, WorkspaceInfo, WorkspaceServerFrame,
};
use theme::{ActiveTheme as _, GlobalTheme};

use crate::connection::{Connection, WorkspaceChannel};

#[derive(Clone, Copy, Debug)]
pub enum RemoteProjectEvent {
    FilesChanged,
    BufferEdited,
}

pub struct RemoteProjectState {
    outgoing: Sender<WorkspaceClientFrame>,
    next_request_id: u64,
    /// Monotonically advances for each daemon filesystem invalidation. Diff
    /// preparation samples this so a watcher event that arrives before its
    /// model subscribes cannot be lost.
    change_epoch: u64,
    pending: HashMap<u64, Pending>,
    saving: std::collections::HashSet<Utf8PathBuf>,
    buffers: HashMap<Utf8PathBuf, OpenBuffer>,
    languages: Arc<language::LanguageRegistry>,
    _incoming: Task<()>,
    _transport: rho_rpc::ChannelTask,
}

struct OpenBuffer {
    buffer: WeakEntity<Buffer>,
    revision: Vec<u8>,
    utf8_bom: bool,
    deleted: bool,
    reload_generation: u64,
    _subscription: Subscription,
}

enum Pending {
    Read(oneshot::Sender<FileReadResult>),
    Save {
        path: Utf8PathBuf,
        tx: oneshot::Sender<FileSaveResult>,
    },
}

impl gpui::EventEmitter<RemoteProjectEvent> for RemoteProjectState {}

impl RemoteProjectState {
    pub fn change_epoch(&self) -> u64 {
        self.change_epoch
    }

    pub fn opened_buffers(&self, _cx: &App) -> Vec<(Utf8PathBuf, Entity<Buffer>)> {
        self.buffers
            .iter()
            .filter_map(|(path, entry)| entry.buffer.upgrade().map(|buffer| (path.clone(), buffer)))
            .collect()
    }

    fn existing_buffer(&self, path: &Utf8PathBuf) -> Option<Entity<Buffer>> {
        self.buffers.get(path)?.buffer.upgrade()
    }

    fn path_for_buffer(&self, needle: &Entity<Buffer>) -> Option<Utf8PathBuf> {
        self.buffers.iter().find_map(|(path, entry)| {
            entry
                .buffer
                .upgrade()
                .filter(|buffer| buffer == needle)
                .map(|_| path.clone())
        })
    }

    fn next_request(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        id
    }

    fn read(&mut self, path: Utf8PathBuf, reload: bool) -> oneshot::Receiver<FileReadResult> {
        let request_id = self.next_request();
        let (tx, rx) = oneshot::channel();
        self.pending.insert(request_id, Pending::Read(tx));
        let frame = if reload {
            WorkspaceClientFrame::Reload { request_id, path }
        } else {
            WorkspaceClientFrame::Open { request_id, path }
        };
        if self.outgoing.try_send(frame).is_err()
            && let Some(Pending::Read(tx)) = self.pending.remove(&request_id)
        {
            let _ = tx.send(FileReadResult::Error("workspace channel closed".into()));
        }
        rx
    }

    fn begin_reload(&mut self, path: Utf8PathBuf) -> (u64, oneshot::Receiver<FileReadResult>) {
        let generation = if let Some(entry) = self.buffers.get_mut(&path) {
            entry.reload_generation = entry.reload_generation.wrapping_add(1);
            entry.reload_generation
        } else {
            0
        };
        (generation, self.read(path, true))
    }

    fn save(
        &mut self,
        path: Utf8PathBuf,
        revision: Vec<u8>,
        contents: Vec<u8>,
        overwrite: bool,
    ) -> oneshot::Receiver<FileSaveResult> {
        let request_id = self.next_request();
        let (tx, rx) = oneshot::channel();
        if !self.saving.insert(path.clone()) {
            let _ = tx.send(FileSaveResult::Error("save already in progress".into()));
            return rx;
        }
        self.pending.insert(
            request_id,
            Pending::Save {
                path: path.clone(),
                tx,
            },
        );
        let frame = if overwrite {
            WorkspaceClientFrame::Overwrite {
                request_id,
                path,
                contents,
            }
        } else {
            WorkspaceClientFrame::Save {
                request_id,
                path,
                revision,
                contents,
            }
        };
        if self.outgoing.try_send(frame).is_err()
            && let Some(Pending::Save { path, tx }) = self.pending.remove(&request_id)
        {
            self.saving.remove(&path);
            let _ = tx.send(FileSaveResult::Error("workspace channel closed".into()));
        }
        rx
    }

    fn handle_frame(&mut self, frame: WorkspaceServerFrame, cx: &mut Context<Self>) {
        match frame {
            WorkspaceServerFrame::Opened {
                request_id, result, ..
            }
            | WorkspaceServerFrame::Reloaded {
                request_id, result, ..
            } => {
                if let Some(Pending::Read(tx)) = self.pending.remove(&request_id) {
                    let _ = tx.send(result);
                }
            }
            WorkspaceServerFrame::Saved {
                request_id, result, ..
            } => {
                if let Some(Pending::Save { path, tx }) = self.pending.remove(&request_id) {
                    self.saving.remove(&path);
                    let _ = tx.send(result);
                }
            }
            WorkspaceServerFrame::Changed { paths, rescan } => {
                self.change_epoch = self.change_epoch.wrapping_add(1);
                cx.emit(RemoteProjectEvent::FilesChanged);
                let paths = if rescan {
                    self.buffers.keys().cloned().collect::<Vec<_>>()
                } else {
                    paths
                };
                let paths = paths
                    .into_iter()
                    .filter(|path| self.existing_buffer(path).is_some())
                    .collect::<Vec<_>>();
                let this = cx.entity().downgrade();
                cx.spawn(async move |_, cx| reload_changed(this, paths, cx).await)
                    .detach();
            }
        }
    }

    fn disconnected(&mut self, cx: &mut Context<Self>) {
        self.outgoing.close_channel();
        for (_, pending) in self.pending.drain() {
            match pending {
                Pending::Read(tx) => {
                    let _ = tx.send(FileReadResult::Error("workspace channel closed".into()));
                }
                Pending::Save { tx, .. } => {
                    let _ = tx.send(FileSaveResult::Error("workspace channel closed".into()));
                }
            }
        }
        for entry in self.buffers.values() {
            if let Some(buffer) = entry.buffer.upgrade() {
                buffer.update(cx, |buffer, cx| {
                    buffer.set_capability(Capability::ReadOnly, cx)
                });
            }
        }
        cx.emit(RemoteProjectEvent::FilesChanged);
    }

    fn install_buffer(
        &mut self,
        path: Utf8PathBuf,
        buffer: Entity<Buffer>,
        revision: Vec<u8>,
        utf8_bom: bool,
        deleted: bool,
        cx: &mut Context<Self>,
    ) {
        let subscription = cx.subscribe(&buffer, |_, _, event, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                cx.emit(RemoteProjectEvent::BufferEdited);
            }
        });
        self.buffers.insert(
            path,
            OpenBuffer {
                buffer: buffer.downgrade(),
                revision,
                utf8_bom,
                deleted,
                reload_generation: 0,
                _subscription: subscription,
            },
        );
    }
}

#[derive(Clone)]
pub struct RemoteProject {
    pub state: Entity<RemoteProjectState>,
}

pub fn open_remote_project(
    connection: &Connection,
    workspace: WorkspaceInfo,
    cx: &mut App,
) -> Task<Result<RemoteProject>> {
    let channel_task = connection.open_channel(workspace, cx);
    cx.spawn(async move |cx| {
        let WorkspaceChannel {
            outgoing,
            mut incoming,
            transport,
        } = channel_task
            .await
            .context("workspace channel dial failed")?;
        let languages = cx.update(language_registry);
        let state = cx.update(|cx| {
            cx.new(|_| RemoteProjectState {
                outgoing,
                next_request_id: 1,
                change_epoch: 0,
                pending: HashMap::new(),
                saving: std::collections::HashSet::new(),
                buffers: HashMap::new(),
                languages,
                _incoming: Task::ready(()),
                _transport: transport,
            })
        });
        let weak = state.downgrade();
        let task = cx.spawn(async move |cx| {
            while let Some(frame) = incoming.next().await {
                let Ok(frame) = frame else { break };
                if weak
                    .update(cx, |state, cx| state.handle_frame(frame, cx))
                    .is_err()
                {
                    break;
                }
            }
            let _ = weak.update(cx, |state, cx| state.disconnected(cx));
        });
        state.update(cx, |state, _| state._incoming = task);
        Ok(RemoteProject { state })
    })
}

pub async fn open_file_buffer(
    remote: &RemoteProject,
    path: Utf8PathBuf,
    cx: &mut AsyncApp,
) -> Result<Entity<Buffer>> {
    let path = normalized_path(path)?;
    if let Some(buffer) = cx.update(|cx| remote.state.read(cx).existing_buffer(&path)) {
        return Ok(buffer);
    }
    let (read_epoch, response) = cx.update(|cx| {
        let read_epoch = remote.state.read(cx).change_epoch();
        let response = remote
            .state
            .update(cx, |state, _| state.read(path.clone(), false));
        (read_epoch, response)
    });
    let response = response.await.context("workspace channel closed")?;
    let (text, revision, utf8_bom, deleted) = match response {
        FileReadResult::File { contents, revision } => {
            let (text, utf8_bom) = decode_utf8(contents, &path)?;
            (text, revision, utf8_bom, false)
        }
        FileReadResult::Deleted => (String::new(), Vec::new(), false, true),
        FileReadResult::Error(error) => return Err(anyhow!(error)),
    };

    let languages = cx.update(|cx| remote.state.read(cx).languages.clone());
    let buffer = cx.update(|cx| {
        remote.state.update(cx, |state, cx| {
            if let Some(buffer) = state.existing_buffer(&path) {
                return buffer;
            }
            let buffer = cx.new(|cx| {
                let buffer = Buffer::local(text, cx);
                buffer.set_language_registry(languages.clone());
                buffer
            });
            state.install_buffer(
                path.clone(),
                buffer.clone(),
                revision,
                utf8_bom,
                deleted,
                cx,
            );
            buffer
        })
    });

    // Close the read/install watcher race only when an invalidation arrived
    // while the buffer had no registration. The former unconditional reload
    // added a second serialized workspace round trip for every newly opened
    // diff file.
    let reload = cx.update(|cx| {
        remote.state.update(cx, |state, _| {
            (state.change_epoch != read_epoch).then(|| state.begin_reload(path.clone()))
        })
    });
    if let Some((generation, reload)) = reload
        && let Ok(result) = reload.await
    {
        apply_reload_result(&remote.state, &path, generation, result, None, cx);
    }

    if let Ok(language) = languages
        .load_language_for_file_path(path.as_std_path())
        .await
    {
        buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
    }
    Ok(buffer)
}

pub async fn opened_dirty_file_buffer(
    remote: &RemoteProject,
    path: Utf8PathBuf,
    cx: &mut AsyncApp,
) -> Result<Option<Entity<Buffer>>> {
    let path = normalized_path(path)?;
    Ok(cx.update(|cx| {
        remote
            .state
            .read(cx)
            .existing_buffer(&path)
            .filter(|buffer| buffer.read(cx).is_dirty())
    }))
}

async fn reload_changed(
    state: WeakEntity<RemoteProjectState>,
    paths: Vec<Utf8PathBuf>,
    cx: &mut AsyncApp,
) {
    for path in paths {
        let Ok((generation, rx)) = state.update(cx, |state, _| state.begin_reload(path.clone()))
        else {
            return;
        };
        let Ok(result) = rx.await else { return };
        let Some(state) = state.upgrade() else { return };
        apply_reload_result(&state, &path, generation, result, None, cx);
    }
}

fn apply_reload_result(
    state: &Entity<RemoteProjectState>,
    path: &Utf8PathBuf,
    generation: u64,
    result: FileReadResult,
    force_if_version: Option<clock::Global>,
    cx: &mut AsyncApp,
) {
    let decoded = match result {
        FileReadResult::File { contents, revision } => match decode_utf8(contents, path) {
            Ok((text, bom)) => Some((text, revision, bom)),
            Err(error) => {
                tracing::warn!(%path, %error, "reload workspace file");
                return;
            }
        },
        FileReadResult::Deleted => None,
        FileReadResult::Error(error) => {
            tracing::warn!(%path, %error, "reload workspace file");
            return;
        }
    };
    state.update(cx, |state, cx| {
        let Some(entry) = state.buffers.get_mut(path) else {
            return;
        };
        if entry.reload_generation != generation {
            return;
        }
        let Some(buffer) = entry.buffer.upgrade() else {
            return;
        };
        let force = force_if_version.is_some();
        if let Some(version) = force_if_version {
            if buffer.read(cx).version() != version {
                buffer.update(cx, |buffer, _| buffer.set_conflict());
                cx.emit(RemoteProjectEvent::BufferEdited);
                cx.notify();
                return;
            }
        } else if buffer.read(cx).is_dirty() {
            buffer.update(cx, |buffer, _| buffer.set_conflict());
            cx.emit(RemoteProjectEvent::BufferEdited);
            cx.notify();
            return;
        }

        match decoded {
            Some((text, revision, utf8_bom)) => {
                if !force && !entry.deleted && entry.revision == revision {
                    return;
                }
                let line_ending = text::LineEnding::detect(&text);
                buffer.update(cx, |buffer, cx| {
                    buffer.set_text(text, cx);
                    // `did_reload` does not clear an explicit conflict. Mark
                    // the newly installed version saved first so accepting a
                    // reload does not leave the buffer permanently dirty.
                    buffer.did_save(buffer.version().clone(), None, cx);
                    buffer.did_reload(buffer.version().clone(), line_ending, None, cx);
                });
                entry.revision = revision;
                entry.utf8_bom = utf8_bom;
                entry.deleted = false;
            }
            None => {
                if !entry.deleted {
                    buffer.update(cx, |buffer, _| buffer.set_conflict());
                    entry.deleted = true;
                    cx.emit(RemoteProjectEvent::BufferEdited);
                    cx.notify();
                }
            }
        }
    });
}

fn normalized_path(path: Utf8PathBuf) -> Result<Utf8PathBuf> {
    let mut normalized = Utf8PathBuf::new();
    for component in path.components() {
        let camino::Utf8Component::Normal(component) = component else {
            return Err(anyhow!(
                "workspace file path must be normalized and relative: {path}"
            ));
        };
        normalized.push(component);
    }
    if normalized.as_str() != path.as_str() || normalized.as_str().is_empty() {
        return Err(anyhow!(
            "workspace file path must be normalized and relative: {path}"
        ));
    }
    Ok(path)
}

struct SavedBuffer {
    path: Utf8PathBuf,
    buffer: Entity<Buffer>,
    version: clock::Global,
    contents: Vec<u8>,
    result: FileSaveResult,
}

pub fn save_buffers(
    remote: RemoteProject,
    buffers: impl IntoIterator<Item = Entity<Buffer>>,
    window: &mut Window,
    cx: &mut App,
) {
    let saves = buffers
        .into_iter()
        .filter_map(|buffer| {
            let state = remote.state.read(cx);
            let path = state.path_for_buffer(&buffer)?;
            let entry = state.buffers.get(&path)?;
            let buffer_read = buffer.read(cx);
            buffer_read.is_dirty().then(|| {
                (
                    path,
                    buffer.clone(),
                    buffer_read.version().clone(),
                    encode_buffer(
                        buffer_read.text(),
                        buffer_read.line_ending(),
                        entry.utf8_bom,
                    ),
                    entry.revision.clone(),
                )
            })
        })
        .collect::<Vec<_>>();
    if saves.is_empty() {
        return;
    }

    window
        .spawn(cx, async move |cx| {
            let mut results = Vec::new();
            for (path, buffer, version, contents, revision) in saves {
                let rx = remote
                    .state
                    .update(cx, |state, _| {
                        state.save(path.clone(), revision, contents.clone(), false)
                    });
                let result = rx
                    .await
                    .unwrap_or_else(|_| FileSaveResult::Error("workspace channel closed".into()));
                results.push(SavedBuffer {
                    path,
                    buffer,
                    version,
                    contents,
                    result,
                });
            }

            let mut conflicts = Vec::new();
            let mut deleted = Vec::new();
            for mut save in results {
                match std::mem::replace(
                    &mut save.result,
                    FileSaveResult::Error("save response consumed".into()),
                ) {
                    FileSaveResult::Saved { revision } => {
                        mark_saved(&remote, &save, revision, cx);
                    }
                    FileSaveResult::Conflict { contents, revision } => {
                        conflicts.push((save, contents, revision));
                    }
                    FileSaveResult::Deleted => deleted.push(save),
                    FileSaveResult::Error(error) => {
                        tracing::error!(path = %save.path, %error, "save buffer")
                    }
                }
            }

            if !conflicts.is_empty() {
                let answer = cx.update(|window, cx| {
                    window.prompt(
                        PromptLevel::Warning,
                        "One or more files changed on disk.",
                        Some("Overwrite saves the live editor contents; Reload discards them."),
                        &["Overwrite", "Reload", "Cancel"],
                        cx,
                    )
                })?;
                match answer.await {
                    Ok(0) => {
                        for (mut save, _, _) in conflicts {
                            let (contents, version) = cx.update(|_, cx| {
                                let buffer = save.buffer.read(cx);
                                let bom = remote
                                    .state
                                    .read(cx)
                                    .buffers
                                    .get(&save.path)
                                    .is_some_and(|entry| entry.utf8_bom);
                                (
                                    encode_buffer(buffer.text(), buffer.line_ending(), bom),
                                    buffer.version().clone(),
                                )
                            })?;
                            save.contents = contents.clone();
                            save.version = version;
                            let rx = remote.state.update(cx, |state, _| {
                                state.save(save.path.clone(), Vec::new(), contents, true)
                            });
                            if let Ok(FileSaveResult::Saved { revision }) = rx.await {
                                mark_saved(&remote, &save, revision, cx);
                            }
                        }
                    }
                    Ok(1) => {
                        for (save, _, _) in conflicts {
                            let (generation, reload, version) = cx.update(|_, cx| {
                                let version = save.buffer.read(cx).version().clone();
                                let (generation, reload) = remote
                                    .state
                                    .update(cx, |state, _| {
                                        state.begin_reload(save.path.clone())
                                    });
                                (generation, reload, version)
                            })?;
                            if let Ok(result) = reload.await {
                                apply_reload_result(
                                    &remote.state,
                                    &save.path,
                                    generation,
                                    result,
                                    Some(version),
                                    cx,
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }

            if !deleted.is_empty() {
                let answer = cx.update(|window, cx| {
                    window.prompt(
                        PromptLevel::Warning,
                        "One or more files were deleted on disk.",
                        Some("Recreate writes the live editor contents. Keep Editing leaves the deletion untouched."),
                        &["Recreate", "Keep Editing", "Cancel"],
                        cx,
                    )
                })?;
                if answer.await == Ok(0) {
                    for mut save in deleted {
                        let (contents, version) = cx.update(|_, cx| {
                            let buffer = save.buffer.read(cx);
                            let bom = remote
                                .state
                                .read(cx)
                                .buffers
                                .get(&save.path)
                                .is_some_and(|entry| entry.utf8_bom);
                            (
                                encode_buffer(buffer.text(), buffer.line_ending(), bom),
                                buffer.version().clone(),
                            )
                        })?;
                        save.contents = contents.clone();
                        save.version = version;
                        let rx = remote.state.update(cx, |state, _| {
                            state.save(save.path.clone(), Vec::new(), contents, true)
                        });
                        if let Ok(FileSaveResult::Saved { revision }) = rx.await {
                            mark_saved(&remote, &save, revision, cx);
                        }
                    }
                }
            }
            anyhow::Ok(())
        })
        .detach();
}

fn mark_saved(remote: &RemoteProject, save: &SavedBuffer, revision: Vec<u8>, cx: &mut AsyncApp) {
    remote.state.update(cx, |state, cx| {
        if let Some(entry) = state.buffers.get_mut(&save.path) {
            entry.revision = revision;
            entry.deleted = false;
            entry.reload_generation = entry.reload_generation.wrapping_add(1);
        }
        save.buffer.update(cx, |buffer, cx| {
            buffer.did_save(save.version.clone(), None, cx)
        });
    });
}

fn decode_utf8(mut contents: Vec<u8>, path: &Utf8PathBuf) -> Result<(String, bool)> {
    let bom = contents.starts_with(&[0xef, 0xbb, 0xbf]);
    if bom {
        contents.drain(..3);
    }
    String::from_utf8(contents)
        .map(|text| (text, bom))
        .with_context(|| format!("file is not valid UTF-8: {path}"))
}

fn encode_buffer(text: String, line_ending: text::LineEnding, bom: bool) -> Vec<u8> {
    let mut contents = Vec::with_capacity(text.len() + usize::from(bom) * 3);
    if bom {
        contents.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    }
    if line_ending == text::LineEnding::Windows {
        for part in text.split_inclusive('\n') {
            if let Some(line) = part.strip_suffix('\n') {
                contents.extend_from_slice(line.as_bytes());
                contents.extend_from_slice(b"\r\n");
            } else {
                contents.extend_from_slice(part.as_bytes());
            }
        }
    } else {
        contents.extend_from_slice(text.as_bytes());
    }
    contents
}

pub struct FileView {
    remote: RemoteProject,
    editor: Entity<editor::Editor>,
}

impl FileView {
    pub fn new(
        remote: RemoteProject,
        buffer: Entity<Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let multibuffer = cx.new(|cx| multi_buffer::MultiBuffer::singleton(buffer.clone(), cx));
            let mut editor =
                editor::Editor::new(editor::EditorMode::full(), multibuffer, None, window, cx);
            crate::editor_config::configure_file(&mut editor, window, cx);
            editor
        });
        Self { remote, editor }
    }

    pub fn editor(&self) -> &Entity<editor::Editor> {
        &self.editor
    }

    fn save(&mut self, _: &crate::FileSave, window: &mut Window, cx: &mut Context<Self>) {
        let buffers = self.editor.read(cx).buffer().read(cx).all_buffers();
        save_buffers(self.remote.clone(), buffers, window, cx);
    }
}

impl Render for FileView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().colors().editor_background;
        div()
            .key_context("RhoFileView")
            .on_action(cx.listener(Self::save))
            .size_full()
            .bg(background)
            .child(self.editor.clone())
    }
}

struct RemoteLanguageRegistry(Arc<language::LanguageRegistry>);
impl gpui::Global for RemoteLanguageRegistry {}

pub(crate) fn language_registry(cx: &mut App) -> Arc<language::LanguageRegistry> {
    if !cx.has_global::<RemoteLanguageRegistry>() {
        let languages = Arc::new(language::LanguageRegistry::new(
            cx.background_executor().clone(),
        ));
        languages.set_theme(cx.theme().clone());
        {
            let fs: Arc<dyn fs::Fs> =
                Arc::new(fs::RealFs::new(None, cx.background_executor().clone()));
            languages::init(
                languages.clone(),
                fs,
                node_runtime::NodeRuntime::unavailable(),
                cx,
            );
        }
        cx.observe_global::<GlobalTheme>({
            let languages = languages.clone();
            move |cx| languages.set_theme(cx.theme().clone())
        })
        .detach();
        cx.set_global(RemoteLanguageRegistry(languages));
    }
    cx.global::<RemoteLanguageRegistry>().0.clone()
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use super::{decode_utf8, encode_buffer, normalized_path};

    #[test]
    fn workspace_paths_are_relative_and_normalized() {
        assert!(normalized_path(Utf8PathBuf::from("src/main.rs")).is_ok());
        assert!(normalized_path(Utf8PathBuf::from("../secret")).is_err());
        assert!(normalized_path(Utf8PathBuf::from("/etc/passwd")).is_err());
        assert!(normalized_path(Utf8PathBuf::from("src/./main.rs")).is_err());
    }

    #[test]
    fn utf8_bom_and_crlf_round_trip() {
        let path = Utf8PathBuf::from("file.txt");
        let (text, bom) = decode_utf8(b"\xef\xbb\xbffirst\r\nsecond\r\n".to_vec(), &path).unwrap();
        assert!(bom);
        assert_eq!(text, "first\r\nsecond\r\n");
        assert_eq!(
            encode_buffer(
                "first\nsecond\n".to_owned(),
                text::LineEnding::Windows,
                true
            ),
            b"\xef\xbb\xbffirst\r\nsecond\r\n"
        );
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        assert!(decode_utf8(vec![0xff], &Utf8PathBuf::from("bad.txt")).is_err());
    }
}
