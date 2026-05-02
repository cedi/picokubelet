use embassy_net::Runner as NetRunner;
use embassy_time::{Duration, Timer};
use esp_radio::wifi::{Interface as WifiInterface, WifiController};
use log::{info, warn};

use crate::config::WIFI_SSID;

#[embassy_executor::task]
pub async fn connection_task(mut controller: WifiController<'static>) {
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
pub async fn net_task(mut runner: NetRunner<'static, WifiInterface<'static>>) -> ! {
    runner.run().await
}
