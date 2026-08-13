// Copyright 2026 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Clone store commands: a shared bare mirror of a remote plus instant,
//! cheap jj clones that borrow its object and index bytes. See
//! `jj_lib::clone_store` for the design.

use std::io::Write as _;
use std::path::PathBuf;

use jj_lib::backend::CommitId;
use jj_lib::clone_store::CloneStore;
use serde_json::json;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::command_error::user_error;
use crate::ui::Ui;

/// Manage a clone store: one shared mirror of a remote, many instant
/// cheap clones borrowing its bytes
///
/// A store is a bare git mirror that never prunes, plus a lazily built
/// index template. A clone is a completely stock jj repo born at the
/// store's last-fetched state in well under 100ms: its git repo borrows
/// the store's objects through `objects/info/alternates` and its jj index
/// is hardlinked from the template. Everything else — refs, remotes,
/// config, operation log — is private to the clone, so any jj or git
/// command is safe inside it.
#[derive(clap::Subcommand, Clone, Debug)]
pub enum StoreCommand {
    /// Create a store: mirror a remote into a bare git repo clones will
    /// borrow objects from
    Init(InitArgs),
    /// Fetch the remote into the store's mirror (a prefetch; clones fetch
    /// on their own regardless)
    Fetch(FetchArgs),
    /// Create a clone: a private jj repo born at the store's last-fetched
    /// state
    Clone(CloneArgs),
    /// Attach a colocated workspace (a real git worktree plus a jj
    /// workspace) to a clone
    Workspace(WorkspaceArgs),
}

#[derive(clap::Args, Clone, Debug)]
pub struct InitArgs {
    /// Store root directory to create
    store: PathBuf,
    /// URL or path of the remote to mirror
    remote: String,
}

#[derive(clap::Args, Clone, Debug)]
pub struct FetchArgs {
    /// Store root directory
    store: PathBuf,
}

#[derive(clap::Args, Clone, Debug)]
pub struct CloneArgs {
    /// Store root directory
    store: PathBuf,
    /// Clone id: a single path component (alphanumerics plus `-`, `_`,
    /// `.`; must not start with `.` or `-`)
    id: String,
}

#[derive(clap::Args, Clone, Debug)]
pub struct WorkspaceArgs {
    /// Store root directory
    store: PathBuf,
    /// Clone id
    id: String,
    /// Directory to materialize the working copy in
    workspace_root: PathBuf,
    /// Workspace name within the clone
    #[arg(long, default_value = "default")]
    name: String,
    /// Full hex commit id to check out (defaults to the clone's trunk:
    /// main/master/trunk at origin)
    #[arg(long)]
    at: Option<String>,
}

#[instrument(skip_all)]
pub async fn cmd_store(
    ui: &mut Ui,
    command: &CommandHelper,
    sub: &StoreCommand,
) -> Result<(), CommandError> {
    let settings = command.settings();
    match sub {
        StoreCommand::Init(args) => {
            let store = CloneStore::init_from_remote(&args.store, &args.remote, settings)
                .await
                .map_err(user_error)?;
            writeln!(
                ui.stdout(),
                "{}",
                json!({"store": args.store, "git": store.git_dir()})
            )?;
            Ok(())
        }
        StoreCommand::Fetch(args) => {
            let store = CloneStore::open(&args.store, settings).map_err(user_error)?;
            store.fetch().await.map_err(user_error)?;
            writeln!(ui.stdout(), "{}", json!({"store": args.store}))?;
            Ok(())
        }
        StoreCommand::Clone(args) => {
            let store = CloneStore::open(&args.store, settings).map_err(user_error)?;
            let repo_path = store.create_clone(&args.id).await.map_err(user_error)?;
            writeln!(ui.stdout(), "{}", json!({"id": args.id, "repo": repo_path}))?;
            Ok(())
        }
        StoreCommand::Workspace(args) => {
            let store = CloneStore::open(&args.store, settings).map_err(user_error)?;
            let target = args
                .at
                .as_ref()
                .map(|hex| {
                    CommitId::try_from_hex(hex)
                        .ok_or_else(|| user_error(format!("invalid commit id {hex:?}")))
                })
                .transpose()?;
            store
                .create_workspace(&args.id, &args.workspace_root, &args.name, target)
                .await
                .map_err(user_error)?;
            writeln!(
                ui.stdout(),
                "{}",
                json!({"id": args.id, "root": args.workspace_root, "name": args.name})
            )?;
            Ok(())
        }
    }
}
