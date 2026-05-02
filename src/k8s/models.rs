//! Wire models for the Kubernetes resources picokubelet talks about.
//!
//! Each struct owns its own JSON serialization via `write_json`. We don't
//! pull in serde — the bodies are short, fixed-shape, and the heapless
//! buffers we write into come pre-sized by the caller.

use core::fmt::Write as FmtWrite;
use core::net::Ipv4Addr;

use heapless::String as HString;

use crate::config::{LEASE_DURATION_SECS, NODE_NAME};
use crate::k8s::conditions::NodeCondTracker;
use crate::wallclock::fmt_rfc3339;

/// Wire model for the Node resource we register at boot.
pub struct NodeRegistration {
    pub ip: Ipv4Addr,
    pub now_unix: u64,
}

impl NodeRegistration {
    pub fn write_json(&self, out: &mut HString<2048>) -> Result<(), core::fmt::Error> {
        let o = self.ip.octets();
        // Stamp every condition with the current time so the controller doesn't
        // immediately mark us Unknown if the status PATCH is slow to land.
        let mut ts: HString<40> = HString::new();
        fmt_rfc3339(self.now_unix, &mut ts)?;
        write!(
            out,
            concat!(
                r#"{{"apiVersion":"v1","kind":"Node","#,
                r#""metadata":{{"#,
                r#""name":"{name}","#,
                r#""labels":{{"#,
                r#""kubernetes.io/hostname":"{name}","#,
                r#""kubernetes.io/arch":"xtensa-lx7","#,
                r#""kubernetes.io/os":"no_std","#,
                r#""node.kubernetes.io/instance-type":"esp32-s3-r8","#,
                r#""hardware":"esp32-s3","#,
                r#""arch":"xtensa-lx7","#,
                r#""node.specht.dev/cursed":"true""#,
                r#"}}"#,
                r#"}},"#,
                r#""spec":{{}},"#,
                r#""status":{{"#,
                r#""capacity":{{"cpu":"240m","memory":"320Ki","pods":"1"}},"#,
                r#""allocatable":{{"cpu":"240m","memory":"320Ki","pods":"1"}},"#,
                r#""nodeInfo":{{"#,
                r#""machineID":"picokubelet-{name}","#,
                r#""systemUUID":"00000000-0000-0000-0000-{mac:012x}","#,
                r#""bootID":"00000000-0000-0000-0000-000000000001","#,
                r#""kernelVersion":"esp-rs-no_std","#,
                r#""osImage":"picokubelet on bare metal","#,
                r#""containerRuntimeVersion":"lies://0.1.0","#,
                r#""kubeletVersion":"v1.31.1-picokubelet","#,
                r#""kubeProxyVersion":"v1.31.1-picokubelet","#,
                r#""operatingSystem":"no_std","#,
                r#""architecture":"xtensa-lx7""#,
                r#"}},"#,
                r#""addresses":[{{"type":"InternalIP","address":"{a}.{b}.{c}.{d}"}},{{"type":"Hostname","address":"{name}"}}],"#,
                r#""daemonEndpoints":{{"kubeletEndpoint":{{"Port":10250}}}},"#,
                r#""conditions":[{{"#,
                r#""type":"Ready","status":"True","reason":"KubeletReady","message":"ESP32-S3 sips electrons but is here","lastHeartbeatTime":"{ts}","lastTransitionTime":"{ts}""#,
                r#"}},{{"#,
                r#""type":"MemoryPressure","status":"False","reason":"KubeletHasSufficientMemory","message":"more than zero bytes free","lastHeartbeatTime":"{ts}","lastTransitionTime":"{ts}""#,
                r#"}},{{"#,
                r#""type":"DiskPressure","status":"False","reason":"KubeletHasNoDiskPressure","message":"there is no disk","lastHeartbeatTime":"{ts}","lastTransitionTime":"{ts}""#,
                r#"}},{{"#,
                r#""type":"PIDPressure","status":"False","reason":"KubeletHasSufficientPID","message":"PIDs are also lies","lastHeartbeatTime":"{ts}","lastTransitionTime":"{ts}""#,
                r#"}}]"#,
                r#"}}"#,
                r#"}}"#,
            ),
            name = NODE_NAME,
            a = o[0],
            b = o[1],
            c = o[2],
            d = o[3],
            mac = ((o[0] as u64) << 24)
                | ((o[1] as u64) << 16)
                | ((o[2] as u64) << 8)
                | (o[3] as u64),
            ts = ts.as_str(),
        )
    }
}

