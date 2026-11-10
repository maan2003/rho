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
//! None of this is chat: no client sees it, and a message from another
//! harness reaches its agent as input, not as a post.

use redb::TableDefinition;
use rho_agent_types::{AgentId, EndpointId, PeerAddress};
use rho_db::{ReadTxn, WriteTxn};
use rho_iroh_auth::gccp_hosts;
use senax_encoder::{Decode, Encode};

/// `(agent, address)`: the name the agent gave an address it accepted.
const CONTACTS: TableDefinition<(AgentId, String), String> = TableDefinition::new("gccp_contacts");
/// The longest name a harness or an address is given.
const MAX_NAME_LEN: usize = 80;

/// An address an agent accepted.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct GccpContact {
    pub agent: AgentId,
    pub peer: PeerAddress,
    pub name: String,
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
        self.open_table(CONTACTS)
            .iter()
            .map(|(key, name)| {
                let (agent, address) = key.value();
                GccpContact {
                    agent,
                    peer: address_of(&address),
                    name: name.value(),
                }
            })
            .collect()
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
    /// that harness. Says whether the harness was approved.
    fn gccp_revoke(&mut self, endpoint: EndpointId) -> bool;
    /// `agent` accepts `peer` under `name`. The user must have approved the
    /// peer's harness.
    fn gccp_accept(&mut self, agent: AgentId, peer: &PeerAddress, name: &str)
    -> anyhow::Result<()>;
    /// Takes an acceptance back. Says whether there was one.
    fn gccp_remove(&mut self, agent: AgentId, peer: &PeerAddress) -> bool;
    /// Takes back all that `agent` accepted: it is gone.
    fn gccp_forget(&mut self, agent: AgentId);
}

impl GccpWrite for WriteTxn {
    fn init_gccp_tables(&mut self) {
        gccp_hosts::init(self);
        self.open_table(CONTACTS);
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
        let mut contacts = self.open_table(CONTACTS);
        for key in there {
            contacts.remove(key);
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
        self.open_table(CONTACTS)
            .remove((agent, peer.to_string()))
            .is_some()
    }

    fn gccp_forget(&mut self, agent: AgentId) {
        let accepted: Vec<String> = self
            .open_table(CONTACTS)
            .range((agent, String::new())..)
            .map(|(key, _)| key.value())
            .take_while(|(holder, _)| *holder == agent)
            .map(|(_, address)| address)
            .collect();
        let mut contacts = self.open_table(CONTACTS);
        for address in accepted {
            contacts.remove((agent, address));
        }
    }
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
            }]
        );
    }

    /// An acceptance taken back is that one alone, and an agent that is gone
    /// takes its own with it and nobody else's.
    #[tokio::test]
    async fn an_acceptance_goes_alone_or_with_its_agent() {
        let (_dir, db) = db().await;
        let (a, b) = (agent(1), agent(2));
        let (dpc, other) = (peer(9, "dpc"), peer(9, "other"));
        let mut write = db.write().await;
        write.gccp_approve(dpc.endpoint, "dpc").unwrap();
        write.gccp_accept(a, &dpc, "dpc").unwrap();
        write.gccp_accept(a, &other, "other").unwrap();
        write.gccp_accept(b, &dpc, "b's dpc").unwrap();
        write.gccp_accept(b, &other, "b's other").unwrap();
        assert!(write.gccp_remove(a, &dpc));
        assert!(!write.gccp_remove(a, &dpc));
        write.gccp_forget(b);
        write.commit();
        assert_eq!(
            db.read().gccp_contacts(),
            vec![GccpContact {
                agent: a,
                peer: other,
                name: "other".into(),
            }]
        );
    }
}
