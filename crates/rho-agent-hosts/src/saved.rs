//! The hosts this client attaches when nothing on the command line says
//! otherwise: the set that was attached when it last ran.
//!
//! One row in the client's database, holding the whole list in attachment
//! order. Order matters: [`crate::HostId`]s are handed out in it, and the
//! first host is the one that owns Slack. The list is rewritten whole on
//! every attach and detach, so what is saved is always exactly what is
//! attached, and the next start finds the same machines in the same order.

use redb::{TableDefinition, TableHandle as _};
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Encode};

use crate::{AttachTarget, HostSpec};

const SAVED: TableDefinition<(), Sen<SavedHosts>> = TableDefinition::new("rho_hosts_saved_v1");

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
struct SavedHosts {
    hosts: Vec<SavedHost>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct SavedHost {
    name: String,
    target: SavedTarget,
}

/// [`AttachTarget`] as it is written down. The iroh endpoint id is kept in
/// its printed form, so a row this build cannot parse is a row to skip
/// rather than a file that will not open.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
enum SavedTarget {
    Unix {
        path: String,
    },
    Iroh {
        endpoint_id: String,
        ssh_destination: String,
        remote_rho: String,
    },
}

impl SavedHost {
    fn from_spec(spec: &HostSpec) -> Self {
        let target = match &spec.target {
            AttachTarget::Unix(path) => SavedTarget::Unix {
                path: path.to_string_lossy().into_owned(),
            },
            AttachTarget::Iroh {
                endpoint_id,
                ssh_destination,
                remote_rho,
            } => SavedTarget::Iroh {
                endpoint_id: endpoint_id.to_string(),
                ssh_destination: ssh_destination.clone(),
                remote_rho: remote_rho.clone(),
            },
        };
        Self {
            name: spec.name.clone(),
            target,
        }
    }

    fn into_spec(self) -> Result<HostSpec, String> {
        let target = match self.target {
            SavedTarget::Unix { path } => AttachTarget::Unix(std::path::PathBuf::from(path)),
            SavedTarget::Iroh {
                endpoint_id,
                ssh_destination,
                remote_rho,
            } => AttachTarget::Iroh {
                endpoint_id: endpoint_id
                    .parse()
                    .map_err(|error| format!("invalid iroh endpoint id: {error}"))?,
                ssh_destination,
                remote_rho,
            },
        };
        Ok(HostSpec {
            name: self.name,
            target,
        })
    }
}

/// The hosts saved by the last run, in attachment order. Nothing saved is
/// an empty list, and so is a database from before the table existed. A
/// row that does not parse is logged and dropped; the others still attach.
pub fn load(db: &RhoDb) -> Vec<HostSpec> {
    let read = db.read();
    if !read.has_table(SAVED.name()) {
        return Vec::new();
    }
    let table = read.open_table(SAVED);
    let Some(saved) = table.get(&()) else {
        return Vec::new();
    };
    let saved = saved.value().into_owned();
    saved
        .hosts
        .into_iter()
        .filter_map(|host| {
            let name = host.name.clone();
            match host.into_spec() {
                Ok(spec) => Some(spec),
                Err(error) => {
                    tracing::warn!(host = %name, %error, "a saved host could not be read and is not attached");
                    None
                }
            }
        })
        .collect()
}

/// Writes the attached set down, replacing whatever was saved. Called on
/// the thread that attaches and detaches, and blocking there: the row is
/// a few hundred bytes and the user has just typed a command, so the
/// commit's fsync is paid where the action happened rather than raced
/// against the next one.
pub fn save<'a>(db: &RhoDb, hosts: impl IntoIterator<Item = &'a HostSpec>) {
    let saved = SavedHosts {
        hosts: hosts.into_iter().map(SavedHost::from_spec).collect(),
    };
    futures::executor::block_on(async {
        let mut write = db.write().await;
        write
            .open_table(SAVED)
            .insert(&(), SenValue::borrowed(&saved));
        write.commit();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iroh_spec(name: &str) -> HostSpec {
        let secret = iroh::SecretKey::from([7; 32]);
        HostSpec {
            name: name.to_owned(),
            target: AttachTarget::Iroh {
                endpoint_id: secret.public(),
                ssh_destination: "fern".to_owned(),
                remote_rho: "/opt/rho/bin/rho".to_owned(),
            },
        }
    }

    fn describe(spec: &HostSpec) -> String {
        let target = match &spec.target {
            AttachTarget::Unix(path) => format!("unix:{}", path.display()),
            AttachTarget::Iroh {
                endpoint_id,
                ssh_destination,
                remote_rho,
            } => format!("iroh:{endpoint_id}@{ssh_destination} via {remote_rho}"),
        };
        format!("{}={target}", spec.name)
    }

    /// What was attached is what the next start attaches, in the same
    /// order: the order is the host numbering and who owns Slack.
    #[test]
    fn the_saved_set_comes_back_in_attachment_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::client::open(dir.path()).unwrap();
        assert!(load(&db).is_empty(), "a fresh database saves nothing");

        let specs = vec![
            iroh_spec("fern"),
            HostSpec {
                name: "local".to_owned(),
                target: AttachTarget::Unix("/run/user/1000/rho/rho.sock".into()),
            },
        ];
        save(&db, &specs);
        let loaded = load(&db);
        assert_eq!(
            loaded.iter().map(describe).collect::<Vec<_>>(),
            specs.iter().map(describe).collect::<Vec<_>>()
        );

        // A detach rewrites the whole row; nothing of the old set lingers.
        save(&db, &specs[1..]);
        assert_eq!(
            load(&db).iter().map(describe).collect::<Vec<_>>(),
            vec![describe(&specs[1])]
        );
        save(&db, &[]);
        assert!(
            load(&db).is_empty(),
            "detaching the last host saves an empty set"
        );
    }
}
