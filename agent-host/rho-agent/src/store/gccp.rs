//! What the host keeps for GCCP ([docs/gccp.md](../../../../docs/gccp.md)):
//! whom it talks to, and what it still has to send.
//!
//! Trust has two levels. The user approves a harness, by its endpoint. An
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
use senax_encoder::{Decode, Encode};

/// The harnesses the user approved, each endpoint id as text: the name the
/// user gave it.
const HOSTS: TableDefinition<String, String> = TableDefinition::new("gccp_hosts");
/// `(agent, address)`: the name the agent gave an address it accepted.
const CONTACTS: TableDefinition<(AgentId, String), String> = TableDefinition::new("gccp_contacts");
/// `(agent, address, id)`: a message of the agent's that the address's
/// harness has not taken, under the id it is sent with. An agent's messages
/// to one address go out in the order of their ids.
const OUTBOX: TableDefinition<(AgentId, String, u64), String> = TableDefinition::new("gccp_outbox");
/// The id of the last message any agent sent. An id is never used again, so
/// a harness that remembers one knows a copy by it.
const LAST_ID: TableDefinition<(), u64> = TableDefinition::new("gccp_last_id");

/// The longest name a harness or an address is given.
const MAX_NAME_LEN: usize = 80;

/// An address an agent accepted.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct GccpContact {
    pub agent: AgentId,
    pub peer: PeerAddress,
    pub name: String,
    /// How many of the agent's messages its harness has not taken.
    pub unsent: u64,
}

/// The oldest message an agent has waiting for one address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GccpOutgoing {
    pub agent: AgentId,
    pub peer: PeerAddress,
    /// What the agent named the address.
    pub name: String,
    pub id: u64,
    pub body: String,
    /// The id of the newest message that waits with this one, which is this
    /// one when none do.
    pub newest: u64,
    /// How many wait, this one among them.
    pub waiting: u64,
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
    /// What is to send next: for each agent and address with messages
    /// waiting, the oldest.
    fn gccp_outgoing(&self) -> Vec<GccpOutgoing>;
}

impl GccpRead for ReadTxn {
    fn gccp_hosts(&self) -> Vec<(EndpointId, String)> {
        self.open_table(HOSTS)
            .iter()
            .map(|(endpoint, name)| (endpoint_of(&endpoint.value()), name.value()))
            .collect()
    }

    fn gccp_host(&self, endpoint: EndpointId) -> Option<String> {
        self.open_table(HOSTS)
            .get(endpoint.to_string())
            .map(|name| name.value())
    }

    fn gccp_contact(&self, agent: AgentId, peer: &PeerAddress) -> Option<String> {
        self.open_table(CONTACTS)
            .get((agent, peer.to_string()))
            .map(|name| name.value())
    }

    fn gccp_contacts(&self) -> Vec<GccpContact> {
        let outbox = self.open_table(OUTBOX);
        self.open_table(CONTACTS)
            .iter()
            .map(|(key, name)| {
                let (agent, address) = key.value();
                let unsent = outbox
                    .range((agent, address.clone(), 0)..=(agent, address.clone(), u64::MAX))
                    .count() as u64;
                GccpContact {
                    agent,
                    peer: address_of(&address),
                    name: name.value(),
                    unsent,
                }
            })
            .collect()
    }

    fn gccp_outgoing(&self) -> Vec<GccpOutgoing> {
        let contacts = self.open_table(CONTACTS);
        let mut outgoing: Vec<GccpOutgoing> = Vec::new();
        // In key order: an agent's messages to one address are together,
        // oldest first.
        for (key, body) in self.open_table(OUTBOX).iter() {
            let (agent, address, id) = key.value();
            let peer = address_of(&address);
            match outgoing.last_mut() {
                Some(head) if head.agent == agent && head.peer == peer => {
                    head.newest = id;
                    head.waiting += 1;
                }
                _ => outgoing.push(GccpOutgoing {
                    agent,
                    name: contacts
                        .get((agent, address))
                        .expect("a message waits only for an accepted address")
                        .value(),
                    peer,
                    id,
                    body: body.value(),
                    newest: id,
                    waiting: 1,
                }),
            }
        }
        outgoing
    }
}

