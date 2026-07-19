//! Remote zed projects over the daemon connection.
//!
//! Each opened workspace gets its own ui-proto channel; the channel carries
//! prost-encoded `proto::Envelope`s between a client-side
//! [`remote::RemoteClient`] here and a `HeadlessProject` session inside
//! rho-daemon. Paths in this layer are the daemon's view of the checkout
//! (pool slot paths) — the origin-path illusion only exists inside agent
//! namespaces and never crosses this wire.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow};
use camino::Utf8PathBuf;
use client::{Client, UserStore};
use editor::Editor;
use futures::channel::mpsc::{Sender, UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use futures::{FutureExt as _, StreamExt as _, select_biased};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, Task, Window, div,
};
use project::{Project, ProjectPath};
use prost::Message as _;
use remote::{
    CommandTemplate, ConnectionIdentifier, CustomConnectionOptions, RemoteClient,
    RemoteClientDelegate, RemoteConnection, RemoteConnectionOptions,
};
use rho_ui_proto::WorkspaceInfo;
use rpc::proto::Envelope;
use theme::ActiveTheme as _;
use util::paths::{PathStyle, RemotePathBuf};

use crate::connection::{Connection, ZedChannel};

/// Opens a zed channel for `workspace` and builds a remote [`Project`] over
/// it. Returns the project together with the workspace's checkout root as
/// the daemon sees it; worktrees are opened by absolute paths under it.
pub fn open_remote_project(
    connection: &Connection,
    workspace: WorkspaceInfo,
    cx: &mut App,
) -> Task<Result<(Entity<Project>, Utf8PathBuf)>> {
    let name = workspace_label(&workspace);
    let channel_task = connection.open_channel(workspace, cx);
    cx.spawn(async move |cx| {
        let ZedChannel {
            root,
            outgoing,
            incoming,
        } = channel_task.await.context("channel dial task failed")??;
        // Kept alive with the connection: dropping it would tell RemoteClient
        // the user cancelled the connection attempt.
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let remote_connection = Arc::new(RhoRemoteConnection {
            name,
            outgoing,
            incoming: Mutex::new(Some(incoming)),
            killed: AtomicBool::new(false),
            _cancel: cancel_tx,
        });
        let remote_client = cx
            .update(|cx| {
                RemoteClient::new(
                    ConnectionIdentifier::setup(),
                    remote_connection,
                    cancel_rx,
                    Arc::new(NoopDelegate),
                    cx,
                )
            })
            .await?
            .context("remote client connection was cancelled")?;
        let project = cx.update(|cx| {
            let (client, user_store, languages, fs) = project_deps(cx);
            Project::remote(
                remote_client,
                client,
                node_runtime::NodeRuntime::unavailable(),
                user_store,
                languages,
                fs,
                false,
                cx,
            )
        });
        Ok((project, root))
    })
}

/// Opens `path` (relative to the workspace root, or absolute) as a buffer
/// in a remote project over `workspace`'s channel. The caller builds the
/// [`FileView`] with its own window.
pub fn open_file_buffer(
    connection: &Connection,
    workspace: WorkspaceInfo,
    path: Utf8PathBuf,
    cx: &mut App,
) -> Task<Result<(Entity<Project>, Entity<language::Buffer>)>> {
    let project_task = open_remote_project(connection, workspace, cx);
    cx.spawn(async move |cx| {
        let (project, root) = project_task.await?;
        let abs_path = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        let (worktree, rel_path) = cx
            .update(|cx| {
                project.update(cx, |project, cx| {
                    project.find_or_create_worktree(abs_path.as_std_path(), true, cx)
                })
            })
            .await?;
        let buffer = cx
            .update(|cx| {
                project.update(cx, |project, cx| {
                    let worktree_id = worktree.read(cx).id();
                    project.open_buffer(
                        ProjectPath {
                            worktree_id,
                            path: rel_path,
                        },
                        cx,
                    )
                })
            })
            .await?;
        Ok((project, buffer))
    })
}

