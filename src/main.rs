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

mod led;

use core::fmt::Write as FmtWrite;
use core::net::Ipv4Addr;
use core::sync::atomic::Ordering;
use portable_atomic::AtomicU64;

use embassy_executor::Spawner;
use embassy_net::{
    Config as NetConfig, IpAddress, Ipv4Address, Runner as NetRunner, Stack,
    StackResources, tcp::TcpSocket,
};
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::Write;
use embedded_tls::{
    Aes128GcmSha256, TlsConfig, TlsConnection, TlsContext, UnsecureProvider,
};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    interrupt::software::SoftwareInterruptControl,
    ram,
    rng::Rng,
    timer::timg::TimerGroup,
};
use esp_radio::wifi::{
    Config as WifiConfig, ControllerConfig, Interface as WifiInterface,
    WifiController, sta::StationConfig,
};
use heapless::String as HString;
use log::{info, warn};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use static_cell::StaticCell;

// ---- compile-time config from .env (loaded by mise) ----------------------
const K3S_API_HOST: &str = env!("K3S_API_HOST");
const K3S_API_PORT_STR: &str = env!("K3S_API_PORT");
const K3S_TOKEN: &str = env!("K3S_TOKEN");
const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PSK: &str = env!("WIFI_PSK");

// Node identity from .env (NODE_NAME), with a friendly default so the
// build doesn't break if it's missing. Each board in the rack should get
// its own. (German nodes get German names — once the Tamagotchi rack
// ships there'll be at least one Günther / Heinrich / Brigitte each.)
//
// Kubernetes node names must match [a-z0-9.-]+, so umlauts get
// transliterated: ü → ue, ö → oe, ß → ss.
const NODE_NAME: &str = match option_env!("NODE_NAME") {
    Some(s) => s,
    None => "esp-node-01-guenther",
};
const LEASE_DURATION_SECS: u32 = 40;
const LEASE_RENEW_PERIOD_SECS: u64 = 10;

// Status subresource updates are the *slow* heartbeat. Real kubelets default
// to 5 min (or sooner on change). The lease covers the fast path; status
// PATCHes refresh `lastHeartbeatTime` so observers like kube-state-metrics
// see a healthy node.
const STATUS_UPDATE_PERIOD_SECS: u64 = 300;

// Real kubelet flips MemoryPressure=True at <100Mi free. Scaled to ESP heap:
// True when free heap drops below this. With ~100KB total heap, 20KB is the
// "you're about to OOM" line.
const MEMORY_PRESSURE_FREE_BYTES: usize = 20 * 1024;

// Required by espflash: embeds an app descriptor (version, name, build date).
esp_bootloader_esp_idf::esp_app_desc!();

// ---- wall clock ---------------------------------------------------------
//
// We don't have an RTC. Anchor a unix timestamp from the HTTP Date header on
// the first /version response, then advance it via embassy_time::Instant.
// Good to ~1s of accuracy, which is well within the 40s lease tolerance.

static WALL_CLOCK_ANCHOR_UNIX: AtomicU64 = AtomicU64::new(0);
static WALL_CLOCK_ANCHOR_BOOT_MS: AtomicU64 = AtomicU64::new(0);

fn set_wall_clock(unix_secs: u64) {
    let now_ms = Instant::now().as_millis();
    WALL_CLOCK_ANCHOR_UNIX.store(unix_secs, Ordering::Release);
    WALL_CLOCK_ANCHOR_BOOT_MS.store(now_ms, Ordering::Release);
}

fn unix_now_secs() -> u64 {
    let anchor = WALL_CLOCK_ANCHOR_UNIX.load(Ordering::Acquire);
    let anchor_at = WALL_CLOCK_ANCHOR_BOOT_MS.load(Ordering::Acquire);
    if anchor == 0 {
        return 0;
    }
    let now_ms = Instant::now().as_millis();
    anchor + (now_ms.saturating_sub(anchor_at)) / 1000
}

// Convert (year, month, day, h, m, s) → unix epoch seconds. Howard Hinnant's
// civil_from_days algorithm in reverse, simplified for AD years.
fn unix_from_ymdhms(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> u64 {
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // 0..399
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) as u64 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as i64 * 146097 + doe as i64 - 719468;
    (days as u64) * 86400 + (h as u64) * 3600 + (mi as u64) * 60 + s as u64
}

