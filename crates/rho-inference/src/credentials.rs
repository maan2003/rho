//! Daemon-owned credential publication. Selection, file changes and refresh
//! deadlines invalidate the snapshot before resolving again. Workers never
//! refresh tokens, and a late resolution cannot replace a newer selection.
use std::time::Duration;

use notify::{Config, EventKindMask, RecommendedWatcher, RecursiveMode, Watcher as _};
use rho_core::UnixMs;
use senax_encoder::{Decode, Encode};
use tokio::sync::watch;

use crate::{ResolvedAuth, SelectedAuth};

/// Secret-bearing private worker protocol state, never public inference state.
#[derive(Clone, Encode, Decode)]
pub struct CredentialSnapshot {
    pub revision: u64,
    pub state: CredentialState,
}

#[derive(Clone, Encode, Decode)]
pub enum CredentialState {
    Pending,
    Ready {
        selected: SelectedAuth,
        auth: ResolvedAuth,
        refresh_at: u64,
    },
    Unavailable {
        selected: Option<SelectedAuth>,
        error: String,
    },
}

impl std::fmt::Debug for CredentialSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialSnapshot")
            .field("revision", &self.revision)
            .field(
                "ready",
                &matches!(self.state, CredentialState::Ready { .. }),
            )
            .finish_non_exhaustive()
    }
}

impl CredentialSnapshot {
    pub(crate) fn matches_selection(&self, selection: &Result<SelectedAuth, String>) -> bool {
        match (&self.state, selection) {
            (CredentialState::Ready { selected, .. }, Ok(wanted)) => selected == wanted,
            (
                CredentialState::Unavailable {
                    selected: Some(selected),
                    ..
                },
                Ok(wanted),
            ) => selected == wanted,
            (
                CredentialState::Unavailable {
                    selected: None,
                    error,
                },
                Err(wanted),
            ) => error == wanted,
            _ => false,
        }
    }

    /// None means pending or past the refresh deadline: wait for an update,
    /// never silently keep using the previous credential.
    pub fn current(&self) -> Option<anyhow::Result<(SelectedAuth, ResolvedAuth)>> {
        match &self.state {
            CredentialState::Ready {
                selected,
                auth,
                refresh_at,
            } if UnixMs::now().0 < *refresh_at => Some(Ok((selected.clone(), auth.clone()))),
            CredentialState::Unavailable { error, .. } => Some(Err(anyhow::anyhow!("{error}"))),
            _ => None,
        }
    }
}

pub(crate) fn subscribe(
    selection: watch::Receiver<Result<SelectedAuth, String>>,
) -> watch::Receiver<CredentialSnapshot> {
    subscribe_with(selection, |auth| auth.resolve_cached())
}

