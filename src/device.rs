use anyhow::{Result, anyhow};
use rockusb::device::Device;
use rockusb::libusb::{DeviceUnavalable, Devices, Transport};
use rockusb::protocol::StorageIndex;

pub fn open_single() -> Result<Device<Transport>> {
    let devices = Devices::new()?;
    let mut iter: Vec<_> = devices.iter().collect();
    match iter.len() {
        0 => Err(anyhow!("No RockUSB devices found")),
        1 => match iter.pop().unwrap() {
            Ok(d) => Ok(d),
            Err(DeviceUnavalable { device, error }) => Err(anyhow!(
                "Device {:?} unavailable: {}",
                device,
                error
            )),
        },
        _ => Err(anyhow!(
            "Multiple RockUSB devices found; specify one with `leaflash usb -d <bus>:<addr>`"
        )),
    }
}

pub fn list() -> Result<Vec<DeviceSummary>> {
    let devices = Devices::new()?;
    let mut out = Vec::new();
    for d in devices.iter() {
        match d {
            Ok(d) => out.push(DeviceSummary {
                bus: d.bus_number(),
                address: d.address(),
                available: true,
            }),
            Err(DeviceUnavalable { device, .. }) => out.push(DeviceSummary {
                bus: device.bus_number(),
                address: device.address(),
                available: false,
            }),
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy)]
pub struct DeviceSummary {
    pub bus: u8,
    pub address: u8,
    pub available: bool,
}

/// Capacity probe + read of the on-disk GPT (if any). Used by the TUI to
/// drive the confirm dialog: if the existing partition table already
/// matches the layout we'd write, we can skip the SD-erase warning and
/// flash_image will skip the full erase + GPT rewrite.
pub fn probe_sd_full() -> Result<SdProbe> {
    let mut device = open_single()?;
    device.switch_storage(StorageIndex::Sd)?;
    let info = device.flash_info()?;
    let total_bytes = info.size();
    let total_sectors = info.sectors() as u64;
    let mut io = device.into_io()?;
    let existing = crate::flash::read_existing_layout(&mut io);
    Ok(SdProbe {
        total_bytes,
        total_sectors,
        existing,
    })
}

#[derive(Debug, Clone)]
pub struct SdProbe {
    pub total_bytes: u64,
    pub total_sectors: u64,
    pub existing: Option<crate::flash::Layout>,
}