/// Parse RFC1123 HTTP Date like "Sat, 02 May 2026 15:47:17 GMT" → unix secs.
/// Tolerant of the surrounding spaces but not other formats.
fn parse_http_date(s: &str) -> Option<u64> {
    // skip past day-of-week + comma + space (5–6 chars)
    let comma = s.find(',')?;
    let rest = s[comma + 1..].trim_start();
    let mut it = rest.split_ascii_whitespace();
    let day: u32 = it.next()?.parse().ok()?;
    let month = match it.next()? {
        "Jan" => 1, "Feb" => 2, "Mar" => 3, "Apr" => 4, "May" => 5, "Jun" => 6,
        "Jul" => 7, "Aug" => 8, "Sep" => 9, "Oct" => 10, "Nov" => 11, "Dec" => 12,
        _ => return None,
    };
    let year: i32 = it.next()?.parse().ok()?;
    let mut hms = it.next()?.split(':');
    let h: u32 = hms.next()?.parse().ok()?;
    let m: u32 = hms.next()?.parse().ok()?;
    let s2: u32 = hms.next()?.parse().ok()?;
    Some(unix_from_ymdhms(year, month, day, h, m, s2))
}

/// Format unix seconds as RFC3339 with microsecond precision (which k8s
/// expects in `renewTime`): "2026-05-02T15:47:17.000000Z".
fn fmt_rfc3339(unix: u64, out: &mut HString<40>) -> Result<(), core::fmt::Error> {
    // civil_from_days, forward
    let days = (unix / 86400) as i64;
    let secs_today = (unix % 86400) as u32;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + (era as i32) * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    let h = secs_today / 3600;
    let mi = (secs_today % 3600) / 60;
    let s = secs_today % 60;
    write!(
        out,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000000Z",
        y, m, d, h, mi, s
    )
}

