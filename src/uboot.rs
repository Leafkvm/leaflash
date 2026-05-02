use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use anyhow::{Result, anyhow, ensure};
use clap::Args;
use indicatif::{ProgressBar, ProgressStyle};
use rockusb::protocol::StorageIndex;

use crate::device;
use crate::flash::SECTOR_SIZE;

/// Write a raw u-boot / SPI-NOR image to the device's SPI NOR storage.
/// Switches storage to SPI NOR, erases the region the image covers, then
/// writes the image starting at LBA 0.
#[derive(Debug, Args)]
pub struct UbootArgs {
    /// U-Boot / SPI-NOR image file (raw, sector-aligned).
    #[arg(long)]
    pub image: PathBuf,
}

pub fn run(args: UbootArgs) -> Result<()> {
    let mut dev = device::open_single()?;

    println!("Switching storage to SPI NOR...");
    dev.switch_storage(StorageIndex::MtdBlkSpiNor)?;
    let current = dev.get_storage()?;
    if current != StorageIndex::MtdBlkSpiNor {
        return Err(anyhow!(
            "Failed to switch storage to SPI NOR; current: {:?}",
            current
        ));
    }

    let info = dev.flash_info()?;
    let total_sectors = info.sectors();
    ensure!(total_sectors > 0, "SPI NOR reports 0 sectors");
    let total_bytes = info.size();
    println!(
        "SPI NOR: {} KiB ({} sectors)",
        total_bytes / 1024,
        total_sectors,
    );

    let img_file = File::open(&args.image)
        .map_err(|e| anyhow!("failed to open image {}: {}", args.image.display(), e))?;
    let img_len = img_file.metadata()?.len();
    ensure!(
        img_len <= total_bytes,
        "image is {} bytes but SPI NOR is only {} bytes",
        img_len,
        total_bytes,
    );

    // Erase only the sectors we're about to write (ceil(img_len / sector_size)).
    let needed_sectors = img_len.div_ceil(SECTOR_SIZE) as u32;
    let erase_chunk: u32 = 32 * 1024;
    let bar = ProgressBar::new(needed_sectors as u64);
    bar.set_style(
        ProgressStyle::with_template("Erasing SPI NOR... [{bar:40.cyan/blue}] {pos}/{len}")
            .unwrap()
            .progress_chars("=>-"),
    );
    let mut start = 0u32;
    while start < needed_sectors {
        let count = erase_chunk.min(needed_sectors - start);
        dev.erase_lba(start, count as u16)?;
        start += count;
        bar.inc(count as u64);
    }
    bar.finish();

    let mut io = dev.into_io()?;
    println!(
        "Writing image {} -> SPI NOR (LBA 0)...",
        args.image.display()
    );
    let mut img = File::open(&args.image)?;
    io.seek(SeekFrom::Start(0))?;
    let bar = ProgressBar::new(img_len);
    bar.set_style(
        ProgressStyle::with_template("Writing... [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
            .unwrap()
            .progress_chars("=>-"),
    );
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = img.read(&mut buf)?;
        if n == 0 {
            break;
        }
        io.write_all(&buf[..n])?;
        bar.inc(n as u64);
    }
    io.flush()?;
    bar.finish();

    println!("Done.");
    Ok(())
}
