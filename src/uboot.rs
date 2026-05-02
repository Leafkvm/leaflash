use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail, ensure};
use clap::Args;
use indicatif::{ProgressBar, ProgressStyle};
use rockusb::device::Device;
use rockusb::libusb::Transport;
use rockusb::protocol::{ResetOpcode, StorageIndex};

use crate::device;
use crate::flash::SECTOR_SIZE;
use crate::mtdparts;
use crate::uboot_env;

/// Flash an image set to the device's SPI NOR. The artifact directory
/// must contain `env.img` (a U-Boot environment image whose `mtdparts`
/// declares the SPI-NOR layout) plus one `<name>.img` (or just `<name>`)
/// per declared partition. We open env.img, parse the mtdparts string,
/// and write each partition file at the offset mtdparts assigns it.
///
/// If `env.img` isn't directly inside the artifact dir we walk subdirs
/// up to a few levels deep; partition files are looked up next to
/// whichever env.img we found.
#[derive(Debug, Args)]
pub struct UbootArgs {
    /// Artifact directory (e.g. spi-flash-img-builder's `output/`).
    /// Must contain `env.img` plus the per-partition `.img` files.
    #[arg(short = 'a', long)]
    pub artifact_dir: PathBuf,
    /// Skip the device reset that runs after a successful flash.
    /// Default behaviour is to reset so the device reboots into the
    /// freshly-written firmware.
    #[arg(long)]
    pub no_reset: bool,
}

struct Job {
    name: String,
    target_sector: u32,
    erase_sectors: u32,
    data: Vec<u8>,
    file_path: PathBuf,
}

pub fn run(args: UbootArgs) -> Result<()> {
    let env_path = find_env_img(&args.artifact_dir)?;
    let img_dir = env_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    println!("Using env.img at {}", env_path.display());

    let env_bytes = std::fs::read(&env_path)
        .map_err(|e| anyhow!("failed to read {}: {}", env_path.display(), e))?;
    let env = uboot_env::parse(&env_bytes)?;
    let mtdparts_str = env
        .get("mtdparts")
        .ok_or_else(|| anyhow!("env.img has no `mtdparts` variable"))?;
    println!("mtdparts: {}", mtdparts_str);
    let parts = mtdparts::parse(mtdparts_str)?;

    let mut jobs = build_jobs(&parts, &img_dir, &env_path)?;
    jobs.sort_by_key(|j| j.target_sector);
    if jobs.is_empty() {
        bail!(
            "no partition files found alongside {} (looked for <name>.img and <name>)",
            env_path.display()
        );
    }

    flash_jobs(jobs, !args.no_reset)
}

fn find_env_img(root: &Path) -> Result<PathBuf> {
    fn walk(dir: &Path, depth: usize) -> Option<PathBuf> {
        let candidate = dir.join("env.img");
        if candidate.is_file() {
            return Some(candidate);
        }
        if depth == 0 {
            return None;
        }
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            if let Ok(t) = entry.file_type() {
                if t.is_dir() {
                    if let Some(p) = walk(&entry.path(), depth - 1) {
                        return Some(p);
                    }
                }
            }
        }
        None
    }
    walk(root, 4).ok_or_else(|| {
        anyhow!(
            "no env.img found under {} (searched 4 levels deep)",
            root.display()
        )
    })
}

