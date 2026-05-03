//! Per-condition state for a Node's status.conditions array.
//!
//! `lastHeartbeatTime` advances every status PATCH; `lastTransitionTime`
//! only advances when a condition's status field actually flips. Conflating
//! the two is a real-kubelet anti-pattern that makes nodes look flappy in
//! monitoring, so we track the "last flipped" time per-condition.

use crate::config::{
    CAFFEINATED_FRESH_SECS, HEAP_TOTAL_BYTES, MEMORY_PRESSURE_FREE_BYTES, VIBES_CURSED_RECONNECTS,
};

#[derive(Clone, Copy)]
pub struct CondState {
    /// Semantic value: for Ready, True means ready; for the *Pressure
    /// conditions, True means the node is under pressure.
    pub value: bool,
    pub transitioned_at: u64,
}

impl CondState {
    pub fn new(initial: bool, now: u64) -> Self {
        Self {
            value: initial,
            transitioned_at: now,
        }
    }

    pub fn observe(&mut self, current: bool, now: u64) -> bool {
        let flipped = current != self.value;
        if flipped {
            self.value = current;
            self.transitioned_at = now;
        }
        flipped
    }
}

pub struct NodeCondTracker {
    pub ready: CondState,
    pub memory_pressure: CondState,
    pub disk_pressure: CondState,
    pub pid_pressure: CondState,
}

impl NodeCondTracker {
    pub fn new(now: u64) -> Self {
        Self {
            ready: CondState::new(true, now),
            memory_pressure: CondState::new(false, now),
            disk_pressure: CondState::new(false, now),
            pid_pressure: CondState::new(false, now),
        }
    }
}

// ---- Custom (joke-but-honest) conditions --------------------------------
//
// Real kubelet only emits Ready / *Pressure. Kubernetes does no validation
// on `status.conditions[].type`, so we tack on a few derived-from-actual-
// state ones that show up in `kubectl describe node`. The joke is the
// reason/message strings; the inputs are real.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CondStatus {
    True,
    False,
    Unknown,
}

impl CondStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CondStatus::True => "True",
            CondStatus::False => "False",
            CondStatus::Unknown => "Unknown",
        }
    }
}

/// One evaluator output: the tri-state status plus its display strings.
/// The strings change without flipping the status (e.g. Existential goes
/// Questioning → Accepting while staying True), so they live here and not
/// in the tracker.
#[derive(Clone, Copy)]
pub struct CustomCond {
    pub status: CondStatus,
    pub reason: &'static str,
    pub message: &'static str,
}

/// Previous-state slot for one custom condition. Carries forward the last
/// `transitioned_at`; we only bump it when `status` actually flips.
#[derive(Clone, Copy)]
pub struct CustomCondState {
    pub status: CondStatus,
    pub transitioned_at: u64,
}

impl CustomCondState {
    pub fn new(now: u64) -> Self {
        Self {
            // Start as Unknown so the first real evaluation always counts as
            // a transition and stamps a real boot-time `transitioned_at`.
            status: CondStatus::Unknown,
            transitioned_at: now,
        }
    }

    /// Update with the freshly evaluated status. Returns whether the status
    /// flipped (so the caller can force an immediate PATCH instead of
    /// waiting for the periodic deadline).
    pub fn observe(&mut self, current: CondStatus, now: u64) -> bool {
        if current != self.status {
            self.status = current;
            self.transitioned_at = now;
            true
        } else {
            false
        }
    }
}

/// Inputs sampled once per status loop and fed to every evaluator. Keeping
/// this as a plain struct of primitives means the eval_* fns are pure and
/// easy to reason about; all the IO/atomics live in the reconciler.
pub struct CustomCondInputs {
    pub heap_free_bytes: usize,
    pub uptime_secs: u64,
    /// Wifi reassociations counted since the previous status push.
    pub wifi_reconnects_5min: u32,
    pub renewal_count: u32,
    /// Wifi associated to a different BSSID than the last sample.
    pub bssid_changed: bool,
    /// Wall-clock delta diverged from monotonic delta by > HAUNTED_SLIP_SECS.
    pub time_slipped: bool,
}

impl CustomCondInputs {
    /// Heap percent free, 0..=100.
    pub fn heap_pct_free(&self) -> u32 {
        let free = self.heap_free_bytes.min(HEAP_TOTAL_BYTES) as u64;
        ((free * 100) / HEAP_TOTAL_BYTES as u64) as u32
    }
}