// ---- main ----------------------------------------------------------------
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let peripherals =
        esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
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
    info!("target k3s api server: {}:{}", K3S_API_HOST, K3S_API_PORT_STR);
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

    let (stack, net_runner) =
        embassy_net::new(interfaces.station, net_config, net_resources, seed);

    spawner.spawn(connection_task(controller).unwrap());
    spawner.spawn(net_task(net_runner).unwrap());

    info!("waiting for DHCP lease...");
    stack.wait_config_up().await;
    let cfg = stack
        .config_v4()
        .expect("ipv4 config missing after wait_config_up");
    let my_ip: Ipv4Addr = cfg.address.address().into();
    info!("DHCP up: ip={} gw={:?}", cfg.address, cfg.gateway);

    // Resolve k3s endpoint.
    let port: u16 = K3S_API_PORT_STR.parse().expect("K3S_API_PORT must be u16");
    let api_host: Ipv4Addr =
        K3S_API_HOST.parse().expect("K3S_API_HOST must be IPv4 dotted-quad");
    let api_octets = api_host.octets();
    let api_smol = Ipv4Address::new(
        api_octets[0], api_octets[1], api_octets[2], api_octets[3],
    );

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
        stack, api_smol, port, rng, tls_read, tls_write,
        "GET", "/version", None, resp_buf,
    ).await {
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
        stack, api_smol, port, rng, tls_read, tls_write,
        "POST", "/api/v1/nodes", Some(node_body.as_bytes()), resp_buf,
    ).await {
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
        stack, api_smol, port, rng, tls_read, tls_write,
        "POST",
        "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases",
        Some(lease_body.as_bytes()), resp_buf,
    ).await {
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
        ).unwrap();
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
        stack, api_smol, port, rng, tls_read, tls_write,
        &status_path, &node_path, &mut tracker, resp_buf,
    ).await;
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
            stack, api_smol, port, rng, tls_read, tls_write,
            "PATCH", &lease_path, Some(body.as_bytes()), resp_buf,
        ).await {
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
                        stack, api_smol, port, rng, tls_read, tls_write,
                        "POST",
                        "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases",
                        Some(body.as_bytes()), resp_buf,
                    ).await;
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
        let due = now != 0
            && now.saturating_sub(last_status_update) >= STATUS_UPDATE_PERIOD_SECS;
        if flipped || due {
            push_status(
                stack, api_smol, port, rng, tls_read, tls_write,
                &status_path, &node_path, &mut tracker, resp_buf,
            ).await;
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
    tracker.memory_pressure.observe(free < MEMORY_PRESSURE_FREE_BYTES, now);

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
        stack, api_ip, api_port, rng, tls_read, tls_write,
        "PATCH-STRATEGIC", status_path, Some(body.as_bytes()), resp_buf,
    ).await {
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
        stack, api_ip, api_port, rng, tls_read, tls_write,
        "PATCH", node_path, Some(ann.as_bytes()), resp_buf,
    ).await {
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
        a = o[0], b = o[1], c = o[2], d = o[3],
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

// ---- node condition tracking --------------------------------------------
//
// `lastHeartbeatTime` advances every status PATCH; `lastTransitionTime`
// only advances when a condition's status field actually flips. Conflating
// the two is a real-kubelet anti-pattern that makes nodes look flappy in
// monitoring, so we track the "last flipped" time per-condition.

#[derive(Clone, Copy)]
struct CondState {
    /// Semantic value: for Ready, True means ready; for the *Pressure
    /// conditions, True means the node is under pressure.
    value: bool,
    transitioned_at: u64,
}

impl CondState {
    fn new(initial: bool, now: u64) -> Self {
        Self { value: initial, transitioned_at: now }
    }

    fn observe(&mut self, current: bool, now: u64) -> bool {
        let flipped = current != self.value;
        if flipped {
            self.value = current;
            self.transitioned_at = now;
        }
        flipped
    }
}

struct NodeCondTracker {
    ready: CondState,
    memory_pressure: CondState,
    disk_pressure: CondState,
    pid_pressure: CondState,
}

impl NodeCondTracker {
    fn new(now: u64) -> Self {
        Self {
            ready: CondState::new(true, now),
            memory_pressure: CondState::new(false, now),
            disk_pressure: CondState::new(false, now),
            pid_pressure: CondState::new(false, now),
        }
    }
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
        mp = if t.memory_pressure.value { "True" } else { "False" },
        dp = if t.disk_pressure.value { "True" } else { "False" },
        pp = if t.pid_pressure.value { "True" } else { "False" },
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

// ---- HTTPS request helper -----------------------------------------------

#[derive(Debug)]
#[allow(dead_code)]
enum ApiError {
    Tcp(embassy_net::tcp::ConnectError),
    Tls(embedded_tls::TlsError),
    Fmt,
    Truncated(usize),
    BadResponse,
}

impl From<embassy_net::tcp::ConnectError> for ApiError {
    fn from(e: embassy_net::tcp::ConnectError) -> Self { Self::Tcp(e) }
}
impl From<embedded_tls::TlsError> for ApiError {
    fn from(e: embedded_tls::TlsError) -> Self { Self::Tls(e) }
}

struct Response<'a> {
    status: u16,
    headers: &'a str,
    body: &'a [u8],
}

impl<'a> Response<'a> {
    fn header(&self, name: &str) -> Option<&'a str> {
        for line in self.headers.lines() {
            if let Some((k, v)) = line.split_once(':') {
                if k.eq_ignore_ascii_case(name) {
                    return Some(v.trim());
                }
            }
        }
        None
    }
}

