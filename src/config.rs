// ---- compile-time config from .env (loaded by mise) ----------------------
pub const K3S_API_HOST: &str = env!("K3S_API_HOST");
pub const K3S_API_PORT_STR: &str = env!("K3S_API_PORT");
pub const K3S_TOKEN: &str = env!("K3S_TOKEN");
pub const WIFI_SSID: &str = env!("WIFI_SSID");
pub const WIFI_PSK: &str = env!("WIFI_PSK");

// Node identity from .env (NODE_NAME), with a friendly default so the
// build doesn't break if it's missing. Each board in the rack should get
// its own. (German nodes get German names — once the Tamagotchi rack
// ships there'll be at least one Günther / Heinrich / Brigitte each.)
//
// Kubernetes node names must match [a-z0-9.-]+, so umlauts get
// transliterated: ü → ue, ö → oe, ß → ss.
pub const NODE_NAME: &str = match option_env!("NODE_NAME") {
    Some(s) => s,
    None => "esp-node-01-guenther",
};
pub const LEASE_DURATION_SECS: u32 = 40;
pub const LEASE_RENEW_PERIOD_SECS: u64 = 10;

// Status subresource updates are the *slow* heartbeat. Real kubelets default
// to 5 min (or sooner on change). The lease covers the fast path; status
// PATCHes refresh `lastHeartbeatTime` so observers like kube-state-metrics
// see a healthy node.
pub const STATUS_UPDATE_PERIOD_SECS: u64 = 300;

// Real kubelet flips MemoryPressure=True at <100Mi free. Scaled to ESP heap:
// True when free heap drops below this. With ~100KB total heap, 20KB is the
// "you're about to OOM" line.
pub const MEMORY_PRESSURE_FREE_BYTES: usize = 20 * 1024;
