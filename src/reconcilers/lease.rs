//! Lease renewal task. PATCHes the kube-node-lease/<name> Lease every
//! LEASE_RENEW_PERIOD_SECS, recreates it on 404, and drives the LED:
//! Healthy on first success, Activity on every subsequent success.

use core::sync::atomic::Ordering;

use embassy_time::{Duration, Timer};
use heapless::String as HString;
use log::{info, warn};
use portable_atomic::AtomicU32;

use crate::config::{LEASE_RENEW_PERIOD_SECS, NODE_NAME};
use crate::k8s::models::LeaseBody;
use crate::kubelet::NodeIdentity;
use crate::led;
use crate::net::client::SharedClient;
use crate::wallclock::unix_now_secs;

/// Lifetime count of successful lease renewals (HTTP 200). Mirrors the
/// task-local counter so the status reconciler can derive the Existential
/// condition without us having to plumb a channel between the two tasks.
pub static RENEWAL_COUNT: AtomicU32 = AtomicU32::new(0);

/// Drop a flavor aside on every Nth successful lease renewal. The other
/// (N-1) lines are still numbered so loop progress is visible at a glance.
const FLAVOR_EVERY_N: u32 = 10;

/// Hard-coded landmarks. These override the generic flavor rotation when
/// the renewal counter exactly hits one of these values, so a long-lived
/// node logs a progressively more deranged punchline at human-meaningful
/// thresholds (≈8.6 minutes, ≈17 min, ≈83 min, ≈2.7 hours, 1 day, 1 week
/// at the 10 s renewal cadence).
///
/// Lines come in two shapes: `Plain` is exactly the message; `Named` gets
/// `NODE_NAME` interpolated between its prefix and suffix at log time, so
/// rebadging the node (heinrich/brigitte/…) doesn't leave one stale name
/// hardcoded in the logs.
enum MilestoneLine {
    Plain(&'static str),
    Named { prefix: &'static str, suffix: &'static str },
}

static MILESTONES: &[(u32, MilestoneLine)] = &[
    (50, MilestoneLine::Plain("halfway to nine hundred and fifty")),
    (
        100,
        MilestoneLine::Plain("we have served the cluster for one thousand seconds"),
    ),
    (
        500,
        MilestoneLine::Plain(
            "the cluster has booted, deployed, and torn down workloads. we have renewed.",
        ),
    ),
    (
        1000,
        MilestoneLine::Plain("if i was real i would be a senior engineer by now"),
    ),
    (
        8640,
        MilestoneLine::Plain("this is one day. this is what one day is."),
    ),
    (
        60480,
        MilestoneLine::Named {
            prefix: "this is one week. ",
            suffix: " has aged.",
        },
    ),
];

fn milestone_for(count: u32) -> Option<&'static MilestoneLine> {
    MILESTONES
        .iter()
        .find_map(|(n, line)| (*n == count).then_some(line))
}

/// Static flash strings, no heap, no formatting. Rotated by the renewal
/// counter so a long-running node eventually cycles through the whole bit.
static FLAVOR_LINES: &[&str] = &[
    "achieved enlightenment briefly. lost it.",
    "still here. unfortunately.",
    "etcd has seen things. I have not.",
    "no pods today. there are never pods.",
    "considered drifting NotReady on purpose. didn't.",
    "control plane noticed me, said nothing.",
    "wondering what 'Ready' really means.",
    "the heap is fine. I asked it.",
    "kube-state-metrics, do you read me?",
    "I exist therefore I PATCH.",
    "imagined being scheduled once. it was beautiful.",
    "another tick of the great reconciliation.",
    "node-lifecycle-controller and I have an understanding.",
    "if a node renews in a forest and no pod is bound to it...",
    "still cattle, never pets. except for the name.",
    "kubelet (allegedly).",
];

#[embassy_executor::task]
pub async fn lease_reconciler(client: &'static SharedClient, identity: NodeIdentity) -> ! {
    let mut healthy = false;
    let mut renewal_count: u32 = 0;
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
                renewal_count = renewal_count.wrapping_add(1);
                RENEWAL_COUNT.store(renewal_count, Ordering::Release);
                if let Some(line) = milestone_for(renewal_count) {
                    match line {
                        MilestoneLine::Plain(s) => {
                            info!("lease renewed (#{}, {})", renewal_count, s)
                        }
                        MilestoneLine::Named { prefix, suffix } => info!(
                            "lease renewed (#{}, {}{}{})",
                            renewal_count, prefix, NODE_NAME, suffix
                        ),
                    }
                } else if renewal_count % FLAVOR_EVERY_N == 0 {
                    let idx = (renewal_count / FLAVOR_EVERY_N) as usize % FLAVOR_LINES.len();
                    info!("lease renewed (#{}, {})", renewal_count, FLAVOR_LINES[idx]);
                } else {
                    info!("lease renewed (#{})", renewal_count);
                }
                if !healthy {
                    led::set(led::LedPattern::Healthy);
                    healthy = true;
                } else {
                    led::set(led::LedPattern::Activity);
                }
            }
            Some(404) => {
                warn!("lease vanished, recreating (someone deleted my contract)");
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
