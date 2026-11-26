use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iroh::EndpointId;
use iroh::endpoint::{Connection, WeakConnectionHandle};
use redb::{TableDefinition, TypeName};
use redb_derive::Value;
use rho_db::RhoDb;

use crate::shared::{EnrollmentCode, PairingKey, enrollment_code};

const TRUSTED_CLIENTS: TableDefinition<TrustedClientId, TrustedClientValue> =
    TableDefinition::new("rho_iroh_auth_trusted_clients_v1");
const MEMORY_TRUST_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_MEMORY_TRUSTED_CLIENTS: usize = 4096;
const MAX_RECENT_ENROLLMENT_CODES: usize = 4096;
/// Why a client's connection closed when its key was revoked.
const REVOKED: &[u8] = b"this device's key was revoked";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct TrustedClientId(EndpointId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Value)]
struct TrustedClientValue {
    trusted_at_unix_secs: u64,
}

/// A client the host trusts for good: a device's key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrustedClient {
    pub endpoint_id: EndpointId,
    /// When it came to be trusted, in seconds since the Unix epoch.
    pub trusted_at_unix_secs: u64,
}

/// Error while approving an enrollment code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApproveError {
    /// No active pending enrollment has this code.
    UnknownCode,
}

/// Server-side result sent on the auth-only first stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerAuthDecision {
    Approved,
    EnrollmentRequired(EnrollmentCode),
    Unavailable,
}

/// Proof that this authenticator admitted an endpoint identity immediately
/// after its authenticated QUIC handshake.
pub struct PreapprovedEndpoint(EndpointId);

impl PreapprovedEndpoint {
    pub(crate) fn matches(&self, endpoint_id: EndpointId) -> bool {
        self.0 == endpoint_id
    }
}

/// Library-level authenticator combining persistent trust and pending
/// approvals.
#[derive(Clone, Debug)]
pub struct IrohAuth {
    inner: Arc<IrohAuthInner>,
}

#[derive(Debug)]
struct IrohAuthInner {
    server_endpoint_id: EndpointId,
    trusted: TrustedClients,
    memory_trusted: tokio::sync::Mutex<MemoryTrustedClients>,
    pending: tokio::sync::Mutex<PendingEnrollments>,
    pairing: tokio::sync::Mutex<PairingKeys>,
    /// Each client's connections admitted while they are open, by its key:
    /// what revoking the key closes. A handle does not keep its connection
    /// open, and one closed goes at the next admission.
    admitted: std::sync::Mutex<HashMap<EndpointId, Vec<WeakConnectionHandle>>>,
}

/// How long a pairing key printed by the host stays good.
pub const PAIRING_TTL: Duration = Duration::from_secs(10 * 60);
/// Live keys at most: each is a link someone asked for on the host.
const MAX_PAIRING_KEYS: usize = 64;

/// Pairing keys not yet spent, until they expire.
#[derive(Debug, Default)]
struct PairingKeys {
    live: HashMap<PairingKey, Instant>,
}

impl PairingKeys {
    fn mint(&mut self) -> PairingKey {
        self.prune();
        if self.live.len() >= MAX_PAIRING_KEYS
            && let Some(oldest) = self
                .live
                .iter()
                .min_by_key(|(_, expires_at)| **expires_at)
                .map(|(key, _)| *key)
        {
            self.live.remove(&oldest);
        }
        let key = PairingKey::generate();
        self.live.insert(key, Instant::now() + PAIRING_TTL);
        key
    }

    /// Whether `key` was live; it is not any more.
    fn spend(&mut self, key: &PairingKey) -> bool {
        self.prune();
        self.live.remove(key).is_some()
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.live.retain(|_, expires_at| *expires_at > now);
    }
}

#[derive(Debug, Default)]
struct MemoryTrustedClients {
    expires: HashMap<EndpointId, Instant>,
}

#[derive(Clone, Debug)]
struct TrustedClients {
    db: RhoDb,
    initialized: Arc<tokio::sync::OnceCell<()>>,
}

#[derive(Debug)]
struct PendingEnrollments {
    active: HashMap<EnrollmentCode, PendingClient>,
    recent: VecDeque<(EnrollmentCode, Instant)>,
    max_pending: usize,
    pending_ttl: Duration,
    recent_ttl: Duration,
}

