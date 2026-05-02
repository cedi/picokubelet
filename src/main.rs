//! picokubelet — phase 1: bring the network up over Wi-Fi and prove we can
//! reach the k3s control plane on TCP. No TLS, no HTTP, no kubelet logic yet.
//!
//! Wi-Fi is the dev-loop transport (laptop + USB cable). The eventual
//! production target is the Waveshare ESP32-S3-ETH's W5500 Ethernet PHY —
//! that path will come back when the rack is ready.

#![no_std]
#![no_main]

use core::net::Ipv4Addr;

use embassy_executor::Spawner;
use embassy_net::{
    Config as NetConfig, Ipv4Address, Runner as NetRunner, StackResources,
    tcp::TcpSocket,
};
use embassy_time::{Duration, Timer};
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
use log::{info, warn};
use static_cell::StaticCell;

// ---- compile-time config from .env (loaded by mise) ----------------------
const K3S_API_HOST: &str = env!("K3S_API_HOST");
const K3S_API_PORT_STR: &str = env!("K3S_API_PORT");
const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PSK: &str = env!("WIFI_PSK");

// Required by espflash: embeds an app descriptor (version, name, build date)
// in the firmware image at a fixed offset for the ESP-IDF bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

// ---- main ----------------------------------------------------------------
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // 1) Bring up the chip with a sensible clock + early logging.
    let peripherals =
        esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_println::logger::init_logger_from_env();

    // 2) esp-radio needs a heap. Grab some reclaimed boot RAM plus a chunk of
    //    regular SRAM. Sizes match the esp-rs canonical Wi-Fi example.
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // 3) Boot the embassy executor on top of esp-rtos.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    info!("picokubelet booting on ESP32-S3 (Wi-Fi mode)");
    info!("target k3s api server: {}:{}", K3S_API_HOST, K3S_API_PORT_STR);
    info!("joining SSID: {}", WIFI_SSID);

    // 4) Wi-Fi controller + STA interface.
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
    let wifi_iface = interfaces.station;

    // 5) embassy-net stack on top of the Wi-Fi STA interface.
    let net_config = NetConfig::dhcpv4(Default::default());
    static NET_RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();
    let net_resources = NET_RESOURCES.init(StackResources::<4>::new());

    let rng = Rng::new();
    let seed = ((rng.random() as u64) << 32) | rng.random() as u64;

    let (stack, net_runner) =
        embassy_net::new(wifi_iface, net_config, net_resources, seed);

    spawner.spawn(connection_task(controller).unwrap());
    spawner.spawn(net_task(net_runner).unwrap());

    // 6) Wait for DHCP.
    info!("waiting for DHCP lease...");
    stack.wait_config_up().await;
    let cfg = stack
        .config_v4()
        .expect("ipv4 config missing after wait_config_up");
    info!(
        "DHCP up: ip={} gw={:?} dns={:?}",
        cfg.address, cfg.gateway, cfg.dns_servers,
    );

    // 7) Probe TCP to the k3s API server. Phase 1 success = three-way handshake.
    let port: u16 = K3S_API_PORT_STR
        .parse()
        .expect("K3S_API_PORT must be a u16");
    let host: Ipv4Addr = K3S_API_HOST
        .parse()
        .expect("K3S_API_HOST must be an IPv4 dotted-quad for now");
    let host_octets = host.octets();
    let host_smol = Ipv4Address::new(
        host_octets[0],
        host_octets[1],
        host_octets[2],
        host_octets[3],
    );

    loop {
        let mut rx_buf = [0u8; 1024];
        let mut tx_buf = [0u8; 1024];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_timeout(Some(Duration::from_secs(5)));

        info!("dialing {}:{}...", host, port);
        match socket.connect((host_smol, port)).await {
            Ok(()) => {
                info!(
                    "TCP connected to k3s api server. \
                     handshake worked, the wires are real."
                );
                // Don't speak HTTPS bytes — k3s would just reset us. Hold
                // the socket briefly to confirm the link is stable.
                Timer::after(Duration::from_secs(2)).await;
                socket.close();
            }
            Err(e) => warn!("connect failed: {:?}", e),
        }

        Timer::after(Duration::from_secs(10)).await;
    }
}

// ---- background tasks ----------------------------------------------------

/// Manages the Wi-Fi association: connect, then wait for disconnect, then
/// reconnect after a short backoff. Survives router reboots, brief AP outages,
/// and us walking out of range.
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