#[allow(clippy::too_many_arguments)]
async fn k8s_request<'a>(
    stack: Stack<'static>,
    api_ip: Ipv4Address,
    api_port: u16,
    rng: Rng,
    tls_read: &mut [u8],
    tls_write: &mut [u8],
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    resp_buf: &'a mut [u8],
) -> Result<Response<'a>, ApiError> {
    // TCP.
    let mut tcp_rx = [0u8; 4096];
    let mut tcp_tx = [0u8; 4096];
    let mut socket = TcpSocket::new(stack, &mut tcp_rx, &mut tcp_tx);
    socket.set_timeout(Some(Duration::from_secs(10)));
    socket.connect((IpAddress::Ipv4(api_ip), api_port)).await?;

    // TLS handshake.
    let cfg = TlsConfig::new().with_server_name(K3S_API_HOST);
    let mut tls = TlsConnection::<_, Aes128GcmSha256>::new(socket, tls_read, tls_write);

    let mut chacha_seed = [0u8; 32];
    for chunk in chacha_seed.chunks_exact_mut(4) {
        chunk.copy_from_slice(&rng.random().to_le_bytes());
    }
    let chacha = ChaCha8Rng::from_seed(chacha_seed);
    tls.open(TlsContext::new(
        &cfg,
        UnsecureProvider::new::<Aes128GcmSha256>(chacha),
    ))
    .await?;

    // Build the request line + headers. Sized for a ~1KB SA-token JWT plus
    // the long-ish lease PATCH path; bumping further is cheap.
    //
    // PATCH defaults to RFC 7396 merge-patch; "PATCH-STRATEGIC" picks
    // application/strategic-merge-patch+json, which is what the kubelet
    // status subresource wants so it knows to merge the conditions array
    // by `type` instead of replacing it wholesale.
    let (http_method, content_type) = match method {
        "PATCH" => ("PATCH", "application/merge-patch+json"),
        "PATCH-STRATEGIC" => ("PATCH", "application/strategic-merge-patch+json"),
        m => (m, "application/json"),
    };
    let mut head: HString<2048> = HString::new();
    let body_len = body.map(|b| b.len()).unwrap_or(0);
    write!(
        &mut head,
        "{method} {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Authorization: Bearer {tok}\r\n\
         User-Agent: picokubelet/0.1\r\n\
         Accept: application/json\r\n\
         Content-Type: {ctype}\r\n\
         Content-Length: {clen}\r\n\
         Connection: close\r\n\
         \r\n",
        method = http_method,
        path = path,
        host = K3S_API_HOST,
        port = K3S_API_PORT_STR,
        tok = K3S_TOKEN,
        ctype = content_type,
        clen = body_len,
    )
    .map_err(|_| ApiError::Fmt)?;

    tls.write_all(head.as_bytes()).await?;
    if let Some(b) = body {
        tls.write_all(b).await?;
    }
    tls.flush().await?;

    // Read until the server closes (we sent Connection: close).
    let mut total = 0;
    loop {
        if total == resp_buf.len() {
            return Err(ApiError::Truncated(total));
        }
        match tls.read(&mut resp_buf[total..]).await {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(embedded_tls::TlsError::ConnectionClosed) => break,
            Err(e) => return Err(ApiError::Tls(e)),
        }
    }
    let _ = tls.close().await;

    parse_response(&resp_buf[..total])
}

fn parse_response(buf: &[u8]) -> Result<Response<'_>, ApiError> {
    // Find header/body split.
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").ok_or(ApiError::BadResponse)?;
    let head = core::str::from_utf8(&buf[..split]).map_err(|_| ApiError::BadResponse)?;
    let body = &buf[split + 4..];

    // Status line: "HTTP/1.1 200 OK"
    let status_line = head.lines().next().ok_or(ApiError::BadResponse)?;
    let mut parts = status_line.split_ascii_whitespace();
    let _http = parts.next();
    let code = parts.next().ok_or(ApiError::BadResponse)?;
    let status: u16 = code.parse().map_err(|_| ApiError::BadResponse)?;

    // Headers = everything after the status line.
    let headers = head.split_once("\r\n").map(|(_, h)| h).unwrap_or("");

    Ok(Response { status, headers, body })
}

// ---- background tasks ----------------------------------------------------

#[embassy_executor::task]
async fn connection_task(mut controller: WifiController<'static>) {
    loop {
        info!("wifi: connecting to '{}'...", WIFI_SSID);
        match controller.connect_async().await {
            Ok(info) => {
                info!("wifi: associated ({:?})", info);
                let dc = controller.wait_for_disconnect_async().await.ok();
                warn!("wifi: disconnected ({:?})", dc);
            }
            Err(e) => warn!("wifi: connect failed: {:?}", e),
        }
        Timer::after(Duration::from_secs(5)).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: NetRunner<'static, WifiInterface<'static>>) -> ! {
    runner.run().await
}
