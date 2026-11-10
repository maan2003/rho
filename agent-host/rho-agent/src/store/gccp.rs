//! What the host keeps for GCCP ([docs/gccp.md](../../../../docs/gccp.md)):
//! whom it talks to. A message is never kept: the host sends one while its
//! agent waits, and gives one it takes straight to its agent.
//!
//! Trust has two levels. The user approves a harness, by its endpoint
//! ([`rho_iroh_auth::gccp_hosts`], which the iroh listener reads too). An
//! agent accepts an address, and only one on a harness the user approved.
//! So no agent, whatever it is told, opens the host to a harness the user
//! does not know. Taking an approval back takes back every acceptance on
//! it.
//!
//! Accepting an address lets messages through and nothing else. A byte
//! stream is access to a program, so an agent opens each service by name
//! to one address it accepted, and the store keeps where that service
//! listens.
//!
//! None of this is chat: no client sees it, and a message from another
//! harness reaches its agent as input, not as a post.

use redb::TableDefinition;
use rho_agent_types::{AgentId, EndpointId, PeerAddress, ServiceName};
use rho_db::{ReadTxn, WriteTxn};
use rho_iroh_auth::gccp_hosts;
use senax_encoder::{Decode, Encode};

/// `(agent, address)`: the name the agent gave an address it accepted.
const CONTACTS: TableDefinition<(AgentId, String), String> = TableDefinition::new("gccp_contacts");
/// `(agent, address, service)`: the Unix socket, in the agent's workset, of a
/// service the agent opened to an address it accepted.
const SERVICES: TableDefinition<(AgentId, String, String), String> =
    TableDefinition::new("gccp_services");
/// The longest name a harness or an address is given.
const MAX_NAME_LEN: usize = 80;

/// An address an agent accepted.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct GccpContact {
    pub agent: AgentId,
    pub peer: PeerAddress,
    pub name: String,
    /// The services the agent opened to the address.
    pub services: Vec<GccpService>,
}

/// A service an agent opened to an address.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct GccpService {
    pub name: ServiceName,
    /// The Unix socket it listens on, as the agent's workset sees it.
    pub path: String,
}

/// A name as the host shows it to an agent inside a `[...]` header: one
/// short line that cannot close the header or start another.
fn checked_name(name: &str) -> anyhow::Result<String> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "a name must not be empty");
    anyhow::ensure!(
        name.chars().count() <= MAX_NAME_LEN,
        "a name is at most {MAX_NAME_LEN} characters"
    );
    anyhow::ensure!(
        !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '[' | ']')),
        "a name is one line without '[' or ']'"
    );
    Ok(name.to_owned())
}

pub trait GccpRead {
    /// The harnesses the user approved, with the names the user gave them.
    fn gccp_hosts(&self) -> Vec<(EndpointId, String)>;
    /// The name of an approved harness.
    fn gccp_host(&self, endpoint: EndpointId) -> Option<String>;
    /// The name `agent` gave `peer` when it accepted it.
    fn gccp_contact(&self, agent: AgentId, peer: &PeerAddress) -> Option<String>;
    /// Every acceptance, of every agent.
    fn gccp_contacts(&self) -> Vec<GccpContact>;
    /// The socket of the service `agent` opened to `peer` under `service`.
    fn gccp_service(
        &self,
        agent: AgentId,
        peer: &PeerAddress,
        service: &ServiceName,
    ) -> Option<String>;
}

impl GccpRead for ReadTxn {
    fn gccp_hosts(&self) -> Vec<(EndpointId, String)> {
        let hosts = gccp_hosts::all(self).into_iter();
        hosts
            .map(|(endpoint, name)| (EndpointId(endpoint), name))
            .collect()
    }

    fn gccp_host(&self, endpoint: EndpointId) -> Option<String> {
        gccp_hosts::name(self, &endpoint.0)
    }

    fn gccp_contact(&self, agent: AgentId, peer: &PeerAddress) -> Option<String> {
        self.open_table(CONTACTS)
            .get((agent, peer.to_string()))
            .map(|name| name.value())
    }

    fn gccp_contacts(&self) -> Vec<GccpContact> {
        let services = self.open_table(SERVICES);
        self.open_table(CONTACTS)
            .iter()
            .map(|(key, name)| {
                let (agent, address) = key.value();
                let opened = services
                    .range((agent, address.clone(), String::new())..)
                    .map(|(key, path)| (key.value(), path.value()))
                    .take_while(|((holder, to, _), _)| *holder == agent && *to == address)
                    .map(|((_, _, service), path)| GccpService {
                        name: service.parse().expect("the store keeps service names"),
                        path,
                    })
                    .collect();
                GccpContact {
                    agent,
                    peer: address_of(&address),
                    name: name.value(),
                    services: opened,
                }
            })
            .collect()
    }

