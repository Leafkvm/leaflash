use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail, ensure};
use clap::Args;
use gptman::{GPT, GPTPartitionEntry};
use indicatif::{ProgressBar, ProgressStyle};
use rockusb::device::Device;
use rockusb::libusb::Transport;
use rockusb::protocol::{ResetOpcode, StorageIndex};

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

/// Reservation for GPT primary + protective MBR + backup GPT + alignment slack.
/// 4 MiB is comfortably more than the standard ~67 sectors actually used.
pub const GPT_OVERHEAD_BYTES: u64 = 4 * 1024 * 1024;

/// 20-byte ASCII marker. When `--userdata-magic` is set we write it at the
/// first and last 20 bytes of the userdata partition so the bootloader can
/// detect a "first boot after flash" state and wipe userdata automatically.
pub const USERDATA_MAGIC: &[u8; 20] = b"LEAFKVMUSERDATAMAGIC";

/// Largest single rootfs (A == B) that fits on a card of the given total size,
/// leaving GPT overhead and zero bytes for userdata. Anything larger is invalid.
pub fn max_rootfs_bytes(total_storage_bytes: u64) -> u64 {
    total_storage_bytes.saturating_sub(GPT_OVERHEAD_BYTES) / 2
}

/// The on-disk LBA layout of the three partitions we manage. Used to
/// compare what `flash_image` would write against what's already on the
/// card, so we can skip the full erase + GPT rewrite when nothing about
/// the layout changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub a_start: u64,
    pub a_end: u64,
    pub b_start: u64,
    pub b_end: u64,
    pub user_start: u64,
    pub user_end: u64,
}

// gptman 1.x defaults: 128 partition entries (32 sectors), 1 MiB alignment.
const GPT_ENTRIES_SECTORS: u64 = 32;
const GPT_FIRST_USABLE_LBA: u64 = 2 + GPT_ENTRIES_SECTORS; // 34
const GPT_ALIGN_SECTORS: u64 = 2048;

/// Compute the LBAs that `flash_image` would write for a given disk +
/// rootfs size. Mirrors gptman's defaults exactly so a comparison with
/// the existing on-disk GPT is meaningful.
pub fn expected_layout(total_sectors: u64, rootfs_size_bytes: u64) -> Result<Layout> {
    let rootfs_sectors = rootfs_size_bytes / SECTOR_SIZE;
    let last_usable = total_sectors
        .checked_sub(GPT_ENTRIES_SECTORS + 2)
        .ok_or_else(|| anyhow!("disk too small for a GPT"))?;
    let a_start = align_up(GPT_FIRST_USABLE_LBA, GPT_ALIGN_SECTORS);
    let a_end = a_start + rootfs_sectors - 1;
    let b_start = align_up(a_end + 1, GPT_ALIGN_SECTORS);
    let b_end = b_start + rootfs_sectors - 1;
    let user_start = align_up(b_end + 1, GPT_ALIGN_SECTORS);
    let user_end = last_usable;
    if user_start >= user_end {
        bail!("no room for userdata after rootfs_a + rootfs_b");
    }
    Ok(Layout { a_start, a_end, b_start, b_end, user_start, user_end })
}

/// Read the existing GPT and, if it has rootfs_a / rootfs_b / userdata
/// partitions in slots 1..=3, return their LBAs. Returns None when the
/// disk has no GPT, or has a GPT with different naming/slot usage.
pub fn read_existing_layout<R>(io: &mut R) -> Option<Layout>
where
    R: std::io::Read + std::io::Seek,
{
    let gpt = GPT::find_from(io).ok()?;
    let p1 = &gpt[1];
    let p2 = &gpt[2];
    let p3 = &gpt[3];
    if !p1.is_used() || !p2.is_used() || !p3.is_used() {
        return None;
    }
    if p1.partition_name.as_str() != "rootfs_a"
        || p2.partition_name.as_str() != "rootfs_b"
        || p3.partition_name.as_str() != "userdata"
    {
        return None;
    }
    Some(Layout {
        a_start: p1.starting_lba,
        a_end: p1.ending_lba,
        b_start: p2.starting_lba,
        b_end: p2.ending_lba,
        user_start: p3.starting_lba,
        user_end: p3.ending_lba,
    })
}