#[derive(Clone, Debug)]
struct PendingClient {
    client_endpoint_id: EndpointId,
    expires_at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BeginEnrollmentError {
    PendingFull { max_pending: usize },
    DuplicateCode,
}

impl redb::Value for TrustedClientId {
    type SelfType<'a>
        = TrustedClientId
    where
        Self: 'a;

    type AsBytes<'a>
        = [u8; 32]
    where
        Self: 'a;

    fn fixed_width() -> Option<usize> {
        Some(32)
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        let bytes: [u8; 32] = data.try_into().expect("trusted client id length");
        Self(EndpointId::from_bytes(&bytes).expect("trusted client id bytes"))
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'b,
    {
        *value.0.as_bytes()
    }

    fn type_name() -> TypeName {
        TypeName::new("rho-iroh-auth::TrustedClientId")
    }
}

impl redb::Key for TrustedClientId {
    fn compare(data1: &[u8], data2: &[u8]) -> std::cmp::Ordering {
        data1.cmp(data2)
    }
}

impl TrustedClients {
    fn new(db: RhoDb) -> Self {
        Self {
            db,
            initialized: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    async fn is_trusted(&self, client_endpoint_id: EndpointId) -> bool {
        self.ensure_tables().await;
        let read = self.db.read();
        let table = read.open_table(TRUSTED_CLIENTS);
        table.get(TrustedClientId(client_endpoint_id)).is_some()
    }

    async fn all(&self) -> Vec<TrustedClient> {
        self.ensure_tables().await;
        let read = self.db.read();
        read.open_table(TRUSTED_CLIENTS)
            .iter()
            .map(|(id, value)| TrustedClient {
                endpoint_id: id.value().0,
                trusted_at_unix_secs: value.value().trusted_at_unix_secs,
            })
            .collect()
    }

    async fn trust(&self, client_endpoint_id: EndpointId) {
        let mut write = self.db.write().await;
        write.open_table(TRUSTED_CLIENTS).insert(
            TrustedClientId(client_endpoint_id),
            TrustedClientValue {
                trusted_at_unix_secs: unix_now_secs(),
            },
        );
        write.commit();
    }

    async fn revoke(&self, client_endpoint_id: EndpointId) -> bool {
        self.ensure_tables().await;
        let mut write = self.db.write().await;
        let removed = write
            .open_table(TRUSTED_CLIENTS)
            .remove(TrustedClientId(client_endpoint_id))
            .is_some();
        write.commit();
        removed
    }

    async fn ensure_tables(&self) {
        self.initialized
            .get_or_init(|| async {
                let mut write = self.db.write().await;
                write.open_table(TRUSTED_CLIENTS);
                crate::gccp_hosts::init(&mut write);
                write.commit();
            })
            .await;
    }
}

impl Default for PendingEnrollments {
    fn default() -> Self {
        Self::new(10, Duration::from_secs(60), Duration::from_secs(5 * 60))
    }
}

impl PendingEnrollments {
    fn new(max_pending: usize, pending_ttl: Duration, recent_ttl: Duration) -> Self {
        Self {
            active: HashMap::new(),
            recent: VecDeque::new(),
            max_pending,
            pending_ttl,
            recent_ttl,
        }
    }

    fn insert(
        &mut self,
        client_endpoint_id: EndpointId,
        code: EnrollmentCode,
    ) -> Result<(), BeginEnrollmentError> {
        self.prune();
        if let Some(existing_code) = self.active.iter().find_map(|(active_code, pending)| {
            (pending.client_endpoint_id == client_endpoint_id).then_some(*active_code)
        }) {
            self.active.remove(&existing_code);
            self.remember_recent(existing_code);
        }
        if self.active.len() >= self.max_pending {
            return Err(BeginEnrollmentError::PendingFull {
                max_pending: self.max_pending,
            });
        }
        if self.active.contains_key(&code) || self.recent.iter().any(|(recent, _)| recent == &code)
        {
            return Err(BeginEnrollmentError::DuplicateCode);
        }
        self.active.insert(
            code,
            PendingClient {
                client_endpoint_id,
                expires_at: Instant::now() + self.pending_ttl,
            },
        );
        Ok(())
    }

    fn take_for_approval(&mut self, code: &EnrollmentCode) -> Result<PendingClient, ApproveError> {
        self.prune();
        let pending = self.active.remove(code).ok_or(ApproveError::UnknownCode)?;
        self.remember_recent(*code);
        Ok(pending)
    }

    fn prune(&mut self) {
        let now = Instant::now();
        let expired = self
            .active
            .iter()
            .filter_map(|(code, pending)| (pending.expires_at <= now).then_some(*code))
            .collect::<Vec<_>>();
        for code in expired {
            self.active.remove(&code);
            self.remember_recent(code);
        }
        while self
            .recent
            .front()
            .is_some_and(|(_, expires_at)| *expires_at <= now)
        {
            self.recent.pop_front();
        }
    }

    fn remember_recent(&mut self, code: EnrollmentCode) {
        while self.recent.len() >= MAX_RECENT_ENROLLMENT_CODES {
            self.recent.pop_front();
        }
        self.recent
            .push_back((code, Instant::now() + self.recent_ttl));
    }
}

impl MemoryTrustedClients {
    fn is_trusted(&mut self, client_endpoint_id: EndpointId) -> bool {
        self.prune();
        let Some(expires_at) = self.expires.get_mut(&client_endpoint_id) else {
            return false;
        };
        *expires_at = Instant::now() + MEMORY_TRUST_TTL;
        true
    }

    fn trust(&mut self, client_endpoint_id: EndpointId) {
        self.prune();
        if self.expires.len() >= MAX_MEMORY_TRUSTED_CLIENTS
            && !self.expires.contains_key(&client_endpoint_id)
            && let Some(oldest) = self
                .expires
                .iter()
                .min_by_key(|(_, expires_at)| *expires_at)
                .map(|(endpoint_id, _)| *endpoint_id)
        {
            self.expires.remove(&oldest);
        }
        self.expires
            .insert(client_endpoint_id, Instant::now() + MEMORY_TRUST_TTL);
    }

    fn revoke(&mut self, client_endpoint_id: EndpointId) -> bool {
        self.expires.remove(&client_endpoint_id).is_some()
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.expires.retain(|_, expires_at| *expires_at > now);
    }
}

impl IrohAuth {
    pub fn new(db: RhoDb, server_endpoint_id: EndpointId) -> Self {
        Self::with_pending(db, server_endpoint_id, PendingEnrollments::default())
    }

    pub fn server_endpoint_id(&self) -> EndpointId {
        self.inner.server_endpoint_id
    }

    fn with_pending(
        db: RhoDb,
        server_endpoint_id: EndpointId,
        pending: PendingEnrollments,
    ) -> Self {
        Self {
            inner: Arc::new(IrohAuthInner {
                server_endpoint_id,
                trusted: TrustedClients::new(db),
                memory_trusted: tokio::sync::Mutex::new(MemoryTrustedClients::default()),
                pending: tokio::sync::Mutex::new(pending),
                pairing: tokio::sync::Mutex::new(PairingKeys::default()),
                admitted: std::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    #[cfg(test)]
    async fn begin_enrollment(
        &self,
        client_endpoint_id: EndpointId,
        code: EnrollmentCode,
    ) -> Result<bool, BeginEnrollmentError> {
        if self.is_trusted_endpoint(client_endpoint_id).await {
            return Ok(true);
        }

        self.inner
            .pending
            .lock()
            .await
            .insert(client_endpoint_id, code)?;
        Ok(false)
    }

    /// Check trust and, for an unknown client, register the exporter-bound code
    /// independently verifiable by that client.
    /// A live `pairing` key trusts the client for good, and is spent.
    pub async fn authenticate_connection(
        &self,
        conn: &Connection,
        pairing: Option<PairingKey>,
    ) -> ServerAuthDecision {
        let client_endpoint_id = conn.remote_id();
        if self.is_trusted_endpoint(client_endpoint_id).await {
            return ServerAuthDecision::Approved;
        }
        if let Some(key) = pairing
            && self.inner.pairing.lock().await.spend(&key)
        {
            self.inner.trusted.trust(client_endpoint_id).await;
            return ServerAuthDecision::Approved;
        }
        let code = enrollment_code(conn, self.inner.server_endpoint_id, client_endpoint_id);
        match self
            .inner
            .pending
            .lock()
            .await
            .insert(client_endpoint_id, code)
        {
            Ok(()) => ServerAuthDecision::EnrollmentRequired(code),
            Err(_) => ServerAuthDecision::Unavailable,
        }
    }

    /// Pins a trusted identity at connection admission: the returned token
    /// approves this connection through its explicit first-stream
    /// confirmation, and a revoke meanwhile closes it once it is admitted.
    pub async fn preapprove_endpoint(
        &self,
        client_endpoint_id: EndpointId,
    ) -> Option<PreapprovedEndpoint> {
        self.is_trusted_endpoint(client_endpoint_id)
            .await
            .then_some(PreapprovedEndpoint(client_endpoint_id))
    }

    async fn is_trusted_endpoint(&self, client_endpoint_id: EndpointId) -> bool {
        if self
            .inner
            .memory_trusted
            .lock()
            .await
            .is_trusted(client_endpoint_id)
        {
            return true;
        }
        self.inner.trusted.is_trusted(client_endpoint_id).await
    }

    /// Whether the user approved the endpoint as a GCCP host
    /// ([`crate::gccp_hosts`]). That says nothing about it as a client.
    pub async fn is_gccp_host(&self, endpoint_id: EndpointId) -> bool {
        self.inner.trusted.ensure_tables().await;
        let read = self.inner.trusted.db.read();
        crate::gccp_hosts::name(&read, endpoint_id.as_bytes()).is_some()
    }

    /// A new pairing key, good for one client within [`PAIRING_TTL`].
    pub async fn mint_pairing_key(&self) -> PairingKey {
        self.inner.pairing.lock().await.mint()
    }

    /// Trust the exact client key associated with this active code.
    pub async fn approve_code(&self, code: &EnrollmentCode) -> Result<EndpointId, ApproveError> {
        let pending = self.inner.pending.lock().await.take_for_approval(code)?;
        self.inner.trusted.trust(pending.client_endpoint_id).await;
        Ok(pending.client_endpoint_id)
    }

    /// Trust an endpoint directly in agent host memory. Intended for a local
    /// control client reached through an already-authenticated SSH login.
    pub async fn trust_in_memory(&self, client_endpoint_id: EndpointId) {
        self.inner
            .memory_trusted
            .lock()
            .await
            .trust(client_endpoint_id);
    }

    /// Every client trusted for good, by an approved code or a pairing link.
    /// One trusted only for a connection's while, through the user's ssh
    /// login or on this machine, is none of them.
    pub async fn trusted_clients(&self) -> Vec<TrustedClient> {
        self.inner.trusted.all().await
    }

    /// Holds `connection`, just admitted as a client, for a revoke of its
    /// key to close. One whose key was revoked while it was being admitted
    /// is closed now; the answer says whether it stays open.
    pub(crate) async fn admit(&self, connection: &Connection) -> bool {
        let client = connection.remote_id();
        {
            let mut admitted = self.inner.admitted.lock().expect("admitted connections");
            admitted.retain(|_, held| {
                held.retain(|handle| {
                    handle
                        .upgrade()
                        .is_some_and(|open| open.close_reason().is_none())
                });
                !held.is_empty()
            });
            admitted
                .entry(client)
                .or_default()
                .push(connection.weak_handle());
        }
        // A revoke takes trust away before it closes what it holds, so a
        // connection held after that close finds the trust gone here.
        if self.is_trusted_endpoint(client).await {
            return true;
        }
        connection.close(0u32.into(), REVOKED);
        false
    }

    /// Takes back trust in a client endpoint, persistent or for its
    /// connections' while, and closes every connection it has open: it must
    /// enroll again to reach the host.
    pub async fn revoke(&self, client_endpoint_id: EndpointId) -> bool {
        let memory_removed = self
            .inner
            .memory_trusted
            .lock()
            .await
            .revoke(client_endpoint_id);
        let removed = self.inner.trusted.revoke(client_endpoint_id).await || memory_removed;
        let open = self
            .inner
            .admitted
            .lock()
            .expect("admitted connections")
            .remove(&client_endpoint_id)
            .unwrap_or_default();
        for connection in open.iter().filter_map(WeakConnectionHandle::upgrade) {
            connection.close(0u32.into(), REVOKED);
        }
        removed
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn endpoint_id(seed: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[seed; 32]).public()
    }

    #[test]
    fn pending_rejects_duplicate_active_and_recent_codes() {
        let mut pending = PendingEnrollments::default();
        let code = EnrollmentCode::from_str("ABCD-EFGH-JK").unwrap();
        pending.insert(endpoint_id(11), code).unwrap();
        assert!(matches!(
            pending.insert(endpoint_id(12), code),
            Err(BeginEnrollmentError::DuplicateCode)
        ));
        assert_eq!(
            pending.take_for_approval(&code).unwrap().client_endpoint_id,
            endpoint_id(11)
        );
        assert!(matches!(
            pending.insert(endpoint_id(12), code),
            Err(BeginEnrollmentError::DuplicateCode)
        ));
    }

    #[tokio::test]
    async fn approving_code_pins_exact_client_key() {
        let temp = tempfile::tempdir().unwrap();
        let client = endpoint_id(2);
        let auth = IrohAuth::new(RhoDb::open(temp.path().join("auth.redb")), endpoint_id(1));
        let code = EnrollmentCode::from_str("ABCD-EFGH-JK").unwrap();

        assert!(!auth.begin_enrollment(client, code).await.unwrap());
        let approved = auth.approve_code(&code).await.unwrap();
        assert_eq!(approved, client);
        assert!(auth.is_trusted_endpoint(client).await);

        assert!(
            auth.begin_enrollment(client, EnrollmentCode::from_str("ABCD-EFGH-JM").unwrap())
                .await
                .unwrap()
        );

        assert!(auth.revoke(client).await);
        assert!(!auth.revoke(client).await);
        assert!(
            !auth
                .begin_enrollment(client, EnrollmentCode::from_str("ABCD-EFGH-JN").unwrap())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_revoked_device_is_no_longer_listed_or_trusted() {
        let temp = tempfile::tempdir().unwrap();
        let auth = IrohAuth::new(RhoDb::open(temp.path().join("auth.redb")), endpoint_id(1));
        let (laptop, phone, session) = (endpoint_id(2), endpoint_id(3), endpoint_id(4));
        for (client, code) in [(laptop, "ABCD-EFGH-JK"), (phone, "ABCD-EFGH-JM")] {
            let code = EnrollmentCode::from_str(code).unwrap();
            auth.begin_enrollment(client, code).await.unwrap();
            auth.approve_code(&code).await.unwrap();
        }
        // Trusted only for its connections, through the user's ssh login.
        auth.trust_in_memory(session).await;
        let listed = |auth: &IrohAuth| {
            let auth = auth.clone();
            async move {
                let mut ids: Vec<_> = auth
                    .trusted_clients()
                    .await
                    .into_iter()
                    .map(|client| client.endpoint_id)
                    .collect();
                ids.sort();
                ids
            }
        };
        let mut both = vec![laptop, phone];
        both.sort();
        assert_eq!(listed(&auth).await, both);

        assert!(auth.revoke(laptop).await);
        assert_eq!(listed(&auth).await, [phone]);
        assert!(!auth.is_trusted_endpoint(laptop).await);
        assert!(auth.preapprove_endpoint(laptop).await.is_none());
        assert!(
            auth.is_trusted_endpoint(phone).await,
            "the other device stays"
        );
    }

    #[tokio::test]
    async fn direct_in_memory_trust_needs_no_pending_enrollment() {
        let temp = tempfile::tempdir().unwrap();
        let client = endpoint_id(4);
        let auth = IrohAuth::new(RhoDb::open(temp.path().join("auth.redb")), endpoint_id(1));
        auth.trust_in_memory(client).await;
        assert!(
            auth.begin_enrollment(client, EnrollmentCode::from_str("ABCD-EFGH-JK").unwrap())
                .await
                .unwrap()
        );
        assert!(auth.revoke(client).await);
        assert!(!auth.is_trusted_endpoint(client).await);
    }

    #[test]
    fn pending_limit_is_enforced() {
        let mut pending =
            PendingEnrollments::new(1, Duration::from_secs(60), Duration::from_secs(60));
        pending
            .insert(
                endpoint_id(11),
                EnrollmentCode::from_str("ABCD-EFGH-JK").unwrap(),
            )
            .unwrap();
        assert!(matches!(
            pending.insert(
                endpoint_id(12),
                EnrollmentCode::from_str("ABCD-EFGH-JM").unwrap(),
            ),
            Err(BeginEnrollmentError::PendingFull { max_pending: 1 })
        ));
    }

    #[test]
    fn recent_code_cache_is_bounded_under_reconnect_flood() {
        let mut pending = PendingEnrollments::default();
        let client = endpoint_id(11);
        for value in 0..(MAX_RECENT_ENROLLMENT_CODES + 100) {
            let code = EnrollmentCode::from_str(&format!("{value:010X}")).unwrap();
            pending.insert(client, code).unwrap();
        }
        assert_eq!(pending.active.len(), 1);
        assert_eq!(pending.recent.len(), MAX_RECENT_ENROLLMENT_CODES);
    }
}
