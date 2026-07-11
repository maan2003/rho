use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iroh::EndpointId;
use iroh::endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, VarInt};
use redb::{TableDefinition, TypeName};
use redb_derive::Value;
use rho_db::RhoDb;

use crate::shared::{EnrollmentCode, enrollment_code};

const TRUSTED_CLIENTS: TableDefinition<TrustedClientId, TrustedClientValue> =
    TableDefinition::new("rho_iroh_auth_trusted_clients_v1");

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
    pending: tokio::sync::Mutex<PendingEnrollments>,
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
    approved: Arc<tokio::sync::Notify>,
}

#[derive(Clone, Debug)]
struct PendingApproval {
    approved: Arc<tokio::sync::Notify>,
    timeout: Duration,
}

#[derive(Clone, Debug)]
enum AuthDecision {
    Trusted,
    Pending(PendingApproval),
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
    ) -> Result<PendingApproval, BeginEnrollmentError> {
        self.prune();
        if let Some(existing_code) = self.active.iter().find_map(|(active_code, pending)| {
            (pending.client_endpoint_id == client_endpoint_id).then_some(*active_code)
        }) {
            if let Some(existing) = self.active.remove(&existing_code) {
                existing.approved.notify_waiters();
            }
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
        let approved = Arc::new(tokio::sync::Notify::new());
        let approval = PendingApproval {
            approved: Arc::clone(&approved),
            timeout: self.pending_ttl,
        };
        self.active.insert(
            code,
            PendingClient {
                client_endpoint_id,
                expires_at: Instant::now() + self.pending_ttl,
                approved,
            },
        );
        Ok(approval)
    }

    fn approve(&mut self, code: &EnrollmentCode) -> Result<EndpointId, ApproveError> {
        self.prune();
        let pending = self.active.remove(code).ok_or(ApproveError::UnknownCode)?;
        self.remember_recent(*code);
        pending.approved.notify_waiters();
        Ok(pending.client_endpoint_id)
    }

    fn prune(&mut self) {
        let now = Instant::now();
        let expired = self
            .active
            .iter()
            .filter_map(|(code, pending)| (pending.expires_at <= now).then_some(*code))
            .collect::<Vec<_>>();
        for code in expired {
            if let Some(expired) = self.active.remove(&code) {
                expired.approved.notify_waiters();
            }
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
        self.recent
            .push_back((code, Instant::now() + self.recent_ttl));
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
                pending: tokio::sync::Mutex::new(pending),
            }),
        }
    }

    async fn begin_enrollment(
        &self,
        client_endpoint_id: EndpointId,
        code: EnrollmentCode,
    ) -> Result<AuthDecision, BeginEnrollmentError> {
        if self.inner.trusted.is_trusted(client_endpoint_id).await {
            return Ok(AuthDecision::Trusted);
        }

        let approval = self
            .inner
            .pending
            .lock()
            .await
            .insert(client_endpoint_id, code)?;
        Ok(AuthDecision::Pending(approval))
    }

    async fn begin_enrollment_for_connection(
        &self,
        conn: &Connection,
    ) -> Result<AuthDecision, BeginEnrollmentError> {
        let client_endpoint_id = conn.remote_id();
        let code = enrollment_code(conn, self.inner.server_endpoint_id, client_endpoint_id);
        self.begin_enrollment(client_endpoint_id, code).await
    }

    /// Trust the exact client key associated with this active code.
    pub async fn approve_code(&self, code: &EnrollmentCode) -> Result<EndpointId, ApproveError> {
        let client_endpoint_id = self.inner.pending.lock().await.approve(code)?;
        self.inner.trusted.trust(client_endpoint_id).await;
        Ok(client_endpoint_id)
    }

    /// Remove persistent trust for a client endpoint. Existing connections
    /// close normally; subsequent connections require enrollment again.
    pub async fn revoke(&self, client_endpoint_id: EndpointId) -> bool {
        self.inner.trusted.revoke(client_endpoint_id).await
    }
}

impl EndpointHooks for IrohAuth {
    async fn after_handshake(&self, conn: &Connection) -> AfterHandshakeOutcome {
        const REJECT_ERROR_CODE: VarInt = VarInt::from_u32(0x5248_4155);

        match self.begin_enrollment_for_connection(conn).await {
            Ok(AuthDecision::Trusted) => AfterHandshakeOutcome::Accept,
            Ok(AuthDecision::Pending(approval)) => {
                let approved = tokio::time::timeout(approval.timeout, approval.approved.notified())
                    .await
                    .is_ok();
                if approved && self.inner.trusted.is_trusted(conn.remote_id()).await {
                    AfterHandshakeOutcome::Accept
                } else {
                    AfterHandshakeOutcome::Reject {
                        error_code: REJECT_ERROR_CODE,
                        reason: b"client enrollment required".to_vec(),
                    }
                }
            }
            Err(_) => AfterHandshakeOutcome::Reject {
                error_code: REJECT_ERROR_CODE,
                reason: b"client enrollment unavailable".to_vec(),
            },
        }
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
        let code = EnrollmentCode::from_str("ABCD-EFGH-JK23").unwrap();
        pending.insert(endpoint_id(11), code).unwrap();
        assert!(matches!(
            pending.insert(endpoint_id(12), code),
            Err(BeginEnrollmentError::DuplicateCode)
        ));
        assert_eq!(pending.approve(&code).unwrap(), endpoint_id(11));
        assert!(matches!(
            pending.insert(endpoint_id(12), code),
            Err(BeginEnrollmentError::DuplicateCode)
        ));
    }

    #[tokio::test]
    async fn approving_code_pins_exact_client_key() {
        let temp = tempfile::tempdir().unwrap();
        let server = endpoint_id(1);
        let client = endpoint_id(2);
        let auth = IrohAuth::new(RhoDb::open(temp.path().join("auth.redb")), server);
        let code = EnrollmentCode::from_str("ABCD-EFGH-JK23").unwrap();

        assert!(matches!(
            auth.begin_enrollment(client, code).await.unwrap(),
            AuthDecision::Pending(_)
        ));
        let approved = auth.approve_code(&code).await.unwrap();
        assert_eq!(approved, client);

        assert!(matches!(
            auth.begin_enrollment(client, EnrollmentCode::from_str("ABCD-EFGH-JK24").unwrap())
                .await
                .unwrap(),
            AuthDecision::Trusted
        ));

        assert!(auth.revoke(client).await);
        assert!(!auth.revoke(client).await);
        assert!(matches!(
            auth.begin_enrollment(client, EnrollmentCode::from_str("ABCD-EFGH-JK25").unwrap())
                .await
                .unwrap(),
            AuthDecision::Pending(_)
        ));
    }

    #[test]
    fn pending_limit_is_enforced() {
        let mut pending =
            PendingEnrollments::new(1, Duration::from_secs(60), Duration::from_secs(60));
        pending
            .insert(
                endpoint_id(11),
                EnrollmentCode::from_str("ABCD-EFGH-JK23").unwrap(),
            )
            .unwrap();
        assert!(matches!(
            pending.insert(
                endpoint_id(12),
                EnrollmentCode::from_str("ABCD-EFGH-JK24").unwrap(),
            ),
            Err(BeginEnrollmentError::PendingFull { max_pending: 1 })
        ));
    }
}