pub struct CustomCondTracker {
    pub vibes: CustomCondState,
    pub caffeinated: CustomCondState,
    pub existential: CustomCondState,
    pub peckish: CustomCondState,
    pub haunted: CustomCondState,
}

impl CustomCondTracker {
    pub fn new(now: u64) -> Self {
        Self {
            vibes: CustomCondState::new(now),
            caffeinated: CustomCondState::new(now),
            existential: CustomCondState::new(now),
            peckish: CustomCondState::new(now),
            haunted: CustomCondState::new(now),
        }
    }
}

// Evaluators are plain functions of CustomCondInputs. Order of branches
// matters: Cursed beats everything in Vibes; ditto NewGhost over
// TimeSlipped in Haunted.

pub fn eval_vibes(i: &CustomCondInputs) -> CustomCond {
    if i.wifi_reconnects_5min >= VIBES_CURSED_RECONNECTS {
        return CustomCond {
            status: CondStatus::False,
            reason: "Cursed",
            message: "wifi keeps doing the thing",
        };
    }
    let pct = i.heap_pct_free();
    if pct < 30 {
        return CustomCond {
            status: CondStatus::False,
            reason: "Off",
            message: "heap is hostile",
        };
    }
    if pct > 70 && i.wifi_reconnects_5min == 0 && i.uptime_secs < CAFFEINATED_FRESH_SECS {
        return CustomCond {
            status: CondStatus::True,
            reason: "Immaculate",
            message: "freshly booted, heap abundant",
        };
    }
    if pct > 50 && i.wifi_reconnects_5min == 0 {
        return CustomCond {
            status: CondStatus::True,
            reason: "Cromulent",
            message: "perfectly cromulent",
        };
    }
    CustomCond {
        status: CondStatus::Unknown,
        reason: "Dissociating",
        message: "vibes indeterminate",
    }
}

pub fn eval_caffeinated(i: &CustomCondInputs) -> CustomCond {
    if i.uptime_secs < CAFFEINATED_FRESH_SECS {
        CustomCond {
            status: CondStatus::True,
            reason: "FreshlyBrewed",
            message: "fresh out of the bootloader",
        }
    } else {
        CustomCond {
            status: CondStatus::False,
            reason: "Decaf",
            message: "running on fumes and conviction",
        }
    }
}

pub fn eval_existential(i: &CustomCondInputs) -> CustomCond {
    if i.renewal_count >= 10_000 {
        CustomCond {
            status: CondStatus::True,
            reason: "Transcendent",
            message: "the lease is the self",
        }
    } else if i.renewal_count >= 1_000 {
        CustomCond {
            status: CondStatus::True,
            reason: "Accepting",
            message: "this is fine, mostly",
        }
    } else if i.renewal_count >= 100 {
        CustomCond {
            status: CondStatus::True,
            reason: "Questioning",
            message: "is this all there is",
        }
    } else {
        CustomCond {
            status: CondStatus::False,
            reason: "Innocent",
            message: "still believes in pods",
        }
    }
}

pub fn eval_peckish(i: &CustomCondInputs) -> CustomCond {
    // Below MemoryPressure's threshold the real condition is louder, so we
    // intentionally don't add a fourth "Starving" tier; Peckish saturates
    // at Hungry and lets MemoryPressure do the talking.
    if i.heap_free_bytes < MEMORY_PRESSURE_FREE_BYTES {
        return CustomCond {
            status: CondStatus::True,
            reason: "Hungry",
            message: "would eat a byte right now",
        };
    }
    let pct = i.heap_pct_free();
    if pct > 60 {
        CustomCond {
            status: CondStatus::False,
            reason: "Sated",
            message: "heap is plenty",
        }
    } else if pct > 40 {
        CustomCond {
            status: CondStatus::True,
            reason: "CouldGoForASnack",
            message: "could go for a snack",
        }
    } else {
        CustomCond {
            status: CondStatus::True,
            reason: "Hungry",
            message: "would eat a byte right now",
        }
    }
}

