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

//! Clone store commands: manage the store root that `jj git clone` and
//! `jj git fetch` use transparently. See `jj_lib::clone_store` for the
//! design.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use jj_lib::clone_store::CloneStoreConfig;
use jj_lib::clone_store::StoreRoot;
use serde_json::json;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::command_error::user_error;
use crate::command_error::user_error_with_message;
use crate::ui::Ui;

/// Manage clone stores: shared mirrors that make `jj git clone` instant
/// and `jj git fetch` local
///
/// With `git.clone-store` (or `JJ_STORE`) set to a directory, `jj git
/// clone` keeps one store per remote URL there: a bare git mirror that
/// never prunes plus a jj index template. Clones borrow the mirror's
/// objects through `objects/info/alternates` and reflink the template's
/// index, so they are born in well under a second at the store's
/// last-fetched state, and `jj git fetch` in them transfers from the
/// mirror instead of the network. Everything else about a clone — refs,
/// remotes, operation log, working copy — is a stock private jj repo.
///
/// These commands maintain the stores; nothing here is needed for a clone
/// to work.
#[derive(clap::Subcommand, Clone, Debug)]
pub enum StoreCommand {
    /// Fetch the remote into each store (all stores, or the given URLs;
    /// a URL without a store yet gets one)
    Fetch(FetchArgs),
    /// List the stores under the store root
    List(ListArgs),
    /// Serve the store root over a unix socket, keeping every store
    /// fetched in the background
    ///
    /// Clients with `git.clone-store-socket` (or `JJ_STORE_SOCKET`) set to
    /// the socket ask the server for a store instead of touching the store
    /// root themselves, so the root can be read-only for them.
    Serve(ServeArgs),
}

#[derive(clap::Args, Clone, Debug)]
pub struct FetchArgs {
    /// Remote URLs to fetch (default: every existing store)
    urls: Vec<String>,
}

#[derive(clap::Args, Clone, Debug)]
pub struct ListArgs {}

#[derive(clap::Args, Clone, Debug)]
pub struct ServeArgs {
    /// Unix socket path to listen on (replaced if it exists)
    #[arg(long)]
    socket: PathBuf,
    /// Seconds between background fetches of every store
    #[arg(long, default_value_t = 60)]
    interval: u64,
    /// Seconds within which a fetched store is served without fetching
    /// again
    #[arg(long, default_value_t = 30)]
    debounce: u64,
}

fn store_root(command: &CommandHelper) -> Result<StoreRoot, CommandError> {
    let config = CloneStoreConfig::from_settings(command.settings())?;
    let root = config
        .root
        .ok_or_else(|| user_error("No clone store root configured (set git.clone-store or JJ_STORE)"))?;
    Ok(StoreRoot::new(root, command.settings().clone()))
}

#[instrument(skip_all)]
pub async fn cmd_store(
    ui: &mut Ui,
    command: &CommandHelper,
    sub: &StoreCommand,
) -> Result<(), CommandError> {
    let stores = store_root(command)?;
    match sub {
        StoreCommand::Fetch(args) => {
            let urls = if args.urls.is_empty() {
                stores
                    .list()
                    .map_err(user_error)?
                    .iter()
                    .map(|store| store.remote_url())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(user_error)?
            } else {
                args.urls.clone()
            };
            for url in urls {
                let store = stores
                    .ensure(&url)
                    .await
                    .map_err(|err| user_error_with_message(format!("Failed to fetch {url}"), err))?;
                store.prepare_template().await.map_err(user_error)?;
                writeln!(ui.stdout(), "{}", json!({"url": url, "store": store.root()}))?;
            }
            Ok(())
        }
        StoreCommand::List(_) => {
            for store in stores.list().map_err(user_error)? {
                let url = store.remote_url().map_err(user_error)?;
                writeln!(ui.stdout(), "{}", json!({"url": url, "store": store.root()}))?;
            }
            Ok(())
        }
        StoreCommand::Serve(args) => serve(ui, stores, args),
    }
}

#[cfg(unix)]
fn serve(ui: &mut Ui, stores: StoreRoot, args: &ServeArgs) -> Result<(), CommandError> {
    use jj_lib::clone_store_server::StoreServer;

    std::fs::create_dir_all(stores.root())
        .map_err(|err| user_error_with_message("Failed to create store root", err))?;
    match std::fs::remove_file(&args.socket) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(user_error_with_message("Failed to replace socket", err)),
    }
    let listener = std::os::unix::net::UnixListener::bind(&args.socket)
        .map_err(|err| user_error_with_message("Failed to bind socket", err))?;
    let server = StoreServer::new(stores, Duration::from_secs(args.debounce));
    drop(server.spawn_refresh_loop(Duration::from_secs(args.interval)));
    writeln!(
        ui.status(),
        "Serving clone stores in {} on {}",
        server.stores().root().display(),
        args.socket.display()
    )?;
    server
        .serve(listener)
        .map_err(|err| user_error_with_message("Store server failed", err))
}

#[cfg(not(unix))]
fn serve(_ui: &mut Ui, _stores: StoreRoot, _args: &ServeArgs) -> Result<(), CommandError> {
    Err(user_error("`jj store serve` needs unix sockets"))
}
