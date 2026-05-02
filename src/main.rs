//! picokubelet — phase 4: register, stay Ready, and report honest health.
//!
//! After Wi-Fi + DHCP + TLS to k3s:
//!  1. GET /version once to anchor a wall clock (parsed from the Date header
//!     because we don't have an RTC).
//!  2. POST /api/v1/nodes with our Node spec — lying about CPU/memory.
//!  3. POST a Lease into kube-node-lease.
//!  4. PATCH /nodes/{name}/status with fresh heartbeats + a heap-derived
//!     MemoryPressure; PATCH /nodes/{name} with a free-heap annotation.
//!  5. Loop forever: lease PATCH every 10s, status + annotation PATCH every
//!     5 min (or whenever a condition flips).
//!
//! The lease keeps the Node Lifecycle Controller off our back on a 40s
//! cadence; the status PATCH keeps every condition's lastHeartbeatTime
//! fresh so kube-state-metrics and other second-order observers see a
//! healthy node, not a node whose conditions are all 0001-01-01.
//!
//! Cert verification is still off — fine for the home lab, will become a
//! real CA + client cert in a later phase.

#![no_std]
#![no_main]

mod conditions;
mod config;
mod led;
mod net;
mod wallclock;

use core::fmt::Write as FmtWrite;
use core::net::Ipv4Addr;

use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, Ipv4Address, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, ram, rng::Rng,
    timer::timg::TimerGroup,
};
use esp_radio::wifi::{Config as WifiConfig, ControllerConfig, sta::StationConfig};
use heapless::String as HString;
use log::{info, warn};
use static_cell::StaticCell;

use crate::conditions::NodeCondTracker;
use crate::config::{
    K3S_API_HOST, K3S_API_PORT_STR, LEASE_DURATION_SECS, LEASE_RENEW_PERIOD_SECS,
    MEMORY_PRESSURE_FREE_BYTES, NODE_NAME, STATUS_UPDATE_PERIOD_SECS, WIFI_PSK, WIFI_SSID,
};
use crate::net::http::k8s_request;
use crate::net::wifi::{connection_task, net_task};
use crate::wallclock::{fmt_rfc3339, parse_http_date, set_wall_clock, unix_now_secs};

// Required by espflash: embeds an app descriptor (version, name, build date).
esp_bootloader_esp_idf::esp_app_desc!();

