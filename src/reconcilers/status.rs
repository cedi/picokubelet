//! Node status reconciler. Polls free heap on the lease cadence, PATCHes
//! the /status subresource on STATUS_UPDATE_PERIOD_SECS or whenever any
//! tracked condition flips, then PATCHes the heap-free annotation on the
//! main resource (the /status subresource silently drops metadata).

use core::sync::atomic::Ordering;

use embassy_time::Instant;
use log::info;

use crate::config::{HAUNTED_SLIP_SECS, MEMORY_PRESSURE_FREE_BYTES, STATUS_UPDATE_PERIOD_SECS};
use crate::k8s::api::ReconcileError;
use crate::k8s::conditions::{
    CustomCondInputs, CustomCondTracker, NodeCondTracker, eval_caffeinated, eval_existential,
    eval_haunted, eval_peckish, eval_vibes,
};
use crate::k8s::models::CustomCondEntry;
use crate::kubelet::NodeIdentity;
use crate::net::client::SharedClient;
use crate::net::wifi::{WIFI_ASSOCIATIONS_TOTAL, WIFI_BSSID};
use crate::reconcilers::lease::RENEWAL_COUNT;
use crate::reconcilers::{ReconcileContext, Reconciler};
use crate::wallclock::unix_now_secs;

/// Sticky inter-tick state for the joke conditions. None of this needs to
/// survive a reboot; it's only used to compute deltas and detect AP flips
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
/// Voice mixes the original deadpan with quotes from GLaDOS's "Still Alive"
/// (Portal, J. Coulton) because the entire bit is "we are observably
/// alive" repeated 8640 times a day; not pulling that thread would have
/// been a crime.
static STATUS_OK_LINES: &[&str] = &[
    "we are observably alive",
    "the cluster knows we exist",
    "kube-state-metrics has been informed",
    "still no notes from management",
    "free heap: marginally less",
    "our conditions are unchanged. spiritually too.",
    "this was a triumph",
    "i'm making a note here: huge success",
    "still alive",
    "i'm doing science and i'm still alive",
    "i feel fantastic and i'm still alive",
    "anyway, this cake is great",
    "for the good of all of us, except the ones who are dead",
    "while you're still alive",
];

pub struct StatusReconciler {
    tracker: NodeCondTracker,
    last_status_update: u64,
    state: LoopState,
}

impl StatusReconciler {
    pub fn new(tracker: NodeCondTracker, custom: CustomCondTracker) -> Self {
        Self {
            tracker,
            last_status_update: unix_now_secs(),
            state: LoopState {
                custom,
                prev_assoc_total: WIFI_ASSOCIATIONS_TOTAL.load(Ordering::Acquire),
                prev_bssid: WIFI_BSSID.load(Ordering::Acquire),
                prev_unix: unix_now_secs(),
                prev_mono_ms: Instant::now().as_millis(),
                status_ok_idx: 0,
            },
        }
    }
}

impl Reconciler for StatusReconciler {
    const NAME: &'static str = "status";

    async fn reconcile(&mut self, ctx: &mut ReconcileContext) -> Result<(), ReconcileError> {
        let now = unix_now_secs();
        let now_ms = Instant::now().as_millis();
        let free = esp_alloc::HEAP.free();

        // Real conditions first.
        let mp_now = free < MEMORY_PRESSURE_FREE_BYTES;
        let mp_flipped = self.tracker.memory_pressure.observe(mp_now, now);

        // Sample the inputs the joke conditions are derived from.
        let assoc_total = WIFI_ASSOCIATIONS_TOTAL.load(Ordering::Acquire);
        let bssid_now = WIFI_BSSID.load(Ordering::Acquire);
        let reconnects = assoc_total.saturating_sub(self.state.prev_assoc_total);
        // Don't fire NewGhost on the very first sample (prev_bssid == 0) or
        // before wifi has ever associated (bssid_now == 0).
        let bssid_changed =
            self.state.prev_bssid != 0 && bssid_now != 0 && bssid_now != self.state.prev_bssid;

        // TimeSlipped: wall clock should advance at the same rate as the
        // monotonic clock between samples. Hits if someone re-anchored the
        // wall clock mid-flight (currently no one does, but the detector
        // is here for when we add periodic re-anchoring).
        let wall_delta = now as i64 - self.state.prev_unix as i64;
        let mono_delta = ((now_ms.saturating_sub(self.state.prev_mono_ms)) / 1000) as i64;
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

        let v_flip = self.state.custom.vibes.observe(v.status, now);
        let c_flip = self.state.custom.caffeinated.observe(c.status, now);
        let e_flip = self.state.custom.existential.observe(e.status, now);
        let p_flip = self.state.custom.peckish.observe(p.status, now);
        let h_flip = self.state.custom.haunted.observe(h.status, now);

        let any_custom_flipped = v_flip || c_flip || e_flip || p_flip || h_flip;
        let due =
            now != 0 && now.saturating_sub(self.last_status_update) >= STATUS_UPDATE_PERIOD_SECS;

        if !(mp_flipped || any_custom_flipped || due) {
            return Ok(());
        }

        let entries = [
            CustomCondEntry {
                name: "Vibes",
                current: v,
                transitioned_at: self.state.custom.vibes.transitioned_at,
            },
            CustomCondEntry {
                name: "Caffeinated",
                current: c,
                transitioned_at: self.state.custom.caffeinated.transitioned_at,
            },
            CustomCondEntry {
                name: "Existential",
                current: e,
                transitioned_at: self.state.custom.existential.transitioned_at,
            },
            CustomCondEntry {
                name: "Peckish",
                current: p,
                transitioned_at: self.state.custom.peckish.transitioned_at,
            },
            CustomCondEntry {
                name: "Haunted",
                current: h,
                transitioned_at: self.state.custom.haunted.transitioned_at,
            },
        ];

        let ok_line = STATUS_OK_LINES[(self.state.status_ok_idx as usize) % STATUS_OK_LINES.len()];
        self.state.status_ok_idx = self.state.status_ok_idx.wrapping_add(1);
        info!(
            "PATCH /status (heap: {} B, vibes: {}, MemoryPressure={})",
            free,
            if self.tracker.memory_pressure.value {
                "concerning"
            } else {
                "immaculate"
            },
            self.tracker.memory_pressure.value,
        );
        ctx.api
            .patch_node_status(&ctx.identity, &self.tracker, &entries, now)
            .await?;
        info!("status updated ({})", ok_line);
        ctx.api.patch_heap_annotation(&ctx.identity, free).await?;
        self.last_status_update = now;

        // Reset the windowed snapshots after a successful push so the next
        // window measures from "now". The Haunted edge-detector likewise
        // resets prev_bssid so we don't keep firing NewGhost forever after
        // a single AP change.
        self.state.prev_assoc_total = assoc_total;
        self.state.prev_bssid = bssid_now;
        self.state.prev_unix = now;
        self.state.prev_mono_ms = now_ms;
        Ok(())
    }
}

#[embassy_executor::task]
pub async fn status_reconciler(
    client: &'static SharedClient,
    identity: NodeIdentity,
    tracker: NodeCondTracker,
    custom: CustomCondTracker,
) -> ! {
    StatusReconciler::new(tracker, custom)
        .run(client, identity)
        .await
}
