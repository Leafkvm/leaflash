use anyhow::{Result, anyhow, bail};
use clap::Args;
use gptman::GPT;
use rockusb::protocol::StorageIndex;

use crate::device;
use crate::flash::Partition;

const ATTR_LEGACY_BIOS_BOOTABLE: u64 = 0x4;

#[derive(Debug, Args, Clone)]
pub struct SwitchRootfsArgs {
    /// Target a specific RockUSB device as <bus>:<address>. If omitted
    /// and exactly one device is attached, that one is used.
    #[arg(short, long, value_parser = device::parse_device_addr)]
    pub device: Option<device::DeviceAddr>,
    /// Slot to make active. If omitted, switch to the inactive slot;
    /// if neither slot is currently active, default to rootfs_a with a
    /// warning.
    #[arg(short = 'p', long, value_parser = parse_partition)]
    pub partition: Option<Partition>,
}

fn parse_partition(s: &str) -> Result<Partition, String> {
    match s {
        "rootfs_a" | "a" | "A" => Ok(Partition::RootfsA),
        "rootfs_b" | "b" | "B" => Ok(Partition::RootfsB),
        other => Err(format!(
            "expected rootfs_a or rootfs_b for switch-rootfs, got {other}"
        )),
    }
}

pub fn run(args: SwitchRootfsArgs) -> Result<()> {
    let mut dev = match args.device {
        Some(addr) => device::open_at(addr.bus, addr.address)?,
        None => device::open_single()?,
    };
    dev.switch_storage(StorageIndex::Sd)?;
    let current = dev.get_storage()?;
    if current != StorageIndex::Sd {
        bail!("failed to switch storage to SD; current: {:?}", current);
    }

    let mut io = dev.into_io()?;
    let mut gpt = GPT::find_from(&mut io)
        .map_err(|e| anyhow!("no readable GPT on the SD card: {}", e))?;

    if !gpt[1].is_used()
        || !gpt[2].is_used()
        || gpt[1].partition_name.as_str() != "rootfs_a"
        || gpt[2].partition_name.as_str() != "rootfs_b"
    {
        bail!(
            "SD card does not have a leaflash A/B layout (rootfs_a + rootfs_b in slots 1 and 2); \
             flash one with `leaflash flash` first"
        );
    }

    let a_active = (gpt[1].attribute_bits & ATTR_LEGACY_BIOS_BOOTABLE) != 0;
    let b_active = (gpt[2].attribute_bits & ATTR_LEGACY_BIOS_BOOTABLE) != 0;

    let target = match args.partition {
        Some(p) => p,
        None => match (a_active, b_active) {
            (true, false) => Partition::RootfsB,
            (false, true) => Partition::RootfsA,
            (true, true) => {
                eprintln!(
                    "warning: both rootfs_a and rootfs_b are marked active; defaulting to rootfs_a"
                );
                Partition::RootfsA
            }
            (false, false) => {
                eprintln!(
                    "warning: neither rootfs_a nor rootfs_b is marked active; defaulting to rootfs_a"
                );
                Partition::RootfsA
            }
        },
    };

    let current_label = match (a_active, b_active) {
        (true, false) => "rootfs_a",
        (false, true) => "rootfs_b",
        (true, true) => "both",
        (false, false) => "neither",
    };
    println!(
        "Active slot: {} -> switching to {}",
        current_label,
        target.name()
    );

    if target == Partition::Both {
        bail!("switch-rootfs only supports rootfs_a or rootfs_b");
    }

    let (a_bits, b_bits) = match target {
        Partition::RootfsA => (ATTR_LEGACY_BIOS_BOOTABLE, 0),
        Partition::RootfsB => (0, ATTR_LEGACY_BIOS_BOOTABLE),
        Partition::Both => unreachable!(),
    };

    if gpt[1].attribute_bits == a_bits && gpt[2].attribute_bits == b_bits {
        println!("Nothing to do; {} is already the only active slot.", target.name());
        return Ok(());
    }

    gpt[1].attribute_bits = a_bits;
    gpt[2].attribute_bits = b_bits;

    gpt.write_into(&mut io)?;
    use std::io::Write;
    io.flush()?;

    println!("Done. Next boot will use {}.", target.name());
    Ok(())
}
