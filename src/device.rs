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

/// Open the specified RockUSB device (matched by USB bus + address).
pub fn open_at(bus: u8, address: u8) -> Result<Device<Transport>> {
    let devices = Devices::new()?;
    for d in devices.iter() {
        match d {
            Ok(dev) if dev.bus_number() == bus && dev.address() == address => return Ok(dev),
            Err(DeviceUnavalable { device, error })
                if device.bus_number() == bus && device.address() == address =>
            {
                return Err(anyhow!(
                    "RockUSB device {}:{} unavailable: {}",
                    bus,
                    address,
                    error
                ));
            }
            _ => {}
        }
    }
    Err(anyhow!("RockUSB device {}:{} not found", bus, address))
}

/// Same as `probe_sd_full` but picks a specific device by bus+address
/// — used by the TUI when more than one device is attached.
pub fn probe_sd_full_at(bus: u8, address: u8) -> Result<SdProbe> {
    probe_with(open_at(bus, address)?)
}

fn probe_with(mut device: Device<Transport>) -> Result<SdProbe> {
    device.switch_storage(StorageIndex::Sd)?;
    let info = device.flash_info()?;
    let total_bytes = info.size();
    let total_sectors = info.sectors() as u64;
    let mut io = device.into_io()?;
    let existing = crate::flash::read_existing_layout_info(&mut io);
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
    pub existing: Option<crate::flash::LayoutInfo>,
}

/// USB bus + device address, used by CLI subcommands that target a
/// specific RockUSB device. Parsed from "<bus>:<address>".
#[derive(Debug, Clone, Copy)]
pub struct DeviceAddr {
    pub bus: u8,
    pub address: u8,
}

pub fn parse_device_addr(s: &str) -> Result<DeviceAddr, String> {
    let mut parts = s.split(':');
    let bus: u8 = parts
        .next()
        .ok_or_else(|| "missing bus number; use <bus>:<address>".to_string())?
        .parse()
        .map_err(|_| "bus should be a number".to_string())?;
    let address: u8 = parts
        .next()
        .ok_or_else(|| "missing address; use <bus>:<address>".to_string())?
        .parse()
        .map_err(|_| "address should be a number".to_string())?;
    if parts.next().is_some() {
        return Err("too many parts; use <bus>:<address>".to_string());
    }
    Ok(DeviceAddr { bus, address })
}
