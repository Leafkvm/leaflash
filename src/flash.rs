use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail, ensure};
use clap::Args;
use gptman::{GPT, GPTPartitionEntry};
use indicatif::{ProgressBar, ProgressStyle};
use rockusb::device::Device;
use rockusb::libusb::Transport;
use rockusb::protocol::StorageIndex;

use crate::device;

pub const SECTOR_SIZE: u64 = 512;

/// Linux filesystem GUID: 0FC63DAF-8483-4772-8E79-3D69D8477DE4 (mixed-endian)
const LINUX_FS_GUID: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];

const ATTR_LEGACY_BIOS_BOOTABLE: u64 = 0x4;

/// Default rounding granularity used by the TUI when the user doesn't
/// override the rootfs size. Exported so the TUI stays consistent.
pub const DEFAULT_ROUND_MIB: u64 = 128;

#[derive(Debug, Args, Clone)]
pub struct FlashArgs {
    /// Image file to write into rootfs_a (raw, sector-aligned)
    #[arg(long)]
    pub image: PathBuf,
    /// Size of each rootfs partition (A and B), e.g. "256MiB", "512M", "1GiB"
    #[arg(long, value_parser = parse_size)]
    pub rootfs_size: u64,
}

pub fn run(args: FlashArgs) -> Result<()> {
    let device = device::open_single()?;
    let report = ProgressReporter::Cli;
    flash_image(device, &args.image, args.rootfs_size, &report)?;
    println!("Done.");
    Ok(())
}

/// Parse human-readable sizes like "256MiB", "512M", "1GiB".
/// Returns size in bytes.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, unit) = s
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| s.split_at(i))
        .unwrap_or((s, ""));
    let n: u64 = num.parse().map_err(|_| format!("invalid number: {num}"))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1_000,
        "kib" => 1024,
        "m" | "mb" => 1_000_000,
        "mib" => 1024 * 1024,
        "g" | "gb" => 1_000_000_000,
        "gib" => 1024 * 1024 * 1024,
        other => return Err(format!("unknown size unit: {other}")),
    };
    Ok(n.checked_mul(mult).ok_or("size overflow")?)
}

/// Round a byte count up to the nearest multiple of `granularity` MiB.
pub fn round_up_mib(bytes: u64, granularity_mib: u64) -> u64 {
    let g = granularity_mib * 1024 * 1024;
    if g == 0 { return bytes; }
    bytes.div_ceil(g) * g
}

/// What the flash workflow reports while running. Kept abstract so the
/// TUI can render it without us depending on indicatif there.
pub trait Report {
    fn stage(&self, msg: &str);
    fn progress_begin(&self, total: u64, msg: &str) -> Box<dyn ProgressHandle>;
}

pub trait ProgressHandle {
    fn inc(&mut self, delta: u64);
    fn finish(self: Box<Self>);
}

pub enum ProgressReporter {
    Cli,
}