/// A single remote buffer, shown as a surface in the pane tree. The view
/// owns the whole remote stack — dropping it drops the editor, project,
/// remote client, and daemon channel in one chain.
pub struct FileView {
    editor: Entity<Editor>,
    project: Entity<Project>,
    buffer: Entity<language::Buffer>,
}

impl FileView {
    pub fn new(
        project: Entity<Project>,
        buffer: Entity<language::Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer.clone(), Some(project.clone()), window, cx);
            crate::editor_config::configure_file(&mut editor, window, cx);
            editor
        });
        Self {
            editor,
            project,
            buffer,
        }
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    /// The shared content behind this view; a split builds a sibling view
    /// (its own editor, cursor, scroll) over the same pair.
    pub fn shared_content(&self) -> (Entity<Project>, Entity<language::Buffer>) {
        (self.project.clone(), self.buffer.clone())
    }

    fn save(&mut self, _: &crate::FileSave, _window: &mut Window, cx: &mut Context<Self>) {
        let buffers = self.editor.read(cx).buffer().read(cx).all_buffers();
        let project = self.project.clone();
        for buffer in buffers {
            project
                .update(cx, |project, cx| project.save_buffer(buffer, cx))
                .detach();
        }
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

fn workspace_label(workspace: &WorkspaceInfo) -> String {
    match workspace {
        WorkspaceInfo::Workspace { repo, id } | WorkspaceInfo::Sandbox { repo, id } => {
            format!("{repo}#{}", id.encoded())
        }
        WorkspaceInfo::UserCheckout { repo } => format!("{repo}#user"),
    }
}

/// Dependencies shared by every remote project in this process: the
/// client/user-store pair zed's project layer wants (they never talk to
/// collab — the http client is blocked), plus one language registry with
/// the bundled grammars so remote buffers get detection and syntax
/// highlighting. Language servers stay on the daemon side; the registry
/// here only ever parses.
struct RemoteProjectDeps {
    client: Arc<Client>,
    user_store: Entity<UserStore>,
    languages: Arc<language::LanguageRegistry>,
    fs: Arc<dyn fs::Fs>,
}

impl gpui::Global for RemoteProjectDeps {}

fn project_deps(
    cx: &mut App,
) -> (
    Arc<Client>,
    Entity<UserStore>,
    Arc<language::LanguageRegistry>,
    Arc<dyn fs::Fs>,
) {
    if !cx.has_global::<RemoteProjectDeps>() {
        let http = Arc::new(http_client::HttpClientWithUrl::new(
            Arc::new(http_client::BlockedHttpClient),
            "http://127.0.0.1",
            None,
        ));
        let client = Client::new(Arc::new(clock::RealSystemClock), http, cx);
        let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
        Project::init(&client, cx);
        let languages = Arc::new(language::LanguageRegistry::new(
            cx.background_executor().clone(),
        ));
        let fs: Arc<dyn fs::Fs> = Arc::new(fs::RealFs::new(None, cx.background_executor().clone()));
        languages::init(
            languages.clone(),
            fs.clone(),
            node_runtime::NodeRuntime::unavailable(),
            cx,
        );
        cx.set_global(RemoteProjectDeps {
            client,
            user_store,
            languages,
            fs,
        });
    }
    let deps = cx.global::<RemoteProjectDeps>();
    (
        deps.client.clone(),
        deps.user_store.clone(),
        deps.languages.clone(),
        deps.fs.clone(),
    )
}

/// The transport: envelopes ride a dedicated stream to the daemon. There is
/// no process to launch or binary to upload, so most of the trait is inert.
struct RhoRemoteConnection {
    name: String,
    /// Dropping this half-closes the stream; the daemon tears the headless
    /// project session down on EOF.
    outgoing: UnboundedSender<Vec<u8>>,
    /// Taken by the first (only) `start_proxy` call; reconnecting over a
    /// dead daemon channel is not possible.
    incoming: Mutex<Option<UnboundedReceiver<Vec<u8>>>>,
    killed: AtomicBool,
    _cancel: oneshot::Sender<()>,
}

#[async_trait::async_trait(?Send)]
impl RemoteConnection for RhoRemoteConnection {
    fn start_proxy(
        &self,
        _unique_identifier: String,
        _reconnect: bool,
        incoming_tx: UnboundedSender<Envelope>,
        mut outgoing_rx: UnboundedReceiver<Envelope>,
        mut connection_activity_tx: Sender<()>,
        _delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Task<Result<i32>> {
        let Some(mut incoming) = self.incoming.lock().unwrap().take() else {
            return Task::ready(Err(anyhow!(
                "rho zed channels cannot reconnect; reopen the workspace instead"
            )));
        };
        let outgoing = self.outgoing.clone();
        cx.background_spawn(async move {
            loop {
                select_biased! {
                    bytes = incoming.next().fuse() => {
                        // Stream end = the daemon side is gone (session
                        // teardown or lost connection).
                        let Some(bytes) = bytes else { return Ok(1) };
                        connection_activity_tx.try_send(()).ok();
                        let envelope = Envelope::decode(bytes.as_slice())
                            .context("bad envelope from daemon")?;
                        if incoming_tx.unbounded_send(envelope).is_err() {
                            return Ok(0);
                        }
                    }
                    envelope = outgoing_rx.next().fuse() => {
                        let Some(envelope) = envelope else { return Ok(0) };
                        if outgoing.unbounded_send(envelope.encode_to_vec()).is_err() {
                            return Ok(1);
                        }
                    }
                }
            }
        })
    }

    fn upload_directory(
        &self,
        _src_path: PathBuf,
        _dest_path: RemotePathBuf,
        _cx: &App,
    ) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    async fn kill(&self) -> Result<()> {
        self.killed.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn has_been_killed(&self) -> bool {
        self.killed.load(Ordering::Relaxed)
    }

    fn build_command(
        &self,
        _program: Option<String>,
        _args: &[String],
        _env: &collections::HashMap<String, String>,
        _working_dir: Option<String>,
        _port_forward: Option<(u16, String, u16)>,
        _interactive: remote::Interactive,
    ) -> Result<CommandTemplate> {
        Err(anyhow!("rho zed channels do not run remote commands"))
    }

    fn build_forward_ports_command(
        &self,
        _forwards: Vec<(u16, String, u16)>,
    ) -> Result<CommandTemplate> {
        Err(anyhow!("rho zed channels do not forward ports"))
    }

    fn connection_options(&self) -> RemoteConnectionOptions {
        RemoteConnectionOptions::Custom(CustomConnectionOptions {
            name: self.name.clone(),
        })
    }

    fn path_style(&self) -> PathStyle {
        PathStyle::Posix
    }

    fn remote_platform(&self) -> remote::RemotePlatform {
        remote::RemotePlatform {
            os: remote::RemoteOs::Linux,
            arch: remote::RemoteArch::X86_64,
        }
    }

    fn remote_os_version(&self) -> Option<String> {
        None
    }

    fn shell(&self) -> String {
        "sh".to_owned()
    }

    fn default_system_shell(&self) -> String {
        "sh".to_owned()
    }

    fn has_wsl_interop(&self) -> bool {
        false
    }
}

/// The remote server is in-process and needs no passwords, downloads, or
/// status UI.
struct NoopDelegate;

impl RemoteClientDelegate for NoopDelegate {
    fn ask_password(
        &self,
        _prompt: String,
        _sender: oneshot::Sender<askpass::EncryptedPassword>,
        _cx: &mut AsyncApp,
    ) {
    }

    fn download_server_binary_locally(
        &self,
        _platform: remote::RemotePlatform,
        _release_channel: release_channel::ReleaseChannel,
        _version: Option<semver::Version>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<PathBuf>> {
        Task::ready(Err(anyhow!("rho zed channels have no server binary")))
    }

    fn get_download_url(
        &self,
        _platform: remote::RemotePlatform,
        _release_channel: release_channel::ReleaseChannel,
        _version: Option<semver::Version>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<Option<String>>> {
        Task::ready(Ok(None))
    }

    fn set_status(&self, _status: Option<&str>, _cx: &mut AsyncApp) {}
}