#[derive(Debug, Args, Clone)]
pub struct FlashArgs {
    /// Image file to write into rootfs_a (raw, sector-aligned)
    #[arg(long)]
    pub image: PathBuf,
    /// Size of each rootfs partition (A and B), e.g. "256MiB", "512M", "1GiB"
    #[arg(long, value_parser = parse_size)]
    pub rootfs_size: u64,
    /// Reset the device after the image is written (boots into the new image)
    #[arg(long, default_value_t = false)]
    pub reset_after_flash: bool,
    /// Write LEAFKVMUSERDATAMAGIC at the start and end of the userdata
    /// partition. Bootloader treats this as a "first boot after flash"
    /// signal and wipes userdata automatically — without it, the
    /// bootloader asks the user to confirm before wiping.
    #[arg(long, default_value_t = false)]
    pub userdata_magic: bool,
}

/// All inputs to `flash_image`. Add fields here instead of widening the
/// `flash_image` signature.
#[derive(Debug, Clone)]
pub struct Config {
    pub image: PathBuf,
    pub rootfs_size_bytes: u64,
    pub reset_after_flash: bool,
    pub userdata_magic: bool,
}

impl From<&FlashArgs> for Config {
    fn from(a: &FlashArgs) -> Self {
        Self {
            image: a.image.clone(),
            rootfs_size_bytes: a.rootfs_size,
            reset_after_flash: a.reset_after_flash,
            userdata_magic: a.userdata_magic,
        }
    }
}

pub fn run(args: FlashArgs) -> Result<()> {
    let device = device::open_single()?;
    let report = ProgressReporter::Cli;
    let cfg = Config::from(&args);
    flash_image(device, &cfg, &report)?;
    println!(
        "Done.{}",
        if cfg.reset_after_flash { " Device reset." } else { "" }
    );
    Ok(())
}