/// Wire model for a Lease create or renewal body.
pub struct LeaseBody {
    pub renew_unix: u64,
}

impl LeaseBody {
    pub fn write_json(&self, out: &mut HString<512>) -> Result<(), core::fmt::Error> {
        let mut renew: HString<40> = HString::new();
        fmt_rfc3339(self.renew_unix, &mut renew)?;
        write!(
            out,
            concat!(
                r#"{{"apiVersion":"coordination.k8s.io/v1","kind":"Lease","#,
                r#""metadata":{{"name":"{name}","namespace":"kube-node-lease"}},"#,
                r#""spec":{{"#,
                r#""holderIdentity":"{name}","#,
                r#""leaseDurationSeconds":{ldur},"#,
                r#""renewTime":"{renew}""#,
                r#"}}}}"#,
            ),
            name = NODE_NAME,
            ldur = LEASE_DURATION_SECS,
            renew = renew.as_str(),
        )
    }
}

/// Wire model for the Node status subresource PATCH (strategic merge).
pub struct NodeStatusPatch<'a> {
    pub tracker: &'a NodeCondTracker,
    pub heartbeat_unix: u64,
}

impl NodeStatusPatch<'_> {
    pub fn write_json(&self, out: &mut HString<1536>) -> Result<(), core::fmt::Error> {
        let t = self.tracker;
        let mut hb: HString<40> = HString::new();
        fmt_rfc3339(self.heartbeat_unix, &mut hb)?;
        let mut ready_t: HString<40> = HString::new();
        fmt_rfc3339(t.ready.transitioned_at, &mut ready_t)?;
        let mut mp_t: HString<40> = HString::new();
        fmt_rfc3339(t.memory_pressure.transitioned_at, &mut mp_t)?;
        let mut dp_t: HString<40> = HString::new();
        fmt_rfc3339(t.disk_pressure.transitioned_at, &mut dp_t)?;
        let mut pp_t: HString<40> = HString::new();
        fmt_rfc3339(t.pid_pressure.transitioned_at, &mut pp_t)?;

        let mp_reason = if t.memory_pressure.value {
            "KubeletHasInsufficientMemory"
        } else {
            "KubeletHasSufficientMemory"
        };
        let mp_msg = if t.memory_pressure.value {
            "free heap below threshold"
        } else {
            "more than zero bytes free"
        };

        write!(
            out,
            concat!(
                r#"{{"status":{{"conditions":["#,
                r#"{{"type":"Ready","status":"{ready}","reason":"KubeletReady","message":"ESP32-S3 sips electrons but is here","lastHeartbeatTime":"{hb}","lastTransitionTime":"{rt}"}},"#,
                r#"{{"type":"MemoryPressure","status":"{mp}","reason":"{mpr}","message":"{mpm}","lastHeartbeatTime":"{hb}","lastTransitionTime":"{mt}"}},"#,
                r#"{{"type":"DiskPressure","status":"{dp}","reason":"KubeletHasNoDiskPressure","message":"there is no disk","lastHeartbeatTime":"{hb}","lastTransitionTime":"{dt}"}},"#,
                r#"{{"type":"PIDPressure","status":"{pp}","reason":"KubeletHasSufficientPID","message":"PIDs are also lies","lastHeartbeatTime":"{hb}","lastTransitionTime":"{pt}"}}"#,
                r#"]}}}}"#,
            ),
            ready = if t.ready.value { "True" } else { "False" },
            mp = if t.memory_pressure.value {
                "True"
            } else {
                "False"
            },
            dp = if t.disk_pressure.value {
                "True"
            } else {
                "False"
            },
            pp = if t.pid_pressure.value {
                "True"
            } else {
                "False"
            },
            mpr = mp_reason,
            mpm = mp_msg,
            hb = hb.as_str(),
            rt = ready_t.as_str(),
            mt = mp_t.as_str(),
            dt = dp_t.as_str(),
            pt = pp_t.as_str(),
        )
    }
}

/// Wire model for the heap-free annotation PATCH (regular merge — the
/// /status subresource silently drops metadata).
pub struct HeapAnnotationPatch {
    pub free_bytes: usize,
}

impl HeapAnnotationPatch {
    pub fn write_json(&self, out: &mut HString<256>) -> Result<(), core::fmt::Error> {
        write!(
            out,
            r#"{{"metadata":{{"annotations":{{"node.specht.dev/heap-bytes-free":"{}"}}}}}}"#,
            self.free_bytes,
        )
    }
}
