use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
use embassy_net::Runner as NetRunner;
use embassy_net_wiznet::{Device as WiznetNetDevice, Runner as WiznetRunner, chip::W5500};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embedded_hal_async::spi::{Operation, SpiDevice as _};
use esp_hal::{
    Async,
    gpio::{Input, Output},
    spi::master::Spi,
};
use log::{info, warn};

const W5500_VERSIONR: u16 = 0x0039;
const W5500_PHYCFGR: u16 = 0x002e;
const W5500_VERSION: u8 = 0x04;
const W5500_PHY_LINK_UP: u8 = 0x01;

pub type EthSpiBus = Spi<'static, Async>;
pub type EthSpiDevice =
    SpiDevice<'static, CriticalSectionRawMutex, EthSpiBus, Output<'static>>;
pub type EthRunner =
    WiznetRunner<'static, W5500, EthSpiDevice, Input<'static>, Output<'static>>;

fn read_header(addr: u16) -> [u8; 3] {
    [(addr >> 8) as u8, addr as u8, 0x00]
}

async fn read_common_register(spi: &mut EthSpiDevice, addr: u16) -> Result<u8, ()> {
    let header = read_header(addr);
    let mut value = [0u8; 1];
    spi.transaction(&mut [Operation::Write(&header), Operation::Read(&mut value)])
        .await
        .map_err(|_| ())?;

    Ok(value[0])
}

pub async fn probe_w5500(spi: &mut EthSpiDevice) -> bool {
    let version = match read_common_register(spi, W5500_VERSIONR).await {
        Ok(version) => version,
        Err(()) => {
            warn!("ethernet: W5500 did not answer the door");
            return false;
        }
    };

    if version != W5500_VERSION {
        warn!("ethernet: W5500 said it is 0x{:02x}, which feels suspicious", version);
        return false;
    }

    match read_common_register(spi, W5500_PHYCFGR).await {
        Ok(phy) if phy & W5500_PHY_LINK_UP != 0 => {
            info!("ethernet: W5500 is here and the cable has opinions");
            true
        }
        Ok(phy) => {
            warn!(
                "ethernet: W5500 is present but the link is taking a personal day (PHYCFGR=0x{:02x})",
                phy
            );
            false
        }
        Err(()) => {
            warn!("ethernet: W5500 appeared, then got weird about PHY status");
            false
        }
    }
}

#[embassy_executor::task]
pub async fn ethernet_task(runner: EthRunner) -> ! {
    runner.run().await
}

#[embassy_executor::task]
pub async fn net_task(mut runner: NetRunner<'static, WiznetNetDevice<'static>>) -> ! {
    runner.run().await
}
