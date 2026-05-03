//! picokubelet, phase 4: register, stay Ready, and report honest health.
//!
//! After Wi-Fi + DHCP + TLS to k3s:
//!  1. GET /version once to anchor a wall clock (parsed from the Date header
//!     because we don't have an RTC).
//!  2. POST /api/v1/nodes with our Node spec, lying about CPU/memory.
//!  3. POST a Lease into kube-node-lease.
//!  4. PATCH /nodes/{name}/status with fresh heartbeats + a heap-derived
//!     MemoryPressure; PATCH /nodes/{name} with a free-heap annotation.
//!  5. Spawn the lease and status reconcilers; park forever.
//!
//! The lease keeps the Node Lifecycle Controller off our back on a 40s
//! cadence; the status PATCH keeps every condition's lastHeartbeatTime
//! fresh so kube-state-metrics and other second-order observers see a
//! healthy node, not a node whose conditions are all 0001-01-01.
//!
//! Cert verification is still off. Fine for the home lab, will become a
//! real CA + client cert in a later phase.

#![no_std]
#![no_main]

mod config;
mod k8s;
mod kubelet;
mod led;
mod net;
mod reconcilers;
mod wallclock;

use core::net::Ipv4Addr;

use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, Ipv4Address, StackResources};
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, ram, rng::Rng,
    timer::timg::TimerGroup,
};
use esp_radio::wifi::{Config as WifiConfig, ControllerConfig, sta::StationConfig};
use log::info;
use static_cell::StaticCell;

use crate::config::{
    K3S_API_HOST, K3S_API_PORT_STR, LEASE_RENEW_PERIOD_SECS, NODE_NAME, STATUS_UPDATE_PERIOD_SECS,
    WIFI_PSK, WIFI_SSID,
};
use crate::kubelet::NodeIdentity;
use crate::net::client::{ApiClient, SharedClient};
use crate::net::wifi::{connection_task, net_task};

// Required by espflash: embeds an app descriptor (version, name, build date).
esp_bootloader_esp_idf::esp_app_desc!();

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

    info!("picokubelet booting on ESP32-S3 (here we go again)");
    info!("identity confirmed: {} (it me)", NODE_NAME);
    info!(
        "target k3s api server: {}:{} (the control plane, allegedly)",
        K3S_API_HOST, K3S_API_PORT_STR
    );
    info!("joining SSID: {} (please be there)", WIFI_SSID);

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

    info!("waiting for DHCP lease (the original lease)...");
    stack.wait_config_up().await;
    let cfg = stack
        .config_v4()
        .expect("ipv4 config missing after wait_config_up");
    let my_ip: Ipv4Addr = cfg.address.address();
    info!(
        "DHCP up: ip={} gw={:?} (we are someone now)",
        cfg.address, cfg.gateway
    );

    // Resolve k3s endpoint.
    let port: u16 = K3S_API_PORT_STR.parse().expect("K3S_API_PORT must be u16");
    let api_host: Ipv4Addr = K3S_API_HOST
        .parse()
        .expect("K3S_API_HOST must be IPv4 dotted-quad");
    let api_octets = api_host.octets();
    let api_smol = Ipv4Address::new(api_octets[0], api_octets[1], api_octets[2], api_octets[3]);

    // TLS record buffers, static so the task arena stays small.
    const TLS_BUF: usize = 16 * 1024 + 256;
    static TLS_READ_BUF: StaticCell<[u8; TLS_BUF]> = StaticCell::new();
    static TLS_WRITE_BUF: StaticCell<[u8; TLS_BUF]> = StaticCell::new();
    let tls_read = TLS_READ_BUF.init([0u8; TLS_BUF]);
    let tls_write = TLS_WRITE_BUF.init([0u8; TLS_BUF]);

    // Response scratch. The Node create reply can be ~6 KB.
    static RESP_BUF: StaticCell<[u8; 8192]> = StaticCell::new();
    let resp_buf = RESP_BUF.init([0u8; 8192]);

    let client = ApiClient::new(stack, api_smol, port, rng, tls_read, tls_write, resp_buf);
    static SHARED_CLIENT: StaticCell<SharedClient> = StaticCell::new();
    let shared: &'static SharedClient = SHARED_CLIENT.init(Mutex::new(client));

    let identity = NodeIdentity::new(my_ip);

    let (tracker, custom) = kubelet::bootstrap(shared, &identity).await;

    info!(
        "entering renewal loop (lease {}s, status {}s). and so it begins.",
        LEASE_RENEW_PERIOD_SECS, STATUS_UPDATE_PERIOD_SECS,
    );
    spawner.spawn(reconcilers::lease::lease_reconciler(shared, identity.clone()).unwrap());
    spawner
        .spawn(reconcilers::status::status_reconciler(shared, identity, tracker, custom).unwrap());

    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}
