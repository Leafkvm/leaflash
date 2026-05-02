use std::path::PathBuf;

use anyhow::{Result, anyhow, bail, ensure};
use clap::Args;
use indicatif::{ProgressBar, ProgressStyle};
use rockusb::device::Device;
use rockusb::libusb::Transport;
use rockusb::protocol::StorageIndex;

use crate::device;
use crate::flash::SECTOR_SIZE;
use crate::rkfw;

/// Write a Rockchip `update.img` to the device's SPI NOR. The image
/// MUST be in RKFW format (output of `rkImageMaker` / what
/// `upgrade_tool UF` consumes); raw flash dumps are not accepted —
/// every component partition is dispatched to the LBA recorded inside
/// the package.
#[derive(Debug, Args)]
pub struct UbootArgs {
    /// Rockchip update.img file (RKFW format).
    #[arg(long)]
    pub image: PathBuf,
}

pub fn run(args: UbootArgs) -> Result<()> {
    let raw = std::fs::read(&args.image)
        .map_err(|e| anyhow!("failed to read image {}: {}", args.image.display(), e))?;
    let pkg = rkfw::parse(&raw).map_err(|e| {
        anyhow!(
            "{}\n  --image must be a Rockchip update.img (RKFW). Pass the file produced by \
             rkImageMaker / `upgrade_tool UF`, not a raw SPI-NOR dump.",
            e,
        )
    })?;

    let mut dev = device::open_single()?;

    println!("Switching storage to SPI NOR...");
    dev.switch_storage(StorageIndex::MtdBlkSpiNor)?;
    let current = dev.get_storage()?;
    if current != StorageIndex::MtdBlkSpiNor {
        bail!("Failed to switch storage to SPI NOR; current: {:?}", current);
    }

    let info = dev.flash_info()?;
    let total_sectors = info.sectors();
    ensure!(total_sectors > 0, "SPI NOR reports 0 sectors");
    println!(
        "SPI NOR: {} KiB ({} sectors)",
        info.size() / 1024,
        total_sectors,
    );

    // Capability-aware erase chunking — same logic as rockusb-cli's
    // erase_flash. SPI NOR is non-eMMC + non-direct-LBA, so it must use
    // erase_force at 1024-sector chunks; eMMC / direct-LBA can take
    // erase_lba at 32K-sector chunks.
    let cap = dev.capability()?;
    let id = dev.flash_id()?;
    let is_emmc = id.to_str() == "EMMC ";
    let is_lba = cap.direct_lba();
    let max_blocks: u32 = if is_emmc || is_lba { 32 * 1024 } else { 1024 };

    let mut to_flash: Vec<&rkfw::Entry> = pkg
        .entries
        .iter()
        .filter(|e| e.target_sector.is_some())
        .collect();
    // Flash partitions in ascending LBA order — keeps the per-entry erase
    // ranges in disk order, which is friendlier on USB and easier to
    // follow in the log.
    to_flash.sort_by_key(|e| e.target_sector.unwrap());

    if to_flash.is_empty() {
        bail!("update.img contains no flashable partitions (all targets are 0xFFFFFFFF)");
    }

    println!("Partitions to flash:");
    for e in &to_flash {
        println!(
            "  {:14} LBA {:>6}  {:>8} sectors partition  ({} bytes file)",
            e.name,
            e.target_sector.unwrap(),
            e.partition_sectors,
            e.data.len(),
        );
    }

    for entry in &to_flash {
        let target = entry.target_sector.unwrap();
        let img_len = entry.data.len() as u64;
        let img_sectors = img_len.div_ceil(SECTOR_SIZE) as u32;
        // Partitions can be larger than the file we're writing into them —
        // wipe the whole partition so stale data doesn't leak in.
        let erase_sectors = entry.partition_sectors.max(img_sectors);
        let end = target
            .checked_add(erase_sectors)
            .ok_or_else(|| anyhow!("LBA overflow for {}", entry.name))?;
        ensure!(
            end as u64 <= total_sectors as u64,
            "{} (LBA {}..{}) exceeds SPI NOR size ({} sectors)",
            entry.name,
            target,
            end,
            total_sectors
        );

        // ---- erase ----
        erase_range(&mut dev, target, erase_sectors, max_blocks, is_emmc || is_lba, &entry.name)?;

        // ---- write ----
        write_entry(&mut dev, target, entry)?;
    }

    println!("Done.");
    Ok(())
}

fn erase_range(
    dev: &mut Device<Transport>,
    first: u32,
    count: u32,
    max_blocks: u32,
    use_lba: bool,
    name: &str,
) -> Result<()> {
    let bar = ProgressBar::new(count as u64);
    bar.set_style(
        ProgressStyle::with_template("Erasing {msg} [{bar:40.cyan/blue}] {pos}/{len}")
            .unwrap()
            .progress_chars("=>-"),
    );
    bar.set_message(name.to_string());

    let mut start = first;
    let end = first
        .checked_add(count)
        .ok_or_else(|| anyhow!("erase range overflow"))?;
    while start < end {
        let chunk = max_blocks.min(end - start);
        if use_lba {
            dev.erase_lba(start, chunk as u16)?;
        } else {
            dev.erase_force(start, chunk as u16)?;
        }
        start += chunk;
        bar.inc(chunk as u64);
    }
    bar.finish();
    Ok(())
}

fn write_entry(dev: &mut Device<Transport>, target_sector: u32, entry: &rkfw::Entry) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    let img_len = entry.data.len() as u64;
    // Use a scoped DeviceIO: reborrow the device through DeviceIO for the
    // write, drop it back to a plain Device when done.
    let mut io = dev.io()?;
    io.seek(SeekFrom::Start(target_sector as u64 * SECTOR_SIZE))?;
    let bar = ProgressBar::new(img_len);
    bar.set_style(
        ProgressStyle::with_template("Writing {msg} [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
            .unwrap()
            .progress_chars("=>-"),
    );
    bar.set_message(entry.name.clone());

    let mut written: u64 = 0;
    let chunk = 64 * 1024usize;
    while written < img_len {
        let n = chunk.min((img_len - written) as usize);
        let s = written as usize;
        io.write_all(&entry.data[s..s + n])?;
        written += n as u64;
        bar.inc(n as u64);
    }
    io.flush()?;
    bar.finish();
    Ok(())
}