pub fn eval_haunted(i: &CustomCondInputs) -> CustomCond {
    if i.bssid_changed {
        CustomCond {
            status: CondStatus::True,
            reason: "NewGhost",
            message: "associated to a different bssid",
        }
    } else if i.time_slipped {
        CustomCond {
            status: CondStatus::True,
            reason: "TimeSlipped",
            message: "wall clock disagrees with monotonic",
        }
    } else {
        CustomCond {
            status: CondStatus::False,
            reason: "Calm",
            message: "no ghosts this interval",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CondState, CondStatus, CustomCondInputs, CustomCondState, eval_caffeinated,
        eval_existential, eval_haunted, eval_peckish, eval_vibes,
    };
    use crate::config::{
        CAFFEINATED_FRESH_SECS, HEAP_TOTAL_BYTES, MEMORY_PRESSURE_FREE_BYTES,
        VIBES_CURSED_RECONNECTS,
    };

    fn inputs() -> CustomCondInputs {
        CustomCondInputs {
            heap_free_bytes: HEAP_TOTAL_BYTES,
            uptime_secs: 1,
            wifi_reconnects_5min: 0,
            renewal_count: 0,
            bssid_changed: false,
            time_slipped: false,
        }
    }

    #[test]
    fn cond_state_tracks_transition_time_only_on_flips() {
        let mut state = CondState::new(false, 10);

        assert!(!state.observe(false, 20));
        assert_eq!(state.transitioned_at, 10);

        assert!(state.observe(true, 30));
        assert!(state.value);
        assert_eq!(state.transitioned_at, 30);
    }

    #[test]
    fn custom_conditions_start_unknown_so_first_real_value_forces_patch() {
        let mut state = CustomCondState::new(10);

        assert!(state.observe(CondStatus::True, 11));
        assert_eq!(state.transitioned_at, 11);

        assert!(!state.observe(CondStatus::True, 12));
        assert_eq!(state.transitioned_at, 11);
    }

    #[test]
    fn heap_percent_is_capped_at_total_heap() {
        let i = CustomCondInputs {
            heap_free_bytes: HEAP_TOTAL_BYTES * 3,
            ..inputs()
        };

        assert_eq!(i.heap_pct_free(), 100);
    }

    #[test]
    fn vibes_prefers_cursed_reconnects_over_good_heap() {
        let i = CustomCondInputs {
            wifi_reconnects_5min: VIBES_CURSED_RECONNECTS,
            ..inputs()
        };

        let cond = eval_vibes(&i);
        assert_eq!(cond.status, CondStatus::False);
        assert_eq!(cond.reason, "Cursed");
    }

    #[test]
    fn vibes_reports_immaculate_on_fresh_stable_boot() {
        let cond = eval_vibes(&inputs());

        assert_eq!(cond.status, CondStatus::True);
        assert_eq!(cond.reason, "Immaculate");
    }

    #[test]
    fn caffeinated_turns_decaf_after_fresh_window() {
        let i = CustomCondInputs {
            uptime_secs: CAFFEINATED_FRESH_SECS,
            ..inputs()
        };

        let cond = eval_caffeinated(&i);
        assert_eq!(cond.status, CondStatus::False);
        assert_eq!(cond.reason, "Decaf");
    }

    #[test]
    fn existential_thresholds_are_monotonic() {
        let innocent = eval_existential(&CustomCondInputs {
            renewal_count: 99,
            ..inputs()
        });
        let questioning = eval_existential(&CustomCondInputs {
            renewal_count: 100,
            ..inputs()
        });
        let accepting = eval_existential(&CustomCondInputs {
            renewal_count: 1_000,
            ..inputs()
        });
        let transcendent = eval_existential(&CustomCondInputs {
            renewal_count: 10_000,
            ..inputs()
        });

        assert_eq!(innocent.reason, "Innocent");
        assert_eq!(questioning.reason, "Questioning");
        assert_eq!(accepting.reason, "Accepting");
        assert_eq!(transcendent.reason, "Transcendent");
    }

    #[test]
    fn peckish_defers_to_memory_pressure_threshold() {
        let i = CustomCondInputs {
            heap_free_bytes: MEMORY_PRESSURE_FREE_BYTES - 1,
            ..inputs()
        };

        let cond = eval_peckish(&i);
        assert_eq!(cond.status, CondStatus::True);
        assert_eq!(cond.reason, "Hungry");
    }

    #[test]
    fn haunted_prefers_new_bssid_over_time_slip() {
        let i = CustomCondInputs {
            bssid_changed: true,
            time_slipped: true,
            ..inputs()
        };

        let cond = eval_haunted(&i);
        assert_eq!(cond.status, CondStatus::True);
        assert_eq!(cond.reason, "NewGhost");
    }
}
