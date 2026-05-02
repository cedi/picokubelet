//! Identity + bootstrap. The four steps that turn a fresh ESP32-S3 into
//! something the API server thinks is a node:
//!  1. anchor wall clock from the /version Date header,
//!  2. POST /api/v1/nodes,
//!  3. POST initial Lease,
//!  4. PATCH initial status so every condition has a fresh
//!     lastHeartbeatTime out of the gate.

use core::fmt::Write as FmtWrite;
use core::net::Ipv4Addr;

use embassy_time::Instant;
use heapless::String as HString;
use log::{info, warn};

use crate::config::{MEMORY_PRESSURE_FREE_BYTES, NODE_NAME};
use crate::k8s::conditions::{
    CustomCondInputs, CustomCondTracker, NodeCondTracker, eval_caffeinated, eval_existential,
    eval_haunted, eval_peckish, eval_vibes,
};
use crate::k8s::models::{CustomCondEntry, HeapAnnotationPatch, LeaseBody, NodeRegistration, NodeStatusPatch};
use crate::net::client::SharedClient;
use crate::wallclock::{parse_http_date, set_wall_clock, unix_now_secs};

#[derive(Clone)]
pub struct NodeIdentity {
    pub name: &'static str,
    pub ip: Ipv4Addr,
    pub lease_path: HString<128>,
    pub status_path: HString<128>,
    pub node_path: HString<128>,
}

impl NodeIdentity {
    pub fn new(ip: Ipv4Addr) -> Self {
        let name = NODE_NAME;
        let mut lease_path: HString<128> = HString::new();
        write!(
            &mut lease_path,
            "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/{}",
            name,
        )
        .unwrap();
        let mut status_path: HString<128> = HString::new();
        write!(&mut status_path, "/api/v1/nodes/{}/status", name).unwrap();
        let mut node_path: HString<128> = HString::new();
        write!(&mut node_path, "/api/v1/nodes/{}", name).unwrap();
        Self {
            name,
            ip,
            lease_path,
            status_path,
            node_path,
        }
    }
}

pub async fn bootstrap(
    client: &SharedClient,
    identity: &NodeIdentity,
) -> (NodeCondTracker, CustomCondTracker) {
    anchor_clock(client).await;
    register_node(client, identity).await;
    create_lease(client).await;
    let now = unix_now_secs();
    let mut tracker = NodeCondTracker::new(now);
    let mut custom = CustomCondTracker::new(now);
    push_initial_status(client, identity, &mut tracker, &mut custom).await;
    (tracker, custom)
}

async fn anchor_clock(client: &SharedClient) {
    info!("anchoring wall clock from k3s (we have no RTC, only vibes)");
    let mut c = client.lock().await;
    match c.get("/version").await {
        Ok(resp) => {
            info!("k3s version probe: HTTP {} (server lives)", resp.status);
            if let Some(date) = resp.header("Date") {
                if let Some(unix) = parse_http_date(date) {
                    set_wall_clock(unix);
                    info!(
                        "wall clock anchored: {} unix ({}) — time exists now",
                        unix, date
                    );
                } else {
                    warn!("could not parse Date header: {}", date);
                }
            } else {
                warn!("no Date header in /version response");
            }
        }
        Err(e) => {
            warn!("failed to anchor clock, will retry: {:?}", e);
            // Bail to the renewal loop anyway; we'll try again there.
        }
    }
}

async fn register_node(client: &SharedClient, identity: &NodeIdentity) {
    let mut node_body: HString<2048> = HString::new();
    if (NodeRegistration {
        ip: identity.ip,
        now_unix: unix_now_secs(),
    })
    .write_json(&mut node_body)
    .is_err()
    {
        warn!("node body build failed");
        return;
    }
    info!(
        "POST /api/v1/nodes (body: {} bytes — a bold introduction)",
        node_body.len()
    );

    let mut c = client.lock().await;
    match c.post("/api/v1/nodes", node_body.as_bytes()).await {
        Ok(resp) => match resp.status {
            201 => info!("node registered ({}) — control plane has accepted the bit", identity.name),
            409 => info!("node already exists, that's fine"),
            other => warn!(
                "unexpected status {} on Node POST: {}",
                other,
                core::str::from_utf8(resp.body).unwrap_or("<non-utf8>"),
            ),
        },
        Err(e) => warn!("node POST failed: {:?}", e),
    }
}

async fn create_lease(client: &SharedClient) {
    let mut lease_body: HString<512> = HString::new();
    if (LeaseBody {
        renew_unix: unix_now_secs(),
    })
    .write_json(&mut lease_body)
    .is_err()
    {
        warn!("lease body build failed");
        return;
    }
    info!("POST .../leases (initial — the contract)");

    let mut c = client.lock().await;
    match c
        .post(
            "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases",
            lease_body.as_bytes(),
        )
        .await
    {
        Ok(resp) => match resp.status {
            201 => info!("lease created (we are now legally a node)"),
            409 => info!("lease already exists, will renew via PUT"),
            other => warn!("unexpected status {} on Lease POST", other),
        },
        Err(e) => warn!("lease POST failed: {:?}", e),
    }
}

async fn push_initial_status(
    client: &SharedClient,
    identity: &NodeIdentity,
    tracker: &mut NodeCondTracker,
    custom: &mut CustomCondTracker,
) {
    let now = unix_now_secs();
    let free = esp_alloc::HEAP.free();
    tracker
        .memory_pressure
        .observe(free < MEMORY_PRESSURE_FREE_BYTES, now);

    // First-shot: no prior wifi window, no prior renewals; we still want
    // every condition stamped with a fresh transition time so kubectl
    // doesn't show 0001-01-01.
    let inputs = CustomCondInputs {
        heap_free_bytes: free,
        uptime_secs: Instant::now().as_secs(),
        wifi_reconnects_5min: 0,
        renewal_count: 0,
        bssid_changed: false,
        time_slipped: false,
    };
    let v = eval_vibes(&inputs);
    let c = eval_caffeinated(&inputs);
    let e = eval_existential(&inputs);
    let p = eval_peckish(&inputs);
    let h = eval_haunted(&inputs);
    custom.vibes.observe(v.status, now);
    custom.caffeinated.observe(c.status, now);
    custom.existential.observe(e.status, now);
    custom.peckish.observe(p.status, now);
    custom.haunted.observe(h.status, now);

    let entries = [
        CustomCondEntry {
            name: "Vibes",
            current: v,
            transitioned_at: custom.vibes.transitioned_at,
        },
        CustomCondEntry {
            name: "Caffeinated",
            current: c,
            transitioned_at: custom.caffeinated.transitioned_at,
        },
        CustomCondEntry {
            name: "Existential",
            current: e,
            transitioned_at: custom.existential.transitioned_at,
        },
        CustomCondEntry {
            name: "Peckish",
            current: p,
            transitioned_at: custom.peckish.transitioned_at,
        },
        CustomCondEntry {
            name: "Haunted",
            current: h,
            transitioned_at: custom.haunted.transitioned_at,
        },
    ];

    let mut body: HString<3072> = HString::new();
    if (NodeStatusPatch {
        tracker,
        custom: &entries,
        heartbeat_unix: now,
    })
    .write_json(&mut body)
    .is_err()
    {
        warn!("status body build failed");
        return;
    }
    info!(
        "PATCH /status (heap: {} B, vibes: {}, MemoryPressure={}) [initial]",
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
