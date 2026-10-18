//! Manifest parsing and the hash -> filename/URL derivations.
//!
//! Ported 1:1 from the verified Python reference (uma_downloader.py and
//! REVERSE_ENGINEERING.md). The binary layout and hash formula must not change.

use anyhow::{anyhow, Result};
use data_encoding::BASE32_NOPAD;
use sha1::{Digest, Sha1};

pub const CDN: &str = "https://assets-umamusume-en.akamaized.net/dl/vertical";

/// Categories whose assets live under /resources/Generic/ (Master/Sound/Movie/Font).
const GENERIC_CATEGORIES: [&[u8]; 4] = [b"master", b"sound", b"movie", b"font"];

pub struct PlatformEntry {
    pub name: Vec<u8>,
    pub size: u64,
    pub checksum: [u8; 8],
}

pub struct CategoryEntry {
    pub names: Vec<Vec<u8>>,
    pub size: u64,
    pub h: [u8; 16],
}

/// A single downloadable asset, flattened from a category manifest entry.
pub struct Asset {
    pub category: Vec<u8>,
    pub name: Vec<u8>,
    pub size: u64,
    pub h: [u8; 16],
}

fn b32(raw: &[u8]) -> String {
    BASE32_NOPAD.encode(raw)
}

/// MSB-first varint. Returns (value, next_index) — here just the value; the
/// cursor is advanced via `i`.
fn read_varint(data: &[u8], i: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    loop {
        let b = *data
            .get(*i)
            .ok_or_else(|| anyhow!("varint out of bounds at offset {}", *i))?;
        *i += 1;
        v = (v << 7) | (b & 0x7F) as u64;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
}

/// Index of the first NUL byte at or after `i`, or an error if none exists.
fn find_null(data: &[u8], i: usize) -> Result<usize> {
    data.get(i..)
        .ok_or_else(|| anyhow!("offset {} past end of manifest", i))?
        .iter()
        .position(|&b| b == 0)
        .map(|p| i + p)
        .ok_or_else(|| anyhow!("unterminated name at offset {}", i))
}

/// Parse a root/platform manifest (type 0x08): list of (name, size, checksum8).
pub fn parse_platform(data: &[u8]) -> Result<Vec<PlatformEntry>> {
    let mut i = 3usize;
    i += 1; // type byte
    let cnt = read_varint(data, &mut i)?;
    if i + 7 > data.len() {
        return Err(anyhow!("truncated platform manifest header"));
    }
    i += 7; // fixed bytes after count
    if cnt > data.len() as u64 / 3 {
        return Err(anyhow!("implausible entry count {cnt}"));
    }
    let mut out = Vec::with_capacity(cnt as usize);
    for _ in 0..cnt {
        let j = find_null(data, i)?;
        let name = data[i..j].to_vec();
        i = j + 1;
        let size = read_varint(data, &mut i)?;
        let checksum = data
            .get(i..i + 8)
            .ok_or_else(|| anyhow!("truncated checksum at offset {}", i))?;
        let mut c = [0u8; 8];
        c.copy_from_slice(checksum);
        i += 8;
        out.push(PlatformEntry { name, size, checksum: c });
    }
    Ok(out)
}

/// Parse a category manifest: list of (names, flags, size, h).
pub fn parse_category(data: &[u8]) -> Result<Vec<CategoryEntry>> {
    let mut i = 3usize;
    i += 1; // type byte
    let cnt = read_varint(data, &mut i)?;
    read_varint(data, &mut i)?; // field2 (unused)
    if i + 10 > data.len() {
        return Err(anyhow!("truncated category manifest header"));
    }
    i += 10; // fixed bytes
    if cnt > data.len() as u64 / 3 {
        return Err(anyhow!("implausible entry count {cnt}"));
    }
    let mut out = Vec::with_capacity(cnt as usize);
    for _ in 0..cnt {
        let j = find_null(data, i)?;
        let name1 = data[i..j].to_vec();
        i = j + 1;
        let mut names = vec![name1];
        if data.get(i) == Some(&0x00) {
            i += 3; // single name -> 3 flag bytes
        } else {
            let j = find_null(data, i)?;
            let name2 = data[i..j].to_vec();
            i = j + 1;
            names.push(name2);
            i += 2; // composite -> 2 flag bytes
        }
        let size = read_varint(data, &mut i)?;
        let h = data
            .get(i..i + 16)
            .ok_or_else(|| anyhow!("truncated hash at offset {}", i))?;
        let mut hh = [0u8; 16];
        hh.copy_from_slice(h);
        i += 16;
        out.push(CategoryEntry { names, size, h: hh });
    }
    Ok(out)
}

/// On-disk filename of an asset: base32(SHA1(h[0:8] + size_be8 + name)).
pub fn asset_filename(name: &[u8], size: u64, h: &[u8; 16]) -> String {
    let mut buf = Vec::with_capacity(8 + 8 + name.len());
    buf.extend_from_slice(&h[..8]);
    buf.extend_from_slice(&size.to_be_bytes());
    buf.extend_from_slice(name);
    let digest = Sha1::digest(&buf);
    b32(digest.as_slice())
}

/// On-disk filename of a manifest: base32(SHA1(checksum + size_be8 + name)).
pub fn manifest_filename(name: &[u8], size: u64, checksum: &[u8; 8]) -> String {
    let mut buf = Vec::with_capacity(8 + 8 + name.len());
    buf.extend_from_slice(checksum);
    buf.extend_from_slice(&size.to_be_bytes());
    buf.extend_from_slice(name);
    let digest = Sha1::digest(&buf);
    b32(digest.as_slice())
}

pub fn manifest_url(name: &[u8], size: u64, checksum: &[u8; 8]) -> String {
    let fn_ = manifest_filename(name, size, checksum);
    format!("{CDN}/resources/Manifest/{}/{fn_}", &fn_[..2])
}

pub fn asset_url(category: &[u8], fn_: &str) -> String {
    if GENERIC_CATEGORIES.contains(&category) {
        format!("{CDN}/resources/Generic/{}/{fn_}", &fn_[..2])
    } else {
        format!("{CDN}/resources/Windows/assetbundles/{}/{fn_}", &fn_[..2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_msb_first() {
        let mut i = 0;
        assert_eq!(read_varint(&[0x01], &mut i).unwrap(), 1);

        i = 0;
        assert_eq!(read_varint(&[0x81, 0x00], &mut i).unwrap(), 128);
        assert_eq!(i, 2);

        i = 0;
        assert_eq!(read_varint(&[0x82, 0x2C], &mut i).unwrap(), 300);
        assert_eq!(i, 2);
    }

    #[test]
    fn base32_nopad() {
        assert_eq!(b32(b"foo"), "MZXW6");
        assert_eq!(b32(b"foobar"), "MZXW6YTBOI");
    }

    // Pins the verified formula base32(SHA1(h[0..8] + size_be8 + name)); every
    // URL and on-disk path depends on it.
    #[test]
    fn asset_filename_formula() {
        let h: [u8; 16] = std::array::from_fn(|i| i as u8);
        assert_eq!(
            asset_filename(b"assetbundle/chara/test", 123456, &h),
            "PKN52X4MDSORIKL5DXIE5XLDEHSVO7L4"
        );
    }

    #[test]
    fn manifest_filename_formula() {
        assert_eq!(
            manifest_filename(b"Windows", 987654321, &[1, 2, 3, 4, 5, 6, 7, 8]),
            "RBSAVLZ4UJ6AU45XYT23E5S76A3TFW23"
        );
    }

    // Corrupt manifests must return Err, not panic on an out-of-range slice.
    #[test]
    fn truncated_manifest_is_an_error() {
        assert!(parse_platform(&[0, 0, 0, 1, 0x00]).is_err());
        assert!(parse_category(&[0, 0, 0, 1, 0x00, 0x00]).is_err());
    }
}
