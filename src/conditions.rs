//! Per-condition state for a Node's status.conditions array.
//!
//! `lastHeartbeatTime` advances every status PATCH; `lastTransitionTime`
//! only advances when a condition's status field actually flips. Conflating
//! the two is a real-kubelet anti-pattern that makes nodes look flappy in
//! monitoring, so we track the "last flipped" time per-condition.

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