    fn gccp_service(
        &self,
        agent: AgentId,
        peer: &PeerAddress,
        service: &ServiceName,
    ) -> Option<String> {
        self.open_table(SERVICES)
            .get((agent, peer.to_string(), service.to_string()))
            .map(|path| path.value())
    }
}

fn address_of(text: &str) -> PeerAddress {
    text.parse().expect("the store keeps addresses")
}

pub trait GccpWrite {
    fn init_gccp_tables(&mut self);
    /// The user approves the harness at `endpoint` under `name`: agents may
    /// accept addresses on it. Approving it again renames it.
    fn gccp_approve(&mut self, endpoint: EndpointId, name: &str) -> anyhow::Result<()>;
    /// Takes an approval back, and with it every acceptance of an address on
    /// that harness and every service opened to one. Says whether the
    /// harness was approved.
    fn gccp_revoke(&mut self, endpoint: EndpointId) -> bool;
    /// `agent` accepts `peer` under `name`. The user must have approved the
    /// peer's harness.
    fn gccp_accept(&mut self, agent: AgentId, peer: &PeerAddress, name: &str)
    -> anyhow::Result<()>;
    /// Takes an acceptance back, and the services opened to the address.
    /// Says whether there was one.
    fn gccp_remove(&mut self, agent: AgentId, peer: &PeerAddress) -> bool;
    /// Takes back all that `agent` accepted and opened: it is gone.
    fn gccp_forget(&mut self, agent: AgentId);
    /// `agent` opens the service at the socket `path` to `peer`, which it
    /// accepted, under `service`. Opening it again moves it.
    fn gccp_expose(
        &mut self,
        agent: AgentId,
        peer: &PeerAddress,
        service: &ServiceName,
        path: String,
    ) -> anyhow::Result<()>;
    /// Closes a service to `peer`. Says whether it was open.
    fn gccp_unexpose(&mut self, agent: AgentId, peer: &PeerAddress, service: &ServiceName) -> bool;
}

impl GccpWrite for WriteTxn {
    fn init_gccp_tables(&mut self) {
        gccp_hosts::init(self);
        self.open_table(CONTACTS);
        self.open_table(SERVICES);
    }

    fn gccp_approve(&mut self, endpoint: EndpointId, name: &str) -> anyhow::Result<()> {
        gccp_hosts::approve(self, &endpoint.0, checked_name(name)?);
        Ok(())
    }

    fn gccp_revoke(&mut self, endpoint: EndpointId) -> bool {
        if !gccp_hosts::revoke(self, &endpoint.0) {
            return false;
        }
        let there: Vec<(AgentId, String)> = self
            .open_table(CONTACTS)
            .iter()
            .map(|(key, _)| key.value())
            .filter(|(_, address)| address_of(address).endpoint == endpoint)
            .collect();
        for (agent, address) in there {
            remove(self, agent, address);
        }
        true
    }

    fn gccp_accept(
        &mut self,
        agent: AgentId,
        peer: &PeerAddress,
        name: &str,
    ) -> anyhow::Result<()> {
        let name = checked_name(name)?;
        anyhow::ensure!(
            gccp_hosts::name_in(self, &peer.endpoint.0).is_some(),
            "the user has not approved the host {endpoint}. Nothing on it can be accepted \
             until the user runs `rho iroh approve --role gccp {endpoint} <name>` on this host",
            endpoint = peer.endpoint
        );
        let mut contacts = self.open_table(CONTACTS);
        let key = (agent, peer.to_string());
        let held = contacts.get(key.clone()).map(|held| held.value());
        match held {
            Some(held) => anyhow::ensure!(
                held == name,
                "{peer} is already accepted as {held:?}; the user can remove it with \
                 `rho gccp remove`"
            ),
            None => {
                contacts.insert(key, name);
            }
        }
        Ok(())
    }

    fn gccp_remove(&mut self, agent: AgentId, peer: &PeerAddress) -> bool {
        remove(self, agent, peer.to_string())
    }

    fn gccp_forget(&mut self, agent: AgentId) {
        let accepted: Vec<String> = self
            .open_table(CONTACTS)
            .range((agent, String::new())..)
            .map(|(key, _)| key.value())
            .take_while(|(holder, _)| *holder == agent)
            .map(|(_, address)| address)
            .collect();
        for address in accepted {
            remove(self, agent, address);
        }
    }