fn build_jobs(parts: &[mtdparts::Partition], dir: &Path, env_path: &Path) -> Result<Vec<Job>> {
    let mut jobs = Vec::new();
    for p in parts {
        let size = p.size.ok_or_else(|| {
            anyhow!(
                "partition '{}' has size '-' (rest of device); not supported",
                p.name
            )
        })?;
        ensure!(
            p.offset % SECTOR_SIZE == 0,
            "partition '{}' offset {} not sector-aligned",
            p.name,
            p.offset
        );
        ensure!(
            size % SECTOR_SIZE == 0,
            "partition '{}' size {} not sector-aligned",
            p.name,
            size
        );

        // The env partition's data IS the env image we just parsed.
        // Use it as-is rather than re-reading.
        if p.name == "env" {
            let data = std::fs::read(env_path)?;
            ensure!(
                (data.len() as u64) <= size,
                "env.img ({} bytes) exceeds partition '{}' ({} bytes)",
                data.len(),
                p.name,
                size
            );
            jobs.push(Job {
                name: p.name.clone(),
                target_sector: (p.offset / SECTOR_SIZE) as u32,
                erase_sectors: (size / SECTOR_SIZE) as u32,
                data,
                file_path: env_path.to_path_buf(),
            });
            continue;
        }

        let candidates = [dir.join(format!("{}.img", p.name)), dir.join(&p.name)];
        let Some(file_path) = candidates.iter().find(|c| c.is_file()).cloned() else {
            eprintln!(
                "Skipping partition '{}': no '{}.img' or '{}' in {}",
                p.name,
                p.name,
                p.name,
                dir.display()
            );
            continue;
        };
        let data = std::fs::read(&file_path)
            .map_err(|e| anyhow!("failed to read {}: {}", file_path.display(), e))?;
        ensure!(
            (data.len() as u64) <= size,
            "{} ({} bytes) is larger than partition '{}' ({} bytes)",
            file_path.display(),
            data.len(),
            p.name,
            size
        );
        jobs.push(Job {
            name: p.name.clone(),
            target_sector: (p.offset / SECTOR_SIZE) as u32,
            erase_sectors: (size / SECTOR_SIZE) as u32,
            data,
            file_path,
        });
    }
    Ok(jobs)
}

fn flash_jobs(jobs: Vec<Job>, reset_after: bool) -> Result<()> {
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

    // Capability-aware erase chunking — SPI NOR is non-eMMC + non-direct-LBA,
    // so it must use erase_force at 1024-sector chunks; eMMC / direct-LBA
    // can take erase_lba at 32K-sector chunks.
    let cap = dev.capability()?;
    let id = dev.flash_id()?;
    let is_emmc = id.to_str() == "EMMC ";
    let is_lba = cap.direct_lba();
    let max_blocks: u32 = if is_emmc || is_lba { 32 * 1024 } else { 1024 };
    let use_lba = is_emmc || is_lba;

    println!("Partitions to flash:");
    for j in &jobs {
        println!(
            "  {:14} LBA {:>6}  partition {:>5} sectors  ({} bytes from {})",
            j.name,
            j.target_sector,
            j.erase_sectors,
            j.data.len(),
            j.file_path.display(),
        );
    }

    for job in &jobs {
        let end = job
            .target_sector
            .checked_add(job.erase_sectors)
            .ok_or_else(|| anyhow!("LBA overflow for {}", job.name))?;
        ensure!(
            end as u64 <= total_sectors as u64,
            "{} (LBA {}..{}) exceeds SPI NOR size ({} sectors)",
            job.name,
            job.target_sector,
            end,
            total_sectors
        );

        erase_range(
            &mut dev,
            job.target_sector,
            job.erase_sectors,
            max_blocks,
            use_lba,
            &job.name,
        )?;
        write_data(&mut dev, job.target_sector, &job.data, &job.name)?;
    }

    if reset_after {
        println!("Resetting device...");
        // The reset itself yanks the USB endpoint, so rockusb often surfaces
        // an error even though the reset succeeded. Don't let that fail the
        // flash — surface it as a warning and exit cleanly.
        if let Err(e) = dev.reset_device(ResetOpcode::Reset) {
            eprintln!(
                "Reset returned {e} (often expected — USB disconnects mid-call)",
            );
        }
    }

    println!("Done.{}", if reset_after { " Device reset." } else { "" });
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

fn write_data(
    dev: &mut Device<Transport>,
    target_sector: u32,
    data: &[u8],
    name: &str,
) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    let img_len = data.len() as u64;
    let mut io = dev.io()?;
    io.seek(SeekFrom::Start(target_sector as u64 * SECTOR_SIZE))?;
    let bar = ProgressBar::new(img_len);
    bar.set_style(
        ProgressStyle::with_template("Writing {msg} [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
            .unwrap()
            .progress_chars("=>-"),
    );
    bar.set_message(name.to_string());

    let mut written: u64 = 0;
    let chunk = 64 * 1024usize;
    while written < img_len {
        let n = chunk.min((img_len - written) as usize);
        let s = written as usize;
        io.write_all(&data[s..s + n])?;
        written += n as u64;
        bar.inc(n as u64);
    }
    io.flush()?;
    bar.finish();
    Ok(())
}
