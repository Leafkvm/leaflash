//! Parser for U-Boot environment images, as produced by `mkenvimage`
//! (`tools/mkenvimage.c` in the U-Boot tree). Format the writer
//! actually emits, paraphrased from that file:
//!
//! - bytes 0..4: CRC32 over everything that follows (bytes 4..end_of_image,
//!   including any padding). Little-endian by default; big-endian if
//!   mkenvimage was invoked with `-b`.
//! - byte 4 (only if mkenvimage was invoked with `-r`): redundant-env
//!   "active flags" byte, written as 0x01.
//! - then NUL-separated `KEY=VALUE` entries, terminated by an empty
//!   entry (i.e. a double NUL).
//! - the rest of the image is filled with the pad byte (0xFF by default;
//!   the rockchip pack-update script uses `-p 0x0` so the padding is NUL).
//!
//! The CRC covers the flags byte (when present) and the padding. We try
//! both endiannesses for the stored CRC, so this works on either an
//! `-b`-built env or a default one.

use anyhow::{Result, anyhow, ensure};

#[derive(Debug)]
pub struct UbootEnv {
    pub vars: Vec<(String, String)>,
}

impl UbootEnv {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

pub fn parse(env: &[u8]) -> Result<UbootEnv> {
    ensure!(env.len() >= 5, "env image too small ({} bytes)", env.len());

    // Validate CRC. Mismatch is fatal — if the bytes don't checksum we
    // can't trust the mtdparts string we're about to act on.
    let stored_le = u32::from_le_bytes(env[0..4].try_into().unwrap());
    let stored_be = u32::from_be_bytes(env[0..4].try_into().unwrap());
    let computed = crc32fast::hash(&env[4..]);
    if stored_le != computed && stored_be != computed {
        return Err(anyhow!(
            "U-Boot env CRC32 mismatch: stored LE=0x{:08x} BE=0x{:08x}, computed=0x{:08x} \
             (file is {} bytes)",
            stored_le,
            stored_be,
            computed,
            env.len()
        ));
    }

    // Detect mkenvimage -r layout. With -r, byte 4 is a single flag byte
    // (0x00 or 0x01) and entries start at byte 5; otherwise entries start
    // at byte 4. The first byte of a normal entry is part of an env var
    // name (an ASCII letter or '_'), so we use that to disambiguate.
    let data_start = if matches!(env[4], 0 | 1)
        && env.len() > 5
        && (env[5].is_ascii_alphabetic() || env[5] == b'_')
    {
        5
    } else {
        4
    };
    let data = &env[data_start..];

    let mut vars = Vec::new();
    let mut start = 0;
    while start < data.len() {
        // mkenvimage uses NUL as the entry terminator and a final empty
        // (double-NUL) entry as the end marker. Anything we hit that
        // isn't a `KEY=VALUE\0` pattern is the trailing padding region.
        let nul = match data[start..].iter().position(|&b| b == 0) {
            Some(0) => break, // empty entry → end of meaningful data
            Some(i) => start + i,
            None => break,    // no more NULs = padding to EOF
        };
        let entry = &data[start..nul];
        let Some(eq) = entry.iter().position(|&b| b == b'=') else {
            // Not a KEY=VALUE pair — treat as garbage / padding and stop.
            break;
        };
        let key = std::str::from_utf8(&entry[..eq])
            .map_err(|_| anyhow!("U-Boot env entry has non-UTF-8 key"))?
            .to_string();
        let val = std::str::from_utf8(&entry[eq + 1..])
            .map_err(|_| anyhow!("U-Boot env entry has non-UTF-8 value"))?
            .to_string();
        vars.push((key, val));
        start = nul + 1;
    }

    Ok(UbootEnv { vars })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(payload: &[u8], total_size: usize) -> Vec<u8> {
        // Pad payload with NULs (to mimic mkenvimage -p 0x0 output) and
        // prepend a correct LE CRC32.
        let mut data = vec![0u8; total_size];
        data[4..4 + payload.len()].copy_from_slice(payload);
        let crc = crc32fast::hash(&data[4..]);
        data[..4].copy_from_slice(&crc.to_le_bytes());
        data
    }

    #[test]
    fn parses_a_single_var() {
        let payload = b"mtdparts=sfc_nor:64K(env)\0\0";
        let env = parse(&build(payload, 256)).unwrap();
        assert_eq!(env.vars.len(), 1);
        assert_eq!(env.vars[0].0, "mtdparts");
        assert_eq!(env.vars[0].1, "sfc_nor:64K(env)");
    }

    #[test]
    fn parses_multiple_vars_and_get() {
        let payload = b"foo=bar\0baz=qux\0mtdparts=x:1K(p1)\0\0";
        let env = parse(&build(payload, 256)).unwrap();
        assert_eq!(env.get("foo"), Some("bar"));
        assert_eq!(env.get("baz"), Some("qux"));
        assert_eq!(env.get("mtdparts"), Some("x:1K(p1)"));
        assert_eq!(env.get("missing"), None);
    }

    #[test]
    fn rejects_bad_crc() {
        let mut img = build(b"foo=bar\0\0", 256);
        img[0] ^= 0xff;
        let err = parse(&img).unwrap_err().to_string();
        assert!(err.contains("CRC32"), "expected CRC error, got: {err}");
    }

    #[test]
    fn handles_be_crc() {
        // mkenvimage -b stores the CRC as big-endian; the parser must
        // accept either endianness.
        let mut img = build(b"foo=bar\0\0", 256);
        let crc = crc32fast::hash(&img[4..]);
        img[..4].copy_from_slice(&crc.to_be_bytes());
        let env = parse(&img).unwrap();
        assert_eq!(env.get("foo"), Some("bar"));
    }
}

