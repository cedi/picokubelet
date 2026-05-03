use embassy_time::{Duration, Timer};
use log::warn;

use crate::config::LEASE_RENEW_PERIOD_SECS;
use crate::k8s::api::{KubeApi, ReconcileError};
use crate::kubelet::NodeIdentity;
use crate::net::client::SharedClient;

pub mod lease;
pub mod status;

pub(crate) struct ReconcileContext {
    pub api: KubeApi,
    pub identity: NodeIdentity,
}

impl ReconcileContext {
    pub const fn new(client: &'static SharedClient, identity: NodeIdentity) -> Self {
        Self {
            api: KubeApi::new(client),
            identity,
        }
    }
}

pub(crate) trait Reconciler {
    const NAME: &'static str;

    fn interval(&self) -> Duration {
        Duration::from_secs(LEASE_RENEW_PERIOD_SECS)
    }

    async fn reconcile(&mut self, ctx: &mut ReconcileContext) -> Result<(), ReconcileError>;

    async fn run(&mut self, client: &'static SharedClient, identity: NodeIdentity) -> ! {
        let mut ctx = ReconcileContext::new(client, identity);
        loop {
            Timer::after(self.interval()).await;
            if let Err(e) = self.reconcile(&mut ctx).await {
                warn!("{} reconcile failed: {}", Self::NAME, e);
            }
        }
    }
}