fn endpoint_of(text: &str) -> EndpointId {
    text.parse().expect("the store keeps endpoint ids")
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
    /// that harness and what waited to be sent there. Says whether the
    /// harness was approved.
    fn gccp_revoke(&mut self, endpoint: EndpointId) -> bool;
    /// `agent` accepts `peer` under `name`. The user must have approved the
    /// peer's harness.
    fn gccp_accept(&mut self, agent: AgentId, peer: &PeerAddress, name: &str)
    -> anyhow::Result<()>;
    /// Takes an acceptance back, with what waited to be sent. Says whether
    /// there was one.
    fn gccp_remove(&mut self, agent: AgentId, peer: &PeerAddress) -> bool;
    /// Takes back all that `agent` accepted: it is gone.
    fn gccp_forget(&mut self, agent: AgentId);
    /// A message of `agent`'s waits for `peer`, which it accepted. Answers
    /// the id it is sent under.
    fn gccp_enqueue(
        &mut self,
        agent: AgentId,
        peer: &PeerAddress,
        body: String,
    ) -> anyhow::Result<u64>;
    /// The agent's messages to `peer` up to `id` are done with: taken, or
    /// given up on.
    fn gccp_done(&mut self, agent: AgentId, peer: &PeerAddress, id: u64);
}

impl GccpWrite for WriteTxn {
    fn init_gccp_tables(&mut self) {
        self.open_table(HOSTS);
        self.open_table(CONTACTS);
        self.open_table(OUTBOX);
        self.open_table(LAST_ID);
    }

    fn gccp_approve(&mut self, endpoint: EndpointId, name: &str) -> anyhow::Result<()> {
        let name = checked_name(name)?;
        self.open_table(HOSTS).insert(endpoint.to_string(), name);
        Ok(())
    }