// ---- main ----------------------------------------------------------------
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_println::logger::init_logger_from_env();

    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    // LED first, before logs and network. The self-test runs on its own
    // and confirms the WS2812 hardware is alive even if everything else
    // fails. The task defaults to Booting after self-test.
    spawner.spawn(led::led_task(peripherals.RMT, peripherals.GPIO21).unwrap());

    info!("picokubelet booting on ESP32-S3");
    info!("identity: {}", NODE_NAME);
    info!(
        "target k3s api server: {}:{}",
        K3S_API_HOST, K3S_API_PORT_STR
    );
    info!("joining SSID: {}", WIFI_SSID);

    led::set(led::LedPattern::Connecting);

    let station_config = WifiConfig::Station(
        StationConfig::default()
            .with_ssid(WIFI_SSID)
            .with_password(WIFI_PSK.into()),
    );
    let (controller, interfaces) = esp_radio::wifi::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_config),
    )
    .expect("esp_radio::wifi::new failed");

    let net_config = NetConfig::dhcpv4(Default::default());
    static NET_RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();
    let net_resources = NET_RESOURCES.init(StackResources::<4>::new());

    let rng = Rng::new();
    let seed = ((rng.random() as u64) << 32) | rng.random() as u64;

    let (stack, net_runner) = embassy_net::new(interfaces.station, net_config, net_resources, seed);

    spawner.spawn(connection_task(controller).unwrap());
    spawner.spawn(net_task(net_runner).unwrap());

    info!("waiting for DHCP lease...");
    stack.wait_config_up().await;
    let cfg = stack
        .config_v4()
        .expect("ipv4 config missing after wait_config_up");
    let my_ip: Ipv4Addr = cfg.address.address();
    info!("DHCP up: ip={} gw={:?}", cfg.address, cfg.gateway);

    // Resolve k3s endpoint.
    let port: u16 = K3S_API_PORT_STR.parse().expect("K3S_API_PORT must be u16");
    let api_host: Ipv4Addr = K3S_API_HOST
        .parse()
        .expect("K3S_API_HOST must be IPv4 dotted-quad");
    let api_octets = api_host.octets();
    let api_smol = Ipv4Address::new(api_octets[0], api_octets[1], api_octets[2], api_octets[3]);

    // TLS record buffers — static so the task arena stays small.
    const TLS_BUF: usize = 16 * 1024 + 256;
    static TLS_READ_BUF: StaticCell<[u8; TLS_BUF]> = StaticCell::new();
    static TLS_WRITE_BUF: StaticCell<[u8; TLS_BUF]> = StaticCell::new();
    let tls_read = TLS_READ_BUF.init([0u8; TLS_BUF]);
    let tls_write = TLS_WRITE_BUF.init([0u8; TLS_BUF]);

    // Response scratch — the Node create reply can be ~6 KB.
    static RESP_BUF: StaticCell<[u8; 8192]> = StaticCell::new();
    let resp_buf = RESP_BUF.init([0u8; 8192]);

    // 1) GET /version → anchor wall clock.
    info!("anchoring wall clock from k3s server time");
    match k8s_request(
        stack, api_smol, port, rng, tls_read, tls_write, "GET", "/version", None, resp_buf,
    )
    .await
    {
        Ok(resp) => {
            info!("k3s version probe: HTTP {}", resp.status);
            if let Some(date) = resp.header("Date") {
                if let Some(unix) = parse_http_date(date) {
                    set_wall_clock(unix);
                    info!("wall clock anchored: {} unix ({})", unix, date);
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

    // 2) POST /api/v1/nodes — register ourselves.
    let mut node_body: HString<2048> = HString::new();
    build_node_body(&mut node_body, my_ip, unix_now_secs()).expect("node body build");
    info!("POST /api/v1/nodes (body: {} bytes)", node_body.len());
    match k8s_request(
        stack,
        api_smol,
        port,
        rng,
        tls_read,
        tls_write,
        "POST",
        "/api/v1/nodes",
        Some(node_body.as_bytes()),
        resp_buf,
    )
    .await
    {
        Ok(resp) => match resp.status {
            201 => info!("node registered ({})", NODE_NAME),
            409 => info!("node already exists, that's fine"),
            other => warn!(
                "unexpected status {} on Node POST: {}",
                other,
                core::str::from_utf8(resp.body).unwrap_or("<non-utf8>"),
            ),
        },
        Err(e) => warn!("node POST failed: {:?}", e),
    }

    // 3) POST initial Lease. We use the lease v1 schema. The lease lives in
    //    `kube-node-lease` namespace and is named after the node.
    let mut lease_body: HString<512> = HString::new();
    build_lease_body(&mut lease_body, unix_now_secs()).expect("lease body build");
    info!("POST .../leases (initial)");
    match k8s_request(
        stack,
        api_smol,
        port,
        rng,
        tls_read,
        tls_write,
        "POST",
        "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases",
        Some(lease_body.as_bytes()),
        resp_buf,
    )
    .await
    {
        Ok(resp) => match resp.status {
            201 => info!("lease created"),
            409 => info!("lease already exists, will renew via PUT"),
            other => warn!("unexpected status {} on Lease POST", other),
        },
        Err(e) => warn!("lease POST failed: {:?}", e),
    }

    // 4) Initial status PATCH so every condition has a fresh
    //    lastHeartbeatTime out of the gate. Without this, observers that
    //    look past `Ready` (kube-state-metrics, well-written admission
    //    controllers) see conditions stamped 0001-01-01 and treat the node
    //    as half-broken even when the lease is current.
    let lease_path: HString<128> = {
        let mut s: HString<128> = HString::new();
        write!(
            &mut s,
            "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/{}",
            NODE_NAME,
        )
        .unwrap();
        s
    };
    let status_path: HString<128> = {
        let mut s: HString<128> = HString::new();
        write!(&mut s, "/api/v1/nodes/{}/status", NODE_NAME).unwrap();
        s
    };
    let node_path: HString<128> = {
        let mut s: HString<128> = HString::new();
        write!(&mut s, "/api/v1/nodes/{}", NODE_NAME).unwrap();
        s
    };

    let mut tracker = NodeCondTracker::new(unix_now_secs());
    push_status(
        stack,
        api_smol,
        port,
        rng,
        tls_read,
        tls_write,
        &status_path,
        &node_path,
        &mut tracker,
        resp_buf,
    )
    .await;
    let mut last_status_update = unix_now_secs();

    // 5) Lease renewal loop. PATCH every LEASE_RENEW_PERIOD_SECS with a
    //    fresh renewTime; PATCH the status subresource on the slower
    //    STATUS_UPDATE_PERIOD_SECS cadence (or whenever a condition flips).
    info!(
        "entering renewal loop (lease {}s, status {}s)",
        LEASE_RENEW_PERIOD_SECS, STATUS_UPDATE_PERIOD_SECS,
    );
    let mut healthy = false;
    loop {
        Timer::after(Duration::from_secs(LEASE_RENEW_PERIOD_SECS)).await;

        let mut body: HString<512> = HString::new();
        if build_lease_body(&mut body, unix_now_secs()).is_err() {
            warn!("lease body build failed (clock not anchored?)");
            continue;
        }

        match k8s_request(
            stack,
            api_smol,
            port,
            rng,
            tls_read,
            tls_write,
            "PATCH",
            &lease_path,
            Some(body.as_bytes()),
            resp_buf,
        )
        .await
        {
            Ok(resp) => match resp.status {
                200 => {
                    info!("lease renewed");
                    if !healthy {
                        led::set(led::LedPattern::Healthy);
                        healthy = true;
                    } else {
                        led::set(led::LedPattern::Activity);
                    }
                }
                404 => {
                    warn!("lease vanished, recreating");
                    let _ = k8s_request(
                        stack,
                        api_smol,
                        port,
                        rng,
                        tls_read,
                        tls_write,
                        "POST",
                        "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases",
                        Some(body.as_bytes()),
                        resp_buf,
                    )
                    .await;
                }
                other => warn!("lease renewal returned {}", other),
            },
            Err(e) => warn!("lease renewal failed: {:?}", e),
        }

        // Status patch — slow cadence, or immediately on a condition flip.
        let now = unix_now_secs();
        let free = esp_alloc::HEAP.free();
        let mp_now = free < MEMORY_PRESSURE_FREE_BYTES;
        let flipped = tracker.memory_pressure.observe(mp_now, now);
        let due = now != 0 && now.saturating_sub(last_status_update) >= STATUS_UPDATE_PERIOD_SECS;
        if flipped || due {
            push_status(
                stack,
                api_smol,
                port,
                rng,
                tls_read,
                tls_write,
                &status_path,
                &node_path,
                &mut tracker,
                resp_buf,
            )
            .await;
            last_status_update = now;
        }
    }
}

/// Send the two status-side PATCHes: conditions on /status (strategic
/// merge), heap-free annotation on the main resource (regular merge —
/// the /status subresource silently drops metadata).
#[allow(clippy::too_many_arguments)]
async fn push_status(
    stack: Stack<'static>,
    api_ip: Ipv4Address,
    api_port: u16,
    rng: Rng,
    tls_read: &mut [u8],
    tls_write: &mut [u8],
    status_path: &str,
    node_path: &str,
    tracker: &mut NodeCondTracker,
    resp_buf: &mut [u8],
) {
    let now = unix_now_secs();
    let free = esp_alloc::HEAP.free();
    tracker
        .memory_pressure
        .observe(free < MEMORY_PRESSURE_FREE_BYTES, now);

    let mut body: HString<1536> = HString::new();
    if build_status_body(&mut body, tracker, now).is_err() {
        warn!("status body build failed");
        return;
    }
    info!(
        "PATCH {} (free heap {} B, MemoryPressure={})",
        status_path, free, tracker.memory_pressure.value,
    );
    match k8s_request(
        stack,
        api_ip,
        api_port,
        rng,
        tls_read,
        tls_write,
        "PATCH-STRATEGIC",
        status_path,
        Some(body.as_bytes()),
        resp_buf,
    )
    .await
    {
        Ok(resp) => match resp.status {
            200 => info!("status updated"),
            other => warn!(
                "status PATCH returned {}: {}",
                other,
                core::str::from_utf8(resp.body).unwrap_or("<non-utf8>"),
            ),
        },
        Err(e) => warn!("status PATCH failed: {:?}", e),
    }

    let mut ann: HString<256> = HString::new();
    if build_annotation_body(&mut ann, free).is_err() {
        return;
    }
    match k8s_request(
        stack,
        api_ip,
        api_port,
        rng,
        tls_read,
        tls_write,
        "PATCH",
        node_path,
        Some(ann.as_bytes()),
        resp_buf,
    )
    .await
    {
        Ok(resp) if resp.status == 200 => {}
        Ok(resp) => warn!("annotation PATCH returned {}", resp.status),
        Err(e) => warn!("annotation PATCH failed: {:?}", e),
    }
}

// ---- JSON body builders --------------------------------------------------

fn build_node_body(
    out: &mut HString<2048>,
    ip: Ipv4Addr,
    now_unix: u64,
) -> Result<(), core::fmt::Error> {
    let o = ip.octets();
    // Stamp every condition with the current time so the controller doesn't
    // immediately mark us Unknown if the status PATCH is slow to land.
    let mut ts: HString<40> = HString::new();
    fmt_rfc3339(now_unix, &mut ts)?;
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
        mac = ((o[0] as u64) << 24) | ((o[1] as u64) << 16) | ((o[2] as u64) << 8) | (o[3] as u64),
        ts = ts.as_str(),
    )
}

fn build_lease_body(out: &mut HString<512>, renew_unix: u64) -> Result<(), core::fmt::Error> {
    let mut renew: HString<40> = HString::new();
    fmt_rfc3339(renew_unix, &mut renew)?;
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

fn build_status_body(
    out: &mut HString<1536>,
    t: &NodeCondTracker,
    heartbeat_unix: u64,
) -> Result<(), core::fmt::Error> {
    let mut hb: HString<40> = HString::new();
    fmt_rfc3339(heartbeat_unix, &mut hb)?;
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

fn build_annotation_body(
    out: &mut HString<256>,
    free_bytes: usize,
) -> Result<(), core::fmt::Error> {
    write!(
        out,
        r#"{{"metadata":{{"annotations":{{"node.specht.dev/heap-bytes-free":"{}"}}}}}}"#,
        free_bytes,
    )
}
