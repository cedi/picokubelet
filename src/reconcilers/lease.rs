//! Lease renewal task. PATCHes the kube-node-lease/<name> Lease every
//! LEASE_RENEW_PERIOD_SECS, recreates it on 404, and drives the LED:
//! Healthy on first success, Activity on every subsequent success.

use embassy_time::{Duration, Timer};
use heapless::String as HString;
use log::{info, warn};

use crate::config::LEASE_RENEW_PERIOD_SECS;
use crate::k8s::models::LeaseBody;
use crate::kubelet::NodeIdentity;
use crate::led;
use crate::net::client::SharedClient;
use crate::wallclock::unix_now_secs;

#[embassy_executor::task]
pub async fn lease_reconciler(client: &'static SharedClient, identity: NodeIdentity) -> ! {
    let mut healthy = false;
    loop {
        Timer::after(Duration::from_secs(LEASE_RENEW_PERIOD_SECS)).await;

        let mut body: HString<512> = HString::new();
        if (LeaseBody {
            renew_unix: unix_now_secs(),
        })
        .write_json(&mut body)
        .is_err()
        {
            warn!("lease body build failed (clock not anchored?)");
            continue;
        }

        let renew_status = {
            let mut c = client.lock().await;
            match c.patch_merge(&identity.lease_path, body.as_bytes()).await {
                Ok(resp) => Some(resp.status),
                Err(e) => {
                    warn!("lease renewal failed: {:?}", e);
                    None
                }
            }
        };

        match renew_status {
            Some(200) => {
                info!("lease renewed");
                if !healthy {
                    led::set(led::LedPattern::Healthy);
                    healthy = true;
                } else {
                    led::set(led::LedPattern::Activity);
                }
            }
            Some(404) => {
                warn!("lease vanished, recreating");
                let mut c = client.lock().await;
                let _ = c
                    .post(
                        "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases",
                        body.as_bytes(),
                    )
                    .await;
            }
            Some(other) => warn!("lease renewal returned {}", other),
            None => {}
        }
    }
}