    fn gccp_revoke(&mut self, endpoint: EndpointId) -> bool {
        if self
            .open_table(HOSTS)
            .remove(endpoint.to_string())
            .is_none()
        {
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
            self.open_table(HOSTS)
                .get(peer.endpoint.to_string())
                .is_some(),
            "the user has not approved the host {endpoint}. Nothing on it can be accepted \
             until the user runs `rho gccp approve {endpoint} <name>` on this host",
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

    fn gccp_enqueue(
        &mut self,
        agent: AgentId,
        peer: &PeerAddress,
        body: String,
    ) -> anyhow::Result<u64> {
        let address = peer.to_string();
        anyhow::ensure!(
            self.open_table(CONTACTS)
                .get((agent, address.clone()))
                .is_some(),
            "{peer} is not an address you accepted"
        );
        let mut last = self.open_table(LAST_ID);
        let id = last.get(()).map_or(0, |id| id.value()) + 1;
        last.insert((), id);
        drop(last);
        self.open_table(OUTBOX).insert((agent, address, id), body);
        Ok(id)
    }

    fn gccp_done(&mut self, agent: AgentId, peer: &PeerAddress, id: u64) {
        let address = peer.to_string();
        let mut outbox = self.open_table(OUTBOX);
        let done: Vec<u64> = outbox
            .range((agent, address.clone(), 0)..=(agent, address.clone(), id))
            .map(|(key, _)| key.value().2)
            .collect();
        for id in done {
            outbox.remove((agent, address.clone(), id));
        }
    }
}

/// Removes one acceptance and what waited for it. Says whether it was there.
fn remove(write: &mut WriteTxn, agent: AgentId, address: String) -> bool {
    if write
        .open_table(CONTACTS)
        .remove((agent, address.clone()))
        .is_none()
    {
        return false;
    }
    let peer = address_of(&address);
    write.gccp_done(agent, &peer, u64::MAX);
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
            refused.contains(&format!("rho gccp approve {}", dpc.endpoint)),
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
                unsent: 0
            }]
        );
    }

    /// What an agent sends waits for its address alone, oldest first, under
    /// ids that are never used twice, until it is done with or its
    /// acceptance goes.
    #[tokio::test]
    async fn the_outbox_holds_each_addresss_messages_in_order() {
        let (_dir, db) = db().await;
        let (a, b) = (agent(1), agent(2));
        let (dpc, other) = (peer(9, "dpc"), peer(9, "other"));
        let mut write = db.write().await;
        write.gccp_approve(dpc.endpoint, "dpc").unwrap();
        assert!(
            write.gccp_enqueue(a, &dpc, "early".into()).is_err(),
            "nothing is sent to an address that is not accepted"
        );
        write.gccp_accept(a, &dpc, "dpc").unwrap();
        write.gccp_accept(a, &other, "other").unwrap();
        write.gccp_accept(b, &dpc, "b's dpc").unwrap();
        let one = write.gccp_enqueue(a, &dpc, "one".into()).unwrap();
        let aside = write.gccp_enqueue(a, &other, "aside".into()).unwrap();
        let two = write.gccp_enqueue(a, &dpc, "two".into()).unwrap();
        let three = write.gccp_enqueue(a, &dpc, "three".into()).unwrap();
        let from_b = write.gccp_enqueue(b, &dpc, "from b".into()).unwrap();
        write.commit();

        let heads = |db: &rho_db::RhoDb| {
            let mut heads: Vec<(AgentId, String, String, u64, u64)> = db
                .read()
                .gccp_outgoing()
                .into_iter()
                .map(|out| {
                    assert_eq!(out.name.is_empty(), false);
                    (
                        out.agent,
                        out.peer.local.to_string(),
                        out.body,
                        out.newest,
                        out.waiting,
                    )
                })
                .collect();
            heads.sort();
            heads
        };
        assert_eq!(
            heads(&db),
            vec![
                (a, "dpc".into(), "one".into(), three, 3),
                (a, "other".into(), "aside".into(), aside, 1),
                (b, "dpc".into(), "from b".into(), from_b, 1),
            ]
        );
        let unsent = |db: &rho_db::RhoDb, agent, peer: &PeerAddress| {
            let contacts = db.read().gccp_contacts();
            let contact = contacts
                .iter()
                .find(|contact| contact.agent == agent && contact.peer == *peer);
            contact.map(|contact| contact.unsent)
        };
        assert_eq!(unsent(&db, a, &dpc), Some(3));

        // The first is taken: the second is next.
        let mut write = db.write().await;
        write.gccp_done(a, &dpc, one);
        write.commit();
        assert_eq!(heads(&db)[0], (a, "dpc".into(), "two".into(), three, 2));

        // The second is given up on, and the third with it. A message sent
        // after that is new, under a new id.
        let mut write = db.write().await;
        write.gccp_done(a, &dpc, three);
        let four = write.gccp_enqueue(a, &dpc, "four".into()).unwrap();
        write.commit();
        assert!([one, aside, two, three, from_b].iter().all(|id| *id < four));
        assert_eq!(heads(&db)[0], (a, "dpc".into(), "four".into(), four, 1));

        // An acceptance taken back sends nothing more; an agent that is
        // gone neither.
        let mut write = db.write().await;
        assert!(write.gccp_remove(a, &dpc));
        assert!(!write.gccp_remove(a, &dpc));
        write.gccp_forget(b);
        write.commit();
        assert_eq!(
            heads(&db),
            vec![(a, "other".into(), "aside".into(), aside, 1)]
        );
        assert_eq!(unsent(&db, a, &dpc), None);
        assert_eq!(unsent(&db, b, &dpc), None);

        // Ids go on where they were after the outbox empties.
        let mut write = db.write().await;
        write.gccp_done(a, &other, aside);
        let five = write.gccp_enqueue(a, &other, "five".into()).unwrap();
        write.commit();
        assert!(five > four);
    }
}