impl Report for ProgressReporter {
    fn stage(&self, msg: &str) {
        println!("{msg}");
    }
    fn progress_begin(&self, total: u64, msg: &str) -> Box<dyn ProgressHandle> {
        let bar = ProgressBar::new(total);
        bar.set_style(
            ProgressStyle::with_template("{msg} [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
                .unwrap()
                .progress_chars("=>-"),
        );
        bar.set_message(msg.to_string());
        Box::new(CliProgress(bar))
    }
}

struct CliProgress(ProgressBar);
impl ProgressHandle for CliProgress {
    fn inc(&mut self, delta: u64) {
        self.0.inc(delta);
    }
    fn finish(self: Box<Self>) {
        self.0.finish();
    }
}

/// The single source of truth for the flash workflow. Both the `flash`
/// subcommand and the TUI route through here.
pub fn flash_image(
    mut device: Device<Transport>,
    image: &Path,
    rootfs_size_bytes: u64,
    report: &dyn Report,
) -> Result<()> {
    ensure!(rootfs_size_bytes > 0, "rootfs size must be > 0");
    ensure!(
        rootfs_size_bytes % SECTOR_SIZE == 0,
        "rootfs size must be a multiple of {SECTOR_SIZE} bytes"
    );

    let img_file = File::open(image)
        .map_err(|e| anyhow!("failed to open image {}: {}", image.display(), e))?;
    let img_len = img_file.metadata()?.len();
    ensure!(
        img_len <= rootfs_size_bytes,
        "image is {} bytes but rootfs partition is only {} bytes",
        img_len,
        rootfs_size_bytes
    );

    report.stage("Switching storage to SD...");
    device.switch_storage(StorageIndex::Sd)?;
    let current = device.get_storage()?;
    if current != StorageIndex::Sd {
        bail!("Failed to switch storage to SD; current: {:?}", current);
    }

    let info = device.flash_info()?;
    let total_sectors = info.sectors();
    ensure!(total_sectors > 0, "SD card reports 0 sectors");
    report.stage(&format!(
        "SD card: {} MiB ({} sectors)",
        info.size() / (1024 * 1024),
        total_sectors
    ));

    let rootfs_sectors = rootfs_size_bytes / SECTOR_SIZE;
    let needed_sectors = rootfs_sectors
        .checked_mul(2)
        .and_then(|x| x.checked_add(2048))
        .ok_or_else(|| anyhow!("rootfs sizing overflow"))?;
    ensure!(
        total_sectors as u64 >= needed_sectors,
        "SD card too small: need at least {} sectors for 2x rootfs, have {}",
        needed_sectors,
        total_sectors
    );

    let mut bar = report.progress_begin(total_sectors as u64, "Erasing card...");
    let erase_chunk: u32 = 32 * 1024;
    let mut start: u32 = 0;
    while start < total_sectors {
        let count = erase_chunk.min(total_sectors - start);
        device.erase_lba(start, count as u16)?;
        start += count;
        bar.inc(count as u64);
    }
    bar.finish();

    std::thread::sleep(std::time::Duration::from_millis(100));

    report.stage("Generating GPT (rootfs_a / rootfs_b / userdata)...");
    let mut io = device.into_io()?;
    let mut gpt = GPT::new_from(&mut io, SECTOR_SIZE, random_guid())?;

    let a_start = align_up(gpt.header.first_usable_lba, gpt.align);
    let a_end = a_start + rootfs_sectors - 1;
    gpt[1] = GPTPartitionEntry {
        partition_type_guid: LINUX_FS_GUID,
        unique_partition_guid: random_guid(),
        starting_lba: a_start,
        ending_lba: a_end,
        attribute_bits: ATTR_LEGACY_BIOS_BOOTABLE,
        partition_name: "rootfs_a".into(),
    };

    let b_start = align_up(a_end + 1, gpt.align);
    let b_end = b_start + rootfs_sectors - 1;
    gpt[2] = GPTPartitionEntry {
        partition_type_guid: LINUX_FS_GUID,
        unique_partition_guid: random_guid(),
        starting_lba: b_start,
        ending_lba: b_end,
        attribute_bits: 0,
        partition_name: "rootfs_b".into(),
    };

    let user_start = align_up(b_end + 1, gpt.align);
    let user_end = gpt.header.last_usable_lba;
    ensure!(
        user_end > user_start,
        "no room left for userdata after rootfs_a + rootfs_b"
    );
    gpt[3] = GPTPartitionEntry {
        partition_type_guid: LINUX_FS_GUID,
        unique_partition_guid: random_guid(),
        starting_lba: user_start,
        ending_lba: user_end,
        attribute_bits: 0,
        partition_name: "userdata".into(),
    };

    report.stage("Writing protective MBR + primary/backup GPT...");
    GPT::write_protective_mbr_into(&mut io, SECTOR_SIZE)?;
    gpt.write_into(&mut io)?;
    io.flush()?;

    for (i, p) in gpt.iter() {
        if p.is_used() {
            report.stage(&format!(
                "  partition {}: {} {} MiB (LBA {}..={})",
                i,
                p.partition_name,
                p.size().unwrap_or(0) * SECTOR_SIZE / (1024 * 1024),
                p.starting_lba,
                p.ending_lba,
            ));
        }
    }

    report.stage(&format!(
        "Writing image {} -> rootfs_a (LBA {})...",
        image.display(),
        a_start
    ));
    let mut img = File::open(image)?;
    io.seek(SeekFrom::Start(a_start * SECTOR_SIZE))?;
    let mut bar = report.progress_begin(img_len, "Writing image...");
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = img.read(&mut buf)?;
        if n == 0 { break; }
        io.write_all(&buf[..n])?;
        bar.inc(n as u64);
    }
    io.flush()?;
    bar.finish();

    Ok(())
}

fn random_guid() -> [u8; 16] {
    let mut g = [0u8; 16];
    for b in &mut g { *b = fastrand::u8(..); }
    g
}

fn align_up(value: u64, align: u64) -> u64 {
    if align == 0 { return value; }
    value.div_ceil(align) * align
}