/// Parse human-readable sizes like "256MiB", "512M", "1GiB". Returns bytes.
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
    n.checked_mul(mult).ok_or_else(|| "size overflow".to_string())
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
    cfg: &Config,
    report: &dyn Report,
) -> Result<()> {
    ensure!(cfg.rootfs_size_bytes > 0, "rootfs size must be > 0");
    ensure!(
        cfg.rootfs_size_bytes % SECTOR_SIZE == 0,
        "rootfs size must be a multiple of {SECTOR_SIZE} bytes"
    );

    let img_file = File::open(&cfg.image)
        .map_err(|e| anyhow!("failed to open image {}: {}", cfg.image.display(), e))?;
    let img_len = img_file.metadata()?.len();
    ensure!(
        img_len <= cfg.rootfs_size_bytes,
        "image is {} MiB ({} bytes) but rootfs partition is {} MiB ({} bytes); \
         pick --rootfs-size >= {} MiB",
        img_len / (1024 * 1024),
        img_len,
        cfg.rootfs_size_bytes / (1024 * 1024),
        cfg.rootfs_size_bytes,
        img_len.div_ceil(1024 * 1024),
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
    let total_bytes = info.size();
    report.stage(&format!(
        "SD card: {} MiB ({} sectors)",
        total_bytes / (1024 * 1024),
        total_sectors
    ));

    let max = max_rootfs_bytes(total_bytes);
    if cfg.rootfs_size_bytes > max {
        bail!(
            "rootfs size {} MiB > {} MiB (half of SD minus GPT overhead). \
             Pick --rootfs-size <= {} MiB or use a larger card.",
            cfg.rootfs_size_bytes / (1024 * 1024),
            max / (1024 * 1024),
            max / (1024 * 1024)
        );
    }

    let layout = expected_layout(total_sectors as u64, cfg.rootfs_size_bytes)?;

    // Inspect the existing GPT. If rootfs_a / rootfs_b / userdata are
    // already at the LBAs we'd write, the flash is just an in-place
    // rootfs_a refresh: skip the full erase and the GPT rewrite, leaving
    // rootfs_b and userdata intact. Userdata-magic still writes its
    // markers separately.
    let mut io = device.into_io()?;
    let existing = read_existing_layout(&mut io);
    let preserve = existing == Some(layout);
    let mut device = io.into_inner();

    if preserve {
        report.stage(
            "Existing GPT matches; preserving rootfs_b and userdata, refreshing rootfs_a only.",
        );
        let a_count = layout.a_end - layout.a_start + 1;
        let mut bar = report.progress_begin(a_count, "Erasing rootfs_a...");
        chunk_erase(&mut device, layout.a_start as u32, a_count as u32, &mut *bar)?;
        bar.finish();
    } else {
        report.stage(&format!(
            "Erasing whole card ({} sectors)...",
            total_sectors
        ));
        let mut bar = report.progress_begin(total_sectors as u64, "Erasing card...");
        chunk_erase(&mut device, 0, total_sectors, &mut *bar)?;
        bar.finish();
    }

    std::thread::sleep(std::time::Duration::from_millis(100));

    let mut io = device.into_io()?;

    if !preserve {
        report.stage("Generating GPT (rootfs_a / rootfs_b / userdata)...");
        let mut gpt = GPT::new_from(&mut io, SECTOR_SIZE, random_guid())?;
        gpt[1] = GPTPartitionEntry {
            partition_type_guid: LINUX_FS_GUID,
            unique_partition_guid: random_guid(),
            starting_lba: layout.a_start,
            ending_lba: layout.a_end,
            attribute_bits: ATTR_LEGACY_BIOS_BOOTABLE,
            partition_name: "rootfs_a".into(),
        };
        gpt[2] = GPTPartitionEntry {
            partition_type_guid: LINUX_FS_GUID,
            unique_partition_guid: random_guid(),
            starting_lba: layout.b_start,
            ending_lba: layout.b_end,
            attribute_bits: 0,
            partition_name: "rootfs_b".into(),
        };
        gpt[3] = GPTPartitionEntry {
            partition_type_guid: LINUX_FS_GUID,
            unique_partition_guid: random_guid(),
            starting_lba: layout.user_start,
            ending_lba: layout.user_end,
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
    }

    report.stage(&format!(
        "Writing image {} -> rootfs_a (LBA {})...",
        cfg.image.display(),
        layout.a_start,
    ));
    let mut img = File::open(&cfg.image)?;
    io.seek(SeekFrom::Start(layout.a_start * SECTOR_SIZE))?;
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

    if cfg.userdata_magic {
        report.stage(
            "Writing userdata magic markers (bootloader will auto-wipe userdata on next boot)...",
        );
        let userdata_first_byte = layout.user_start * SECTOR_SIZE;
        let userdata_last_byte = (layout.user_end + 1) * SECTOR_SIZE;
        let trailing_offset = userdata_last_byte - USERDATA_MAGIC.len() as u64;

        io.seek(SeekFrom::Start(userdata_first_byte))?;
        io.write_all(USERDATA_MAGIC)?;

        io.seek(SeekFrom::Start(trailing_offset))?;
        io.write_all(USERDATA_MAGIC)?;

        io.flush()?;
    }

    if cfg.reset_after_flash {
        report.stage("Resetting device...");
        let mut device = io.into_inner();
        // The reset itself yanks the USB endpoint; rockusb may surface that
        // as an error even though the reset succeeded. Don't fail the flash.
        if let Err(e) = device.reset_device(ResetOpcode::Reset) {
            report.stage(&format!("Reset returned {} (often expected — USB disconnects mid-call)", e));
        }
    }

    Ok(())
}

fn chunk_erase(
    device: &mut Device<Transport>,
    first: u32,
    count: u32,
    bar: &mut dyn ProgressHandle,
) -> Result<()> {
    let chunk: u32 = 32 * 1024;
    let mut remaining = count;
    let mut start = first;
    while remaining > 0 {
        let n = chunk.min(remaining);
        device.erase_lba(start, n as u16)?;
        start = start.saturating_add(n);
        remaining -= n;
        bar.inc(n as u64);
    }
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
