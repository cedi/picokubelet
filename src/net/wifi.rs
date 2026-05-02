use core::sync::atomic::Ordering;

use embassy_net::Runner as NetRunner;
use embassy_time::{Duration, Timer};
use esp_radio::wifi::{Interface as WifiInterface, WifiController};
use log::{info, warn};
use portable_atomic::{AtomicU32, AtomicU64};

use crate::config::WIFI_SSID;

/// Last associated AP's BSSID, folded into the low 48 bits of a u64. 0 means
/// "never associated yet"; sentinel so the first sample doesn't trip the
/// Haunted condition. Updated by `connection_task` on every successful
/// associate; read by the status reconciler to detect AP flips.
pub static WIFI_BSSID: AtomicU64 = AtomicU64::new(0);

/// Total wifi associations since boot. The first associate counts as 1,
/// every subsequent reconnect adds one. The status reconciler keeps its own
/// "last seen" snapshot and computes a per-window delta from this.
pub static WIFI_ASSOCIATIONS_TOTAL: AtomicU32 = AtomicU32::new(0);

fn bssid_to_u64(b: [u8; 6]) -> u64 {
    ((b[0] as u64) << 40)
        | ((b[1] as u64) << 32)
        | ((b[2] as u64) << 24)
        | ((b[3] as u64) << 16)
        | ((b[4] as u64) << 8)
        | (b[5] as u64)
}

#[embassy_executor::task]
pub async fn connection_task(mut controller: WifiController<'static>) {
    loop {
        info!("wifi: knocking on '{}'...", WIFI_SSID);
        match controller.connect_async().await {
            Ok(info) => {
                WIFI_BSSID.store(bssid_to_u64(info.bssid), Ordering::Release);
                WIFI_ASSOCIATIONS_TOTAL.fetch_add(1, Ordering::Release);
                info!("wifi: associated ({:?}); we have a layer 2", info);
                let dc = controller.wait_for_disconnect_async().await.ok();
                warn!("wifi: disconnected ({:?}); back to the void", dc);
            }
            Err(e) => warn!("wifi: connect failed: {:?}", e),
        }
        Timer::after(Duration::from_secs(5)).await;
    }
}

#[embassy_executor::task]
pub async fn net_task(mut runner: NetRunner<'static, WifiInterface<'static>>) -> ! {
    runner.run().await
}