    fn gccp_expose(
        &mut self,
        agent: AgentId,
        peer: &PeerAddress,
        service: &ServiceName,
        path: String,
    ) -> anyhow::Result<()> {
        let address = peer.to_string();
        anyhow::ensure!(
            self.open_table(CONTACTS)
                .get((agent, address.clone()))
                .is_some(),
            "{peer} is not an address you accepted"
        );
        self.open_table(SERVICES)
            .insert((agent, address, service.to_string()), path);
        Ok(())
    }

    fn gccp_unexpose(&mut self, agent: AgentId, peer: &PeerAddress, service: &ServiceName) -> bool {
        self.open_table(SERVICES)
            .remove((agent, peer.to_string(), service.to_string()))
            .is_some()
    }
}

/// Removes one acceptance and the services opened to its address. Says
/// whether it was there.
fn remove(write: &mut WriteTxn, agent: AgentId, address: String) -> bool {
    if write
        .open_table(CONTACTS)
        .remove((agent, address.clone()))
        .is_none()
    {
        return false;
    }
    let mut services = write.open_table(SERVICES);
    let opened: Vec<(AgentId, String, String)> = services
        .range((agent, address.clone(), String::new())..)
        .map(|(key, _)| key.value())
        .take_while(|(holder, to, _)| *holder == agent && *to == address)
        .collect();
    for key in opened {
        services.remove(key);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StoreWrite as _;

    fn agent(n: u64) -> AgentId {
        AgentId::from_counter(n, &rho_agent_types::AgentIdDomain(7)).unwrap()
    }

    fn peer(endpoint: u8, local: &str) -> PeerAddress {
        PeerAddress {
            endpoint: EndpointId([endpoint; 32]),
            local: local.parse().unwrap(),
        }
    }

    async fn db() -> (tempfile::TempDir, rho_db::RhoDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("db"));
        let mut write = db.write().await;
        write.init_store_tables();
        write.commit();
        (dir, db)
    }

    /// An agent accepts only on a harness the user approved, and a revoked
    /// approval takes the acceptances on that harness with it, and no
    /// others.
    #[tokio::test]
    async fn an_acceptance_needs_the_users_approval_of_the_harness() {
        let (_dir, db) = db().await;
        let (a, b) = (agent(1), agent(2));
        let (dpc, other, far) = (peer(9, "dpc"), peer(9, "other"), peer(8, "dpc"));
        let mut write = db.write().await;

        let refused = write.gccp_accept(a, &dpc, "dpc").unwrap_err().to_string();
        assert!(
            refused.contains(&format!("rho iroh approve --role gccp {}", dpc.endpoint)),
            "{refused}"
        );
        write.gccp_approve(dpc.endpoint, "dpc's laptop").unwrap();
        write.gccp_approve(far.endpoint, "far").unwrap();
        write.gccp_accept(a, &dpc, " dpc ").unwrap();
        write.gccp_accept(a, &dpc, "dpc").unwrap();
        assert!(
            write.gccp_accept(a, &dpc, "boss").is_err(),
            "an agent does not rename what it accepted"
        );
        write.gccp_accept(a, &other, "other").unwrap();
        write.gccp_accept(a, &far, "far dpc").unwrap();
        write.gccp_accept(b, &dpc, "dpc for b").unwrap();
        for bad in ["", "  ", "a]\n[b", "two\nlines", &"x".repeat(81)] {
            assert!(write.gccp_accept(b, &other, bad).is_err(), "{bad:?}");
            assert!(write.gccp_approve(other.endpoint, bad).is_err(), "{bad:?}");
        }
        write.gccp_approve(dpc.endpoint, "dpc").unwrap();
        write.commit();

        let read = db.read();
        assert_eq!(read.gccp_host(dpc.endpoint).as_deref(), Some("dpc"));
        assert_eq!(read.gccp_host(EndpointId([1; 32])), None);
        assert_eq!(read.gccp_contact(a, &dpc).as_deref(), Some("dpc"));
        assert_eq!(read.gccp_contact(b, &other), None);
        assert_eq!(read.gccp_contacts().len(), 4);
        drop(read);

        let mut write = db.write().await;
        assert!(write.gccp_revoke(dpc.endpoint));
        assert!(!write.gccp_revoke(dpc.endpoint));
        assert!(write.gccp_accept(a, &dpc, "dpc").is_err());
        write.commit();
        let read = db.read();
        assert_eq!(read.gccp_hosts(), vec![(far.endpoint, "far".to_owned())]);
        assert_eq!(
            read.gccp_contacts(),
            vec![GccpContact {
                agent: a,
                peer: far,
                name: "far dpc".into(),
                services: vec![],
            }]
        );
    }

    /// A service is open to the one address it was opened to, which the
    /// agent accepted. It goes when it is closed, with its acceptance, with
    /// the approval of its harness, and with its agent, and then nothing of
    /// another address or agent goes with it.
    #[tokio::test]
    async fn a_service_is_open_to_one_accepted_address() {
        let (_dir, db) = db().await;
        let (a, b) = (agent(1), agent(2));
        let (dpc, other, far) = (peer(9, "dpc"), peer(9, "other"), peer(8, "dpc"));
        let name = |name: &str| name.parse::<ServiceName>().unwrap();
        let (web, git) = (name("web"), name("git"));
        let open = |db: &rho_db::RhoDb| {
            let contacts = db.read().gccp_contacts();
            let mut open = Vec::new();
            for contact in contacts {
                for service in contact.services {
                    let to = (contact.peer.endpoint.0[0], contact.peer.local.to_string());
                    open.push((contact.agent, to, service.name.to_string(), service.path));
                }
            }
            open
        };
        let row = |agent, endpoint: u8, local: &str, service: &str, path: &str| {
            let to = (endpoint, local.to_owned());
            (agent, to, service.to_owned(), path.to_owned())
        };

        let mut write = db.write().await;
        write.gccp_approve(dpc.endpoint, "dpc").unwrap();
        write.gccp_approve(far.endpoint, "far").unwrap();
        let early = write.gccp_expose(a, &dpc, &web, "/src/web.sock".into());
        assert!(early.unwrap_err().to_string().contains("not an address"));
        for (agent, peer) in [(a, &dpc), (a, &other), (a, &far), (b, &dpc)] {
            write.gccp_accept(agent, peer, "someone").unwrap();
        }
        write
            .gccp_expose(a, &dpc, &web, "/src/old.sock".into())
            .unwrap();
        write
            .gccp_expose(a, &dpc, &web, "/src/web.sock".into())
            .unwrap();
        write
            .gccp_expose(a, &dpc, &git, "/src/git.sock".into())
            .unwrap();
        write
            .gccp_expose(a, &far, &web, "/src/far.sock".into())
            .unwrap();
        write
            .gccp_expose(b, &dpc, &web, "/src/b.sock".into())
            .unwrap();
        write.commit();
        let read = db.read();
        assert_eq!(
            read.gccp_service(a, &dpc, &web).as_deref(),
            Some("/src/web.sock")
        );
        assert_eq!(read.gccp_service(a, &other, &web), None, "another address");
        assert_eq!(read.gccp_service(b, &dpc, &git), None, "another agent");
        drop(read);
        assert_eq!(
            open(&db),
            vec![
                row(a, 8, "dpc", "web", "/src/far.sock"),
                row(a, 9, "dpc", "git", "/src/git.sock"),
                row(a, 9, "dpc", "web", "/src/web.sock"),
                row(b, 9, "dpc", "web", "/src/b.sock"),
            ]
        );

        let mut write = db.write().await;
        assert!(write.gccp_unexpose(a, &dpc, &git));
        assert!(!write.gccp_unexpose(a, &dpc, &git));
        write.commit();
        assert_eq!(open(&db).len(), 3);

        // The acceptance goes: its services go, and accepting again opens
        // none of them.
        let mut write = db.write().await;
        assert!(write.gccp_remove(a, &dpc));
        assert!(!write.gccp_remove(a, &dpc));
        write.gccp_accept(a, &dpc, "dpc again").unwrap();
        write.commit();
        assert_eq!(
            open(&db),
            vec![
                row(a, 8, "dpc", "web", "/src/far.sock"),
                row(b, 9, "dpc", "web", "/src/b.sock"),
            ]
        );

        let mut write = db.write().await;
        assert!(write.gccp_revoke(far.endpoint));
        write.commit();
        assert_eq!(open(&db), vec![row(b, 9, "dpc", "web", "/src/b.sock")]);

        let mut write = db.write().await;
        write.gccp_forget(b);
        write.commit();
        assert_eq!(open(&db), vec![]);
        let left: Vec<(AgentId, String)> = db
            .read()
            .gccp_contacts()
            .into_iter()
            .map(|contact| (contact.agent, contact.peer.local.to_string()))
            .collect();
        assert_eq!(left, [(a, "dpc".to_owned()), (a, "other".to_owned())]);
    }
}
