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

//! A clone store server: the one writer for a store root.
//!
//! The server keeps every store under its root fetched in the background,
//! so clones are born current without waiting on the network and fetches
//! in clones can be served from the local mirror. Clients reach it over a
//! unix socket (`git.clone-store-socket`) with one line per request:
//!
//! ```text
//! ensure <url>     init the store if missing, refresh it if stale
//! refresh <url>    fetch now (subject to the same debounce)
//! ```
//!
//! and one line back: `ok <store path>` or `error <message>`. Both
//! requests block until the store is ready; concurrent requests for one
//! URL wait on the same lock and share one fetch.

use std::collections::HashMap;
use std::io;
use std::io::BufRead as _;
use std::io::BufReader;
use std::io::Write as _;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use pollster::FutureExt as _;

use crate::clone_store::CloneStoreError;
use crate::clone_store::Context as _;
use crate::clone_store::StoreRoot;
use crate::clone_store::store_key;

type Result<T, E = CloneStoreError> = std::result::Result<T, E>;

/// Owns a store root: serializes work per store and remembers when each
/// was last fetched.
pub struct StoreServer {
    stores: StoreRoot,
    entries: Mutex<HashMap<String, Arc<Mutex<Option<Instant>>>>>,
    debounce: Duration,
}

impl StoreServer {
    /// `debounce` is how recently a store must have been fetched for a
    /// request to skip fetching it again.
    pub fn new(stores: StoreRoot, debounce: Duration) -> Arc<Self> {
        Arc::new(Self {
            stores,
            entries: Mutex::new(HashMap::new()),
            debounce,
        })
    }

    /// The root this server owns.
    pub fn stores(&self) -> &StoreRoot {
        &self.stores
    }

    fn entry(&self, url: &str) -> Arc<Mutex<Option<Instant>>> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(entries.entry(store_key(url)).or_default())
    }

    /// The store for `url`, initialized if missing and fetched unless it
    /// was fetched within the debounce window. Returns the store path.
    pub fn ensure(&self, url: &str) -> Result<PathBuf> {
        let entry = self.entry(url);
        let mut last = entry.lock().unwrap_or_else(|e| e.into_inner());
        let store = match self.stores.open(url)? {
            Some(store) => {
                if last.is_none_or(|at| at.elapsed() >= self.debounce) {
                    store.fetch().block_on()?;
                    *last = Some(Instant::now());
                }
                store
            }
            None => {
                let store = self.stores.init(url).block_on()?;
                // Clients may not be able to write the store, so the
                // template is built here, where the fetch just happened.
                store.prepare_template().block_on()?;
                *last = Some(Instant::now());
                store
            }
        };
        Ok(store.root().to_owned())
    }

    /// Fetches every store under the root that is due, returning the
    /// failures. Meant for the background loop.
    pub fn refresh_all(&self) -> Vec<(PathBuf, CloneStoreError)> {
        let stores = match self.stores.list() {
            Ok(stores) => stores,
            Err(err) => return vec![(self.stores.root().to_owned(), err)],
        };
        let mut failures = Vec::new();
        for store in stores {
            let result = store.remote_url().and_then(|url| self.ensure(&url));
            if let Err(err) = result {
                failures.push((store.root().to_owned(), err));
            }
        }
        failures
    }

    /// Runs `refresh_all` every `interval` on a background thread for as
    /// long as the server lives.
    pub fn spawn_refresh_loop(self: &Arc<Self>, interval: Duration) -> std::thread::JoinHandle<()> {
        let server = Arc::clone(self);
        std::thread::spawn(move || {
            loop {
                for (store, err) in server.refresh_all() {
                    tracing::warn!(?store, %err, "clone store background fetch failed");
                }
                std::thread::sleep(interval);
            }
        })
    }

    /// Answers one protocol line.
    pub fn handle_request(&self, line: &str) -> String {
        let line = line.trim_end_matches(['\r', '\n']);
        let response = match line.split_once(' ') {
            Some(("ensure" | "refresh", url)) if !url.trim().is_empty() => {
                self.ensure(url.trim())
            }
            _ => Err(CloneStoreError::msg(format!("unrecognized request {line:?}"))),
        };
        match response {
            Ok(path) => format!("ok {}\n", path.display()),
            Err(err) => {
                let mut message = err.to_string();
                let mut source = std::error::Error::source(&err);
                while let Some(cause) = source {
                    message.push_str(": ");
                    message.push_str(&cause.to_string());
                    source = cause.source();
                }
                format!("error {}\n", message.replace(['\r', '\n'], " "))
            }
        }
    }

    /// Serves connections on `listener` until accept fails, one thread
    /// per connection.
    pub fn serve(self: &Arc<Self>, listener: UnixListener) -> io::Result<()> {
        for stream in listener.incoming() {
            let stream = stream?;
            let server = Arc::clone(self);
            std::thread::spawn(move || {
                if let Err(err) = server.handle_connection(stream) {
                    tracing::debug!(%err, "clone store connection failed");
                }
            });
        }
        Ok(())
    }

    fn handle_connection(&self, stream: UnixStream) -> io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let response = self.handle_request(&line);
        let mut stream = stream;
        stream.write_all(response.as_bytes())?;
        stream.flush()
    }
}

/// Sends one request to a store server and returns the store path.
pub fn request(socket: &Path, verb: &str, url: &str) -> Result<PathBuf> {
    let mut stream = UnixStream::connect(socket)
        .ctx(|| format!("connect to clone store server at {}", socket.display()))?;
    stream
        .write_all(format!("{verb} {url}\n").as_bytes())
        .ctx(|| "send clone store request".to_string())?;
    stream
        .flush()
        .ctx(|| "send clone store request".to_string())?;
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .ctx(|| "read clone store response".to_string())?;
    let response = response.trim_end_matches(['\r', '\n']);
    match response.split_once(' ') {
        Some(("ok", path)) => Ok(PathBuf::from(path)),
        Some(("error", message)) => Err(CloneStoreError::msg(format!(
            "clone store server: {message}"
        ))),
        _ => Err(CloneStoreError::msg(format!(
            "clone store server closed the connection ({response:?})"
        ))),
    }
}
