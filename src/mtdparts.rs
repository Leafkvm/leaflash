//! Parser for the kernel `mtdparts=` syntax used by U-Boot / Linux MTD.
//!
//! Format (single device — multi-device strings separated by `;` are
//! intentionally not supported here):
//!
//!     mtdparts=<mtd-id>:<part>[,<part>]*
//!     part = <size>[@<offset>](<name>)[<ro-flag>][<lk-flag>]
//!
//! `<size>` and `<offset>` accept k/K (1024), m/M (1024²), g/G (1024³),
//! or no suffix (raw bytes). `<size>` may be `-` meaning "rest of the
//! device". When `@<offset>` is omitted the partition starts where the
//! previous one ended (or at 0 for the first).
//!
//! This is the standard kernel format documented in `cmdlinepart.c`.

use anyhow::{Result, anyhow, bail, ensure};

#[derive(Debug, Clone)]
pub struct Partition {
    pub name: String,
    /// Offset in bytes from the start of the flash device.
    pub offset: u64,
    /// Size in bytes, or `None` for the trailing `-` (rest of device).
    pub size: Option<u64>,
}

/// Parse a single-device `mtdparts=...` string and return its partitions
/// in declaration order, with offsets resolved.
pub fn parse(s: &str) -> Result<Vec<Partition>> {
    let s = s.trim().strip_prefix("mtdparts=").unwrap_or_else(|| s.trim());
    let (_dev, rest) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("mtdparts: missing ':' separating <mtd-id> from partitions"))?;

    let mut out = Vec::new();
    let mut next_offset: u64 = 0;
    for spec in rest.split(',') {
        let spec = spec.trim();
        ensure!(!spec.is_empty(), "mtdparts: empty partition spec");

        let lparen = spec
            .find('(')
            .ok_or_else(|| anyhow!("mtdparts: missing '(' in '{}'", spec))?;
        let rparen = spec
            .find(')')
            .ok_or_else(|| anyhow!("mtdparts: missing ')' in '{}'", spec))?;
        ensure!(lparen < rparen, "mtdparts: malformed '{}'", spec);

        let head = &spec[..lparen];
        let name = spec[lparen + 1..rparen].to_string();

        let (size_str, offset_str) = match head.find('@') {
            Some(i) => (&head[..i], Some(&head[i + 1..])),
            None => (head, None),
        };

        let size = if size_str == "-" {
            None
        } else {
            Some(parse_size(size_str)?)
        };
        let offset = match offset_str {
            Some(s) => parse_size(s)?,
            None => next_offset,
        };
        if let Some(sz) = size {
            next_offset = offset
                .checked_add(sz)
                .ok_or_else(|| anyhow!("mtdparts: offset overflow at '{}'", name))?;
        }

        out.push(Partition { name, offset, size });
    }
    if out.is_empty() {
        bail!("mtdparts: no partitions");
    }
    Ok(out)
}

fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    ensure!(!s.is_empty(), "mtdparts: empty size");
    let last = s.chars().last().unwrap();
    let (digits, mult) = if last.is_ascii_digit() {
        (s, 1u64)
    } else {
        let m = match last.to_ascii_lowercase() {
            'k' => 1024,
            'm' => 1024 * 1024,
            'g' => 1024 * 1024 * 1024,
            _ => bail!("mtdparts: unknown size suffix '{}' in '{}'", last, s),
        };
        (&s[..s.len() - 1], m)
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| anyhow!("mtdparts: invalid number '{}'", digits))?;
    n.checked_mul(mult)
        .ok_or_else(|| anyhow!("mtdparts: size overflow in '{}'", s))
}

