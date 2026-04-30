use anyhow::{Result, anyhow};
use clap::Args;
use rockusb::libusb::{DeviceUnavalable, Devices};
use rockusb_cli::{Command, DeviceArg, ExampleDevice};

#[derive(Debug, Args)]
pub struct UsbArgs {
    /// Device specified as <bus>:<address> (when multiple devices are attached)
    #[arg(short, long, value_parser = parse_device)]
    pub device: Option<DeviceArg>,
    #[command(subcommand)]
    pub command: Command,
}

fn parse_device(s: &str) -> Result<DeviceArg, String> {
    let mut parts = s.split(':');
    let bus_number: u8 = parts
        .next()
        .ok_or_else(|| "no bus number: use <bus>:<address>".to_string())?
        .parse()
        .map_err(|_| "bus should be a number".to_string())?;
    let address: u8 = parts
        .next()
        .ok_or_else(|| "no address: use <bus>:<address>".to_string())?
        .parse()
        .map_err(|_| "address should be a number".to_string())?;
    if parts.next().is_some() {
        return Err("too many parts".to_string());
    }
    Ok(DeviceArg { bus_number, address })
}

pub fn run(args: UsbArgs) -> Result<()> {
    if matches!(args.command, Command::List) {
        return list_available_devices();
    }

    let devices = Devices::new()?;
    let device = if let Some(want) = args.device {
        devices
            .iter()
            .find(|d| match d {
                Ok(d) => d.bus_number() == want.bus_number && d.address() == want.address,
                Err(DeviceUnavalable { device, .. }) => {
                    device.bus_number() == want.bus_number && device.address() == want.address
                }
            })
            .ok_or_else(|| anyhow!("specified device not found"))?
    } else {
        let mut devs: Vec<_> = devices.iter().collect();
        match devs.len() {
            0 => return Err(anyhow!("no devices found")),
            1 => devs.pop().unwrap(),
            _ => {
                drop(devs);
                let _ = list_available_devices();
                return Err(anyhow!("please select a specific device with -d"));
            }
        }
    }?;

    let device = ExampleDevice::new(device);
    args.command.run(device)
}

fn list_available_devices() -> Result<()> {
    let devices = Devices::new()?;
    println!("Available rockchip devices");
    for d in devices.iter() {
        match d {
            Ok(d) => println!("* {:?}", d.transport().handle().device()),
            Err(DeviceUnavalable { device, error }) => {
                println!("* {:?} - Unavailable: {}", device, error)
            }
        }
    }
    Ok(())
}
