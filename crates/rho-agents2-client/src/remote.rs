//! Short requests to one agent host, valid across reconnects.
use std::future::Future;

use rho_agent_hosts::{Dialer, Link};
use rho_rpc::protocol::Call;

#[derive(Clone)]
pub struct Agents2Link {
    link: Link,
}
impl Agents2Link {
    pub fn new(link: Link) -> Self {
        Self { link }
    }
    pub fn call<C: Call>(
        &self,
        call: C,
    ) -> impl Future<Output = anyhow::Result<C::Reply>> + Send + 'static {
        self.link.run(|dialer| dial_call(dialer, call))
    }
}
async fn dial_call<C: Call>(dialer: Dialer, call: C) -> anyhow::Result<C::Reply> {
    let mut stream = dialer.open(C::PRIORITY).await?;
    rho_rpc::protocol::call(&mut stream, call).await
}
