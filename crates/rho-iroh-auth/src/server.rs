use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iroh::EndpointId;
use iroh::endpoint::Connection;
use redb::{TableDefinition, TypeName};
use redb_derive::Value;
use rho_db::RhoDb;

use crate::shared::{EnrollmentCode, enrollment_code};

const TRUSTED_CLIENTS: TableDefinition<TrustedClientId, TrustedClientValue> =
    TableDefinition::new("rho_iroh_auth_trusted_clients_v1");
const MEMORY_TRUST_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_MEMORY_TRUSTED_CLIENTS: usize = 4096;
const MAX_RECENT_ENROLLMENT_CODES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct TrustedClientId(EndpointId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Value)]
struct TrustedClientValue {
    trusted_at_unix_secs: u64,
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
            }),
        }
    }

    #[cfg(test)]
    async fn begin_enrollment(
        &self,
        client_endpoint_id: EndpointId,
        code: EnrollmentCode,
    ) -> Result<bool, BeginEnrollmentError> {
        if self.is_trusted(client_endpoint_id).await {
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
    pub async fn authenticate_connection(&self, conn: &Connection) -> ServerAuthDecision {
        let client_endpoint_id = conn.remote_id();
        if self.is_trusted(client_endpoint_id).await {
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

    async fn is_trusted(&self, client_endpoint_id: EndpointId) -> bool {
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

    /// Trust the exact client key associated with this active code.
    pub async fn approve_code(&self, code: &EnrollmentCode) -> Result<EndpointId, ApproveError> {
        let pending = self.inner.pending.lock().await.take_for_approval(code)?;
        self.inner.trusted.trust(pending.client_endpoint_id).await;
        Ok(pending.client_endpoint_id)
    }

    /// Trust an endpoint directly in daemon memory. Intended for a local
    /// control client reached through an already-authenticated SSH login.
    pub async fn trust_in_memory(&self, client_endpoint_id: EndpointId) {
        self.inner
            .memory_trusted
            .lock()
            .await
            .trust(client_endpoint_id);
    }

    /// Remove persistent trust for a client endpoint. Existing connections
    /// close normally; subsequent connections require enrollment again.
    pub async fn revoke(&self, client_endpoint_id: EndpointId) -> bool {
        let memory_removed = self
            .inner
            .memory_trusted
            .lock()
            .await
            .revoke(client_endpoint_id);
        self.inner.trusted.revoke(client_endpoint_id).await || memory_removed
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

        assert!(matches!(
            auth.begin_enrollment(client, code).await.unwrap(),
            false
        ));
        let approved = auth.approve_code(&code).await.unwrap();
        assert_eq!(approved, client);

        assert!(matches!(
            auth.begin_enrollment(client, EnrollmentCode::from_str("ABCD-EFGH-JM").unwrap())
                .await
                .unwrap(),
            true
        ));

        assert!(auth.revoke(client).await);
        assert!(!auth.revoke(client).await);
        assert!(matches!(
            auth.begin_enrollment(client, EnrollmentCode::from_str("ABCD-EFGH-JN").unwrap())
                .await
                .unwrap(),
            false
        ));
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
        assert!(!auth.is_trusted(client).await);
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
