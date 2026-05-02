//! Node status reconciler. Polls free heap on the lease cadence, PATCHes
//! the /status subresource on STATUS_UPDATE_PERIOD_SECS or whenever a
//! condition flips, then PATCHes the heap-free annotation on the main
//! resource (the /status subresource silently drops metadata).

use embassy_time::{Duration, Timer};
use heapless::String as HString;
use log::{info, warn};

use crate::config::{
    LEASE_RENEW_PERIOD_SECS, MEMORY_PRESSURE_FREE_BYTES, STATUS_UPDATE_PERIOD_SECS,
};
use crate::k8s::conditions::NodeCondTracker;
use crate::k8s::models::{HeapAnnotationPatch, NodeStatusPatch};
use crate::kubelet::NodeIdentity;
use crate::net::client::SharedClient;
use crate::wallclock::unix_now_secs;

#[embassy_executor::task]
pub async fn status_reconciler(
    client: &'static SharedClient,
    identity: NodeIdentity,
    mut tracker: NodeCondTracker,
) -> ! {
    let mut last_status_update = unix_now_secs();
    loop {
        Timer::after(Duration::from_secs(LEASE_RENEW_PERIOD_SECS)).await;

        let now = unix_now_secs();
        let free = esp_alloc::HEAP.free();
        let mp_now = free < MEMORY_PRESSURE_FREE_BYTES;
        let flipped = tracker.memory_pressure.observe(mp_now, now);
        let due = now != 0 && now.saturating_sub(last_status_update) >= STATUS_UPDATE_PERIOD_SECS;

        if !(flipped || due) {
            continue;
        }

        push_status(client, &identity, &tracker, now, free).await;
        last_status_update = now;
    }
}

async fn push_status(
    client: &SharedClient,
    identity: &NodeIdentity,
    tracker: &NodeCondTracker,
    now: u64,
    free: usize,
) {
    let mut body: HString<1536> = HString::new();
    if (NodeStatusPatch {
        tracker,
        heartbeat_unix: now,
    })
    .write_json(&mut body)
    .is_err()
    {
        warn!("status body build failed");
        return;
    }
    info!(
        "PATCH /status (heap: {} B, vibes: {}, MemoryPressure={})",
        free,
        if tracker.memory_pressure.value {
            "concerning"
        } else {
            "immaculate"
        },
        tracker.memory_pressure.value,
    );

    {
        let mut c = client.lock().await;
        match c
            .patch_strategic(&identity.status_path, body.as_bytes())
            .await
        {
            Ok(resp) => match resp.status {
                200 => info!("status updated (we are observably alive)"),
                other => warn!(
                    "status PATCH returned {}: {}",
                    other,
                    core::str::from_utf8(resp.body).unwrap_or("<non-utf8>"),
                ),
            },
            Err(e) => warn!("status PATCH failed: {:?}", e),
        }
    }

    let mut ann: HString<256> = HString::new();
    if (HeapAnnotationPatch { free_bytes: free })
        .write_json(&mut ann)
        .is_err()
    {
        return;
    }
    let mut c = client.lock().await;
    match c.patch_merge(&identity.node_path, ann.as_bytes()).await {
        Ok(resp) if resp.status == 200 => {}
        Ok(resp) => warn!("annotation PATCH returned {}", resp.status),
        Err(e) => warn!("annotation PATCH failed: {:?}", e),
    }
}
