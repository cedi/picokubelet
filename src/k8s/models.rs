//! Wire models for the Kubernetes resources picokubelet talks about.
//!
//! Each struct owns its own JSON serialization via `write_json`. We don't
//! pull in serde; the bodies are short, fixed-shape, and the heapless
//! buffers we write into come pre-sized by the caller.

use core::fmt::Write as FmtWrite;
use core::net::Ipv4Addr;

use heapless::String as HString;

use crate::config::{LEASE_DURATION_SECS, NODE_NAME};
use crate::k8s::conditions::{CustomCond, NodeCondTracker};
use crate::wallclock::fmt_rfc3339;

/// One custom condition row, ready to serialize: name + freshly evaluated
/// status/reason/message + the previous transition timestamp (carried
/// forward by the tracker; only the reconciler knows when it last
/// flipped).
pub struct CustomCondEntry {
    pub name: &'static str,
    pub current: CustomCond,
    pub transitioned_at: u64,
}

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
///
/// `custom` is appended *after* the four real conditions. Strategic merge
/// uses `type` as the merge key for status.conditions, so any custom type
/// the API server already has from a previous PATCH gets updated in place;
/// dropping a custom condition from this slice does NOT remove it from the
/// stored object (would need an explicit JSON-patch remove for that, which
/// we don't bother with (the set is fixed).
pub struct NodeStatusPatch<'a> {
    pub tracker: &'a NodeCondTracker,
    pub custom: &'a [CustomCondEntry],
    pub heartbeat_unix: u64,
}

impl NodeStatusPatch<'_> {
    pub fn write_json(&self, out: &mut HString<3072>) -> Result<(), core::fmt::Error> {
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
        )?;

        for entry in self.custom {
            let mut tt: HString<40> = HString::new();
            fmt_rfc3339(entry.transitioned_at, &mut tt)?;
            write!(
                out,
                r#",{{"type":"{name}","status":"{status}","reason":"{reason}","message":"{message}","lastHeartbeatTime":"{hb}","lastTransitionTime":"{tt}"}}"#,
                name = entry.name,
                status = entry.current.status.as_str(),
                reason = entry.current.reason,
                message = entry.current.message,
                hb = hb.as_str(),
                tt = tt.as_str(),
            )?;
        }

        out.push_str("]}}").map_err(|_| core::fmt::Error)
    }
}

/// Wire model for the heap-free annotation PATCH (regular merge; the
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

#[cfg(test)]
mod tests {
    use core::net::Ipv4Addr;

    use heapless::String as HString;

    use super::{CustomCondEntry, HeapAnnotationPatch, LeaseBody, NodeRegistration, NodeStatusPatch};
    use crate::config::{LEASE_DURATION_SECS, NODE_NAME};
    use crate::k8s::conditions::{CondStatus, CustomCond, NodeCondTracker};

    #[test]
    fn lease_body_contains_name_namespace_duration_and_renew_time() {
        let mut out: HString<512> = HString::new();

        (LeaseBody {
            renew_unix: 1_777_736_837,
        })
        .write_json(&mut out)
        .expect("lease body fits");

        assert!(out.contains(r#""kind":"Lease""#));
        assert!(out.contains(r#""namespace":"kube-node-lease""#));
        assert!(out.contains(NODE_NAME));
        assert!(out.contains(r#""leaseDurationSeconds":40"#));
        assert!(out.contains(r#""renewTime":"2026-05-02T15:47:17.000000Z""#));
        assert_eq!(LEASE_DURATION_SECS, 40);
    }

    #[test]
    fn status_patch_writes_custom_conditions_after_builtin_conditions() {
        let tracker = NodeCondTracker::new(1_777_736_800);
        let custom = [CustomCondEntry {
            name: "Vibes",
            current: CustomCond {
                status: CondStatus::True,
                reason: "Immaculate",
                message: "freshly booted, heap abundant",
            },
            transitioned_at: 1_777_736_837,
        }];
        let mut out: HString<3072> = HString::new();

        (NodeStatusPatch {
            tracker: &tracker,
            custom: &custom,
            heartbeat_unix: 1_777_736_837,
        })
        .write_json(&mut out)
        .expect("status body fits");

        let ready = out.find(r#""type":"Ready""#).expect("Ready condition");
        let vibes = out.find(r#""type":"Vibes""#).expect("Vibes condition");

        assert!(ready < vibes);
        assert!(out.contains(r#""lastHeartbeatTime":"2026-05-02T15:47:17.000000Z""#));
        assert!(out.contains(r#""reason":"Immaculate""#));
    }

    #[test]
    fn heap_annotation_serializes_free_bytes_as_string_annotation() {
        let mut out: HString<256> = HString::new();

        (HeapAnnotationPatch { free_bytes: 12345 })
            .write_json(&mut out)
            .expect("annotation body fits");

        assert_eq!(
            out.as_str(),
            r#"{"metadata":{"annotations":{"node.specht.dev/heap-bytes-free":"12345"}}}"#,
        );
    }

    #[test]
    fn node_registration_uses_supplied_ip_as_internal_address() {
        let mut out: HString<2048> = HString::new();

        (NodeRegistration {
            ip: Ipv4Addr::new(10, 42, 0, 7),
            now_unix: 1_777_736_837,
        })
        .write_json(&mut out)
        .expect("node registration fits");

        assert!(out.contains(r#""type":"InternalIP","address":"10.42.0.7""#));
        assert!(out.contains(r#""kubernetes.io/hostname""#));
    }
}
