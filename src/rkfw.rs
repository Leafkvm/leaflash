//! Minimal parser for the Rockchip update-image format (`rkImageMaker`
//! + `afptool` output, the file `upgrade_tool UF` consumes).
//!
//! Layout:
//! - 0x00..0x66  RKFW header (only the four offset/length fields in here
//!   are interesting to us; they're stored as **unaligned** little-endian
//!   `u32`s at byte offsets 0x19, 0x1d, 0x21, 0x25)
//! - `loader_off..+loader_len`  raw loader bundle (`LDR ` magic — the
//!   thing maskrom expects via `download-boot`)
//! - `image_off..+image_len`    RKAF section:
//!   - 0x00..0x88  RKAF header (magic + length + reserved fields)
//!   - 0x88..0x8c  `num_files` (LE u32)
//!   - 0x8c..      `num_files` entries of 112 bytes each:
//!     - `name[32]` partition / virtual entry name
//!     - `path[60]` original file name
//!     - 5 LE u32 fields:
//!         partition_sectors,
//!         data_offset (within RKAF),
//!         target_sector (== 0xFFFF_FFFF when entry isn't flashed),
//!         (unused — possibly chunk count or CRC offset),
//!         size_bytes,
//!
//! Field positions and sizes were reverse-engineered from a real
//! rkImageMaker output for RV1126B; they match the values the picture-
//! perfect parsers in rkdeveloptool and afptool use too.

use anyhow::{Result, anyhow, bail, ensure};

#[derive(Debug)]
pub struct Package<'a> {
    /// The maskrom boot bundle from the RKFW container. Currently parsed
    /// but unused by the `uboot` subcommand (the user pushes the loader
    /// themselves via `leaflash usb download-boot` before invoking
    /// `uboot`); kept around so a future "auto-bootstrap" path can use
    /// it without changing the parser API.
    #[allow(dead_code)]
    pub loader: &'a [u8],
    pub entries: Vec<Entry<'a>>,
}

#[derive(Debug)]
pub struct Entry<'a> {
    pub name: String,
    pub data: &'a [u8],
    /// `None` when the entry is metadata (`package-file`) or flashed via a
    /// different path (`bootloader` → `download-boot`).
    pub target_sector: Option<u32>,
    /// Partition size in 512-byte sectors as recorded in the RKAF entry.
    /// Used to size the erase region so the whole partition is wiped
    /// even when the file is shorter than the partition.
    pub partition_sectors: u32,
}

const RKFW_HEADER_MIN: usize = 0x66;
const ENTRY_SIZE: usize = 112;

pub fn parse(data: &[u8]) -> Result<Package<'_>> {
    ensure!(
        data.len() >= RKFW_HEADER_MIN,
        "file is shorter than the RKFW header"
    );
    ensure!(
        &data[0..4] == b"RKFW",
        "not a Rockchip update.img (expected magic \"RKFW\" at offset 0)"
    );

    let read_u32 = |off: usize| -> Result<u32> {
        ensure!(off + 4 <= data.len(), "RKFW header truncated at 0x{:x}", off);
        Ok(u32::from_le_bytes(data[off..off + 4].try_into().unwrap()))
    };
    let loader_off = read_u32(0x19)? as usize;
    let loader_len = read_u32(0x1d)? as usize;
    let image_off = read_u32(0x21)? as usize;
    let image_len = read_u32(0x25)? as usize;

    ensure!(
        loader_off + loader_len <= data.len(),
        "loader range out of bounds: {}..{} (file size {})",
        loader_off,
        loader_off + loader_len,
        data.len()
    );
    ensure!(
        image_off + image_len <= data.len(),
        "image range out of bounds: {}..{} (file size {})",
        image_off,
        image_off + image_len,
        data.len()
    );

    let loader = &data[loader_off..loader_off + loader_len];
    ensure!(
        loader.len() >= 4 && &loader[0..4] == b"LDR ",
        "loader section missing 'LDR ' magic"
    );

    let rkaf = &data[image_off..image_off + image_len];
    ensure!(
        rkaf.len() >= 0x8c + 4,
        "RKAF section too small ({} bytes)",
        rkaf.len()
    );
    ensure!(
        &rkaf[0..4] == b"RKAF",
        "expected 'RKAF' magic at start of image section"
    );

    let num_files = u32::from_le_bytes(rkaf[0x88..0x8c].try_into().unwrap()) as usize;
    let entries_end = 0x8c + num_files.checked_mul(ENTRY_SIZE).ok_or_else(|| {
        anyhow!("num_files too large: {}", num_files)
    })?;
    ensure!(
        entries_end <= rkaf.len(),
        "RKAF entries overflow the section"
    );

    let mut entries = Vec::with_capacity(num_files);
    for i in 0..num_files {
        let base = 0x8c + i * ENTRY_SIZE;
        let name = read_cstr(&rkaf[base..base + 32]);

        let partition_sectors =
            u32::from_le_bytes(rkaf[base + 92..base + 96].try_into().unwrap());
        let data_off = u32::from_le_bytes(rkaf[base + 96..base + 100].try_into().unwrap()) as usize;
        let target_sector = u32::from_le_bytes(rkaf[base + 100..base + 104].try_into().unwrap());
        // base+104..108 is unused (best guess: chunk count); skip.
        let size_bytes =
            u32::from_le_bytes(rkaf[base + 108..base + 112].try_into().unwrap()) as usize;

        ensure!(
            data_off
                .checked_add(size_bytes)
                .map(|end| end <= rkaf.len())
                .unwrap_or(false),
            "entry {:?} data range {}..{} out of bounds",
            name,
            data_off,
            data_off + size_bytes
        );

        let data_slice = &rkaf[data_off..data_off + size_bytes];
        let target_sector = if target_sector == 0xFFFF_FFFF {
            None
        } else {
            Some(target_sector)
        };
        entries.push(Entry {
            name,
            data: data_slice,
            target_sector,
            partition_sectors,
        });
    }

    if entries.is_empty() {
        bail!("RKAF section reports zero entries");
    }

    Ok(Package { loader, entries })
}

fn read_cstr(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}