fn subscribe_with(
    mut selection: watch::Receiver<Result<SelectedAuth, String>>,
    resolve: impl Fn(crate::InferenceAuth) -> std::io::Result<(ResolvedAuth, u64)>
    + Send
    + Sync
    + 'static,
) -> watch::Receiver<CredentialSnapshot> {
    let resolve = std::sync::Arc::new(resolve);
    let initial = CredentialSnapshot {
        revision: 0,
        state: CredentialState::Pending,
    };
    let (published, updates) = watch::channel(initial);
    tokio::spawn(async move {
        let (changed, mut changes) = watch::channel(());
        loop {
            let selected = selection.borrow_and_update().clone();
            published.send_modify(|snapshot| {
                snapshot.revision += 1;
                snapshot.state = CredentialState::Pending;
            });
            changes.borrow_and_update();
            // Keep the directory watch installed through resolution and until
            // the next invalidation, including atomic replacement/deletion.
            let mut watcher = None;
            let resolution = async {
                let selected = selected
                    .as_ref()
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                let path = selected.auth.path();
                let callback_path = path.clone();
                let changed = changed.clone();
                let mut watch = RecommendedWatcher::new(
                    move |event: Result<notify::Event, notify::Error>| {
                        let relevant = event.as_ref().map_or(true, |event| {
                            event.need_rescan() || event.paths.contains(&callback_path)
                        });
                        if relevant {
                            changed.send_replace(());
                        }
                    },
                    Config::default().with_event_kinds(EventKindMask::CORE),
                )?;
                watch.watch(
                    path.parent()
                        .ok_or_else(|| anyhow::anyhow!("credential path has no parent"))?,
                    RecursiveMode::NonRecursive,
                )?;
                watcher = Some(watch);
                let auth = selected.auth.clone();
                let resolve = resolve.clone();
                let (resolved, refresh_at) =
                    tokio::task::spawn_blocking(move || resolve(auth)).await??;
                anyhow::ensure!(
                    refresh_at > UnixMs::now().0,
                    "refreshed OAuth credentials already need renewal"
                );
                Ok::<_, anyhow::Error>((resolved, refresh_at))
            };
            tokio::pin!(resolution);
            let mut invalidated = false;
            let resolved = loop {
                tokio::select! {
                    biased;
                    _ = published.closed() => return,
                    change = selection.changed() => {
                        if change.is_err() { return; }
                        invalidated = true;
                    }
                    _ = changes.changed() => invalidated = true,
                    result = &mut resolution => break result.map_err(|error| error.to_string()),
                }
            };
            // spawn_blocking cannot be cancelled. Keep one resolution in flight
            // and coalesce changes rather than accumulating abandoned refreshes.
            if invalidated {
                continue;
            }
            let delay = match &resolved {
                Ok((_, deadline)) => {
                    Duration::from_millis(deadline.saturating_sub(UnixMs::now().0))
                        .min(Duration::from_secs(60 * 60))
                }
                Err(_) => Duration::from_secs(1),
            };
            published.send_modify(|snapshot| {
                snapshot.revision += 1;
                snapshot.state = match resolved {
                    Ok((auth, refresh_at)) => CredentialState::Ready {
                        selected: selected.clone().unwrap(),
                        auth,
                        refresh_at,
                    },
                    Err(error) => CredentialState::Unavailable {
                        selected: selected.clone().ok(),
                        error,
                    },
                };
            });
            tokio::select! {
                _ = published.closed() => return,
                change = selection.changed() => {
                    if change.is_err() { return; }
                }
                _ = changes.changed() => {}
                _ = tokio::time::sleep(delay) => {}
            }
        }
    });
    updates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InferenceAuth;
    use crate::responses::oauth::ResponsesOAuthCredentials;

    fn account(path: &std::path::Path, token: &str) -> SelectedAuth {
        replace(path, token);
        SelectedAuth {
            auth: InferenceAuth::oauth_file(path),
            namespace: Some(token.into()),
            account_id: None,
        }
    }

    fn replace(path: &std::path::Path, token: &str) {
        let temporary = path.with_extension("new");
        std::fs::write(
            &temporary,
            serde_json::to_vec(&ResponsesOAuthCredentials {
                access_token: token.into(),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::rename(temporary, path).unwrap();
    }

    async fn until(
        updates: &mut watch::Receiver<CredentialSnapshot>,
        predicate: impl Fn(&CredentialSnapshot) -> bool,
    ) -> CredentialSnapshot {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = updates.borrow_and_update().clone();
                if predicate(&snapshot) {
                    return snapshot;
                }
                updates.changed().await.unwrap();
            }
        })
        .await
        .unwrap()
    }

    fn token(snapshot: &CredentialSnapshot) -> Option<String> {
        snapshot.current()?.ok().map(|(_, auth)| auth.bearer_token)
    }

    #[tokio::test]
    async fn file_replacement_removal_and_disabled_selection_are_pushed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.json");
        let selected = account(&path, "first");
        let (selection, selected_updates) = watch::channel(Ok(selected.clone()));
        let mut updates = subscribe(selected_updates);
        let first = until(&mut updates, |s| token(s).as_deref() == Some("first")).await;
        replace(&path, "replacement");
        let replacement = until(&mut updates, |s| token(s).as_deref() == Some("replacement")).await;
        assert!(replacement.revision > first.revision);
        assert!(replacement.matches_selection(&Ok(selected)));

        std::fs::remove_file(&path).unwrap();
        until(&mut updates, |s| {
            matches!(s.state, CredentialState::Unavailable { .. })
        })
        .await;
        replace(&path, "recreated");
        until(&mut updates, |s| token(s).as_deref() == Some("recreated")).await;

        let _ = selection.send_replace(Err("all accounts disabled".into()));
        let disabled = until(&mut updates, |s| {
            s.matches_selection(&Err("all accounts disabled".into()))
        })
        .await;
        assert_eq!(
            disabled.current().unwrap().unwrap_err().to_string(),
            "all accounts disabled"
        );
    }

    #[tokio::test]
    async fn invalidations_coalesce_and_late_resolution_cannot_replace_new_selection() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let a = account(&dir.path().join("a.json"), "a");
        let b = account(&dir.path().join("b.json"), "b");
        let c = account(&dir.path().join("c.json"), "c");
        let (selection, selected_updates) = watch::channel(Ok(a.clone()));
        let started = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, released) = std::sync::mpsc::channel();
        let released = std::sync::Mutex::new(released);
        let mut updates = subscribe_with(selected_updates, {
            let started = started.clone();
            let calls = calls.clone();
            move |auth| {
                calls.fetch_add(1, Ordering::Relaxed);
                if auth == a.auth {
                    started.notify_one();
                    released.lock().unwrap().recv().unwrap();
                }
                auth.resolve_cached()
            }
        });
        started.notified().await;
        let _ = selection.send_replace(Ok(b));
        tokio::task::yield_now().await;
        let _ = selection.send_replace(Ok(c.clone()));
        replace(&dir.path().join("a.json"), "rotated-a");
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "started a second blocking refresh"
        );
        assert!(updates.borrow().current().is_none());
        release.send(()).unwrap();
        let snapshot = until(&mut updates, |s| token(s).as_deref() == Some("c")).await;
        assert!(snapshot.matches_selection(&Ok(c)));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn deadline_refresh_failure_replaces_ready_without_account_failover() {
        let dir = tempfile::tempdir().unwrap();
        let selected = account(&dir.path().join("a.json"), "a");
        let (_selection, selected_updates) = watch::channel(Ok(selected.clone()));
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let mut updates = subscribe_with(selected_updates, move |auth| {
            if calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                let (resolved, _) = auth.resolve_cached()?;
                Ok((resolved, UnixMs::now().0 + 50))
            } else {
                Err(std::io::Error::other("refresh failed"))
            }
        });
        until(&mut updates, |s| token(s).as_deref() == Some("a")).await;
        let error = until(&mut updates, |s| {
            matches!(s.state, CredentialState::Unavailable { .. })
        })
        .await;
        assert!(error.matches_selection(&Ok(selected)));
        assert_eq!(
            error.current().unwrap().unwrap_err().to_string(),
            "refresh failed"
        );
    }

    #[test]
    fn pending_expired_and_secret_debug() {
        let selected = SelectedAuth {
            auth: InferenceAuth::oauth_file("/unused"),
            namespace: None,
            account_id: None,
        };
        let mut snapshot = CredentialSnapshot {
            revision: 1,
            state: CredentialState::Pending,
        };
        assert!(snapshot.current().is_none());
        snapshot.state = CredentialState::Ready {
            selected,
            auth: ResolvedAuth {
                bearer_token: "secret-bearer".into(),
                account_id: None,
                client_secret: [7; 32],
            },
            refresh_at: UnixMs::now().0,
        };
        assert!(snapshot.current().is_none());
        assert!(!format!("{snapshot:?}").contains("secret-bearer"));
        let encoded = senax_encoder::encode(&snapshot).unwrap();
        let decoded: CredentialSnapshot = senax_encoder::decode(&mut encoded.as_ref()).unwrap();
        assert_eq!(decoded.revision, 1);
        assert!(decoded.current().is_none());
    }
}
