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

/// Open the device, switch to SD storage, and read its capacity. Used by
/// the TUI to size-check images before flashing.
pub fn probe_sd_size() -> Result<u64> {
    let mut device = open_single()?;
    device.switch_storage(StorageIndex::Sd)?;
    let info = device.flash_info()?;
    Ok(info.size())
}
