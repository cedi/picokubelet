//! Node status reconciler. Polls free heap on the lease cadence, PATCHes
//! the /status subresource on STATUS_UPDATE_PERIOD_SECS or whenever any
//! tracked condition flips, then PATCHes the heap-free annotation on the
//! main resource (the /status subresource silently drops metadata).

use core::sync::atomic::Ordering;

use embassy_time::{Duration, Instant, Timer};
use heapless::String as HString;
use log::{info, warn};

use crate::config::{
    HAUNTED_SLIP_SECS, LEASE_RENEW_PERIOD_SECS, MEMORY_PRESSURE_FREE_BYTES,
    STATUS_UPDATE_PERIOD_SECS,
};
use crate::k8s::conditions::{
    CustomCondInputs, CustomCondTracker, NodeCondTracker, eval_caffeinated, eval_existential,
    eval_haunted, eval_peckish, eval_vibes,
};
use crate::k8s::models::{CustomCondEntry, HeapAnnotationPatch, NodeStatusPatch};
use crate::kubelet::NodeIdentity;
use crate::net::client::SharedClient;
use crate::net::wifi::{WIFI_ASSOCIATIONS_TOTAL, WIFI_BSSID};
use crate::reconcilers::lease::RENEWAL_COUNT;
use crate::wallclock::unix_now_secs;

/// Sticky inter-tick state for the joke conditions. None of this needs to
/// survive a reboot — it's only used to compute deltas and detect AP flips
/// across status loop iterations.
struct LoopState {
    custom: CustomCondTracker,
    /// Snapshot of WIFI_ASSOCIATIONS_TOTAL at the previous status push;
    /// we count "reconnects in window" as `(now - prev_total)`.
    prev_assoc_total: u32,
    /// Last BSSID we saw (0 = never). Compared every push to fire Haunted.
    prev_bssid: u64,
    /// Wall-clock + monotonic anchor at the previous push, for TimeSlipped.
    prev_unix: u64,
    prev_mono_ms: u64,
    /// Rotates `STATUS_OK_LINES` so a long-lived node doesn't repeat the
    /// same success log line for hours.
    status_ok_idx: u32,
}

/// Cycled on every successful /status PATCH. Same dramatic effect as
/// "we are observably alive" but the logs don't go braindead at #500.
static STATUS_OK_LINES: &[&str] = &[
    "we are observably alive",
    "the cluster knows we exist",
    "kube-state-metrics has been informed",
    "still no notes from management",
    "free heap: marginally less",
    "our conditions are unchanged. spiritually too.",
];

#[embassy_executor::task]
pub async fn status_reconciler(
    client: &'static SharedClient,
    identity: NodeIdentity,
    tracker: NodeCondTracker,
    custom: CustomCondTracker,
) -> ! {
    let mut tracker = tracker;
    let mut last_status_update = unix_now_secs();
    let mut state = LoopState {
        custom,
        prev_assoc_total: WIFI_ASSOCIATIONS_TOTAL.load(Ordering::Acquire),
        prev_bssid: WIFI_BSSID.load(Ordering::Acquire),
        prev_unix: unix_now_secs(),
        prev_mono_ms: Instant::now().as_millis(),
        status_ok_idx: 0,
    };

    loop {
        Timer::after(Duration::from_secs(LEASE_RENEW_PERIOD_SECS)).await;

        let now = unix_now_secs();
        let now_ms = Instant::now().as_millis();
        let free = esp_alloc::HEAP.free();

        // Real conditions first.
        let mp_now = free < MEMORY_PRESSURE_FREE_BYTES;
        let mp_flipped = tracker.memory_pressure.observe(mp_now, now);

        // Sample the inputs the joke conditions are derived from.
        let assoc_total = WIFI_ASSOCIATIONS_TOTAL.load(Ordering::Acquire);
        let bssid_now = WIFI_BSSID.load(Ordering::Acquire);
        let reconnects = assoc_total.saturating_sub(state.prev_assoc_total);
        // Don't fire NewGhost on the very first sample (prev_bssid == 0) or
        // before wifi has ever associated (bssid_now == 0).
        let bssid_changed =
            state.prev_bssid != 0 && bssid_now != 0 && bssid_now != state.prev_bssid;

        // TimeSlipped: wall clock should advance at the same rate as the
        // monotonic clock between samples. Hits if someone re-anchored the
        // wall clock mid-flight (currently no one does, but the detector
        // is here for when we add periodic re-anchoring).
        let wall_delta = now as i64 - state.prev_unix as i64;
        let mono_delta = ((now_ms.saturating_sub(state.prev_mono_ms)) / 1000) as i64;
        let time_slipped = (wall_delta - mono_delta).abs() > HAUNTED_SLIP_SECS;

        let inputs = CustomCondInputs {
            heap_free_bytes: free,
            uptime_secs: Instant::now().as_secs(),
            wifi_reconnects_5min: reconnects,
            renewal_count: RENEWAL_COUNT.load(Ordering::Acquire),
            bssid_changed,
            time_slipped,
        };

        let v = eval_vibes(&inputs);
        let c = eval_caffeinated(&inputs);
        let e = eval_existential(&inputs);
        let p = eval_peckish(&inputs);
        let h = eval_haunted(&inputs);

        let v_flip = state.custom.vibes.observe(v.status, now);
        let c_flip = state.custom.caffeinated.observe(c.status, now);
        let e_flip = state.custom.existential.observe(e.status, now);
        let p_flip = state.custom.peckish.observe(p.status, now);
        let h_flip = state.custom.haunted.observe(h.status, now);

        let any_custom_flipped = v_flip || c_flip || e_flip || p_flip || h_flip;
        let due = now != 0 && now.saturating_sub(last_status_update) >= STATUS_UPDATE_PERIOD_SECS;

        if !(mp_flipped || any_custom_flipped || due) {
            continue;
        }

        let entries = [
            CustomCondEntry {
                name: "Vibes",
                current: v,
                transitioned_at: state.custom.vibes.transitioned_at,
            },
            CustomCondEntry {
                name: "Caffeinated",
                current: c,
                transitioned_at: state.custom.caffeinated.transitioned_at,
            },
            CustomCondEntry {
                name: "Existential",
                current: e,
                transitioned_at: state.custom.existential.transitioned_at,
            },
            CustomCondEntry {
                name: "Peckish",
                current: p,
                transitioned_at: state.custom.peckish.transitioned_at,
            },
            CustomCondEntry {
                name: "Haunted",
                current: h,
                transitioned_at: state.custom.haunted.transitioned_at,
            },
        ];

        let ok_line = STATUS_OK_LINES[(state.status_ok_idx as usize) % STATUS_OK_LINES.len()];
        state.status_ok_idx = state.status_ok_idx.wrapping_add(1);
        push_status(client, &identity, &tracker, &entries, now, free, ok_line).await;
        last_status_update = now;

        // Reset the windowed snapshots after a successful push so the next
        // window measures from "now". The Haunted edge-detector likewise
        // resets prev_bssid so we don't keep firing NewGhost forever after
        // a single AP change.
        state.prev_assoc_total = assoc_total;
        state.prev_bssid = bssid_now;
        state.prev_unix = now;
        state.prev_mono_ms = now_ms;
    }
}

async fn push_status(
    client: &SharedClient,
    identity: &NodeIdentity,
    tracker: &NodeCondTracker,
    custom: &[CustomCondEntry],
    now: u64,
    free: usize,
    ok_line: &'static str,
) {
    let mut body: HString<3072> = HString::new();
    if (NodeStatusPatch {
        tracker,
        custom,
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
                200 => info!("status updated ({})", ok_line),
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
