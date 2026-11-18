//! The PNG reader the snapshot-artefact gate needs — deliberately not the `image` crate.
//!
//! `xtask` is the gate layer; the plan's house rule is that a gate should not drag in a dependency
//! graph of its own (`licenses` reads `deny.toml` with a hand-written parser for the same reason,
//! and `cargo-deny` is not installed here — plan §9 disposition 17). All the gate has to answer is
//! "is this a real PNG, and how wide and tall is it", which the eight-byte signature and the first
//! chunk answer exactly. Walking every chunk and checking its CRC32 additionally catches a truncated
//! or hand-edited file, which is what "stale" means in practice.

use std::error::Error;

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, b'\n', 0x1a, b'\n'];

/// A decoded PNG header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PngInfo {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub colour_type: u8,
}

/// Validate `bytes` as a PNG (signature, chunk framing, every chunk CRC, IHDR first, IEND last) and
/// return its dimensions.
pub fn inspect(bytes: &[u8]) -> Result<PngInfo, String> {
    if bytes.len() < SIGNATURE.len() + 12 {
        return Err("too short to be a PNG".to_string());
    }
    if bytes[..SIGNATURE.len()] != SIGNATURE {
        return Err("not a PNG (bad 8-byte signature)".to_string());
    }
    let mut at = SIGNATURE.len();
    let mut info: Option<PngInfo> = None;
    let mut saw_idat = false;
    let mut chunks = 0usize;
    while at < bytes.len() {
        if bytes.len() < at + 12 {
            return Err("truncated chunk header".to_string());
        }
        let len =
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        let kind = &bytes[at + 4..at + 8];
        if bytes.len() < at + 12 + len {
            return Err(format!(
                "{} chunk claims {len} bytes, the file ends early",
                kind_ascii(kind)
            ));
        }
        let data = &bytes[at + 8..at + 8 + len];
        let stored_crc = u32::from_be_bytes([
            bytes[at + 8 + len],
            bytes[at + 9 + len],
            bytes[at + 10 + len],
            bytes[at + 11 + len],
        ]);
        let mut crc_input = Vec::with_capacity(4 + len);
        crc_input.extend_from_slice(kind);
        crc_input.extend_from_slice(data);
        if crc32(&crc_input) != stored_crc {
            return Err(format!(
                "{} chunk CRC is invalid — the file is corrupt or was edited by hand",
                kind_ascii(kind)
            ));
        }
        chunks += 1;
        match kind {
            b"IHDR" => {
                if at != SIGNATURE.len() {
                    return Err("IHDR must be the first chunk".to_string());
                }
                if data.len() != 13 {
                    return Err(format!("IHDR is {} bytes, not 13", data.len()));
                }
                let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
                if width == 0 || height == 0 {
                    return Err("IHDR declares an empty image".to_string());
                }
                info = Some(PngInfo {
                    width,
                    height,
                    bit_depth: data[8],
                    colour_type: data[9],
                });
            }
            b"IDAT" => saw_idat = true,
            b"IEND" => {
                if !saw_idat {
                    return Err("the PNG has no image data (IDAT) chunk".to_string());
                }
                if at + 12 + len != bytes.len() {
                    return Err("bytes follow IEND".to_string());
                }
                return Ok(info.expect("IHDR was checked before IEND can be reached"));
            }
            _ => {}
        }
        at += 12 + len;
    }
    Err(format!(
        "no IEND chunk after {chunks} chunk(s): the PNG is truncated"
    ))
}

fn kind_ascii(kind: &[u8]) -> String {
    String::from_utf8_lossy(kind).to_string()
}

/// The PNG's own CRC-32 (the same polynomial as zlib), table-free: the gate runs on one file.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Read `path` and validate it as a PNG of exactly `want` pixels wide and tall.
pub fn check_dimensions(
    path: &str,
    bytes: &[u8],
    want: (u32, u32),
) -> Result<PngInfo, Box<dyn Error>> {
    let info = inspect(bytes).map_err(|why| format!("{path}: {why}"))?;
    if (info.width, info.height) != want {
        return Err(format!(
            "{path}: {}x{} px, the shell geometry says {}x{} px",
            info.width, info.height, want.0, want.1
        )
        .into());
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x1 RGB PNG, built here so the test does not depend on an asset.
    fn png(width: u32, height: u32) -> Vec<u8> {
        fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            let mut crc_input = kind.to_vec();
            crc_input.extend_from_slice(data);
            out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
            out
        }
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // bit depth 8, colour type 2 (RGB)
        let mut out = SIGNATURE.to_vec();
        out.extend_from_slice(&chunk(b"IHDR", &ihdr));
        out.extend_from_slice(&chunk(
            b"IDAT",
            &[0x78, 0x9c, 0x63, 0x00, 0x00, 0x00, 0x05, 0x00, 0x01],
        ));
        out.extend_from_slice(&chunk(b"IEND", &[]));
        out
    }

    #[test]
    fn accepts_a_well_formed_png_and_reads_its_size() {
        let bytes = png(2, 1);
        assert_eq!(
            inspect(&bytes).expect("valid"),
            PngInfo {
                width: 2,
                height: 1,
                bit_depth: 8,
                colour_type: 2
            }
        );
    }

    #[test]
    fn rejects_a_truncated_file() {
        let mut bytes = png(2, 1);
        let cut = bytes.len() - 6;
        bytes.truncate(cut);
        assert!(inspect(&bytes).is_err());
    }

    #[test]
    fn rejects_a_corrupted_chunk() {
        let mut bytes = png(2, 1);
        let last = bytes.len() - 13;
        bytes[last] ^= 0xff; // flip a byte of the IDAT payload
        assert!(inspect(&bytes).unwrap_err().contains("CRC"));
    }

    #[test]
    fn rejects_a_non_png() {
        assert!(inspect(b"GIF89a................................").is_err());
    }

    #[test]
    fn dimension_check_names_the_expected_size() {
        let bytes = png(2, 1);
        let err = check_dimensions("x.png", &bytes, (1440, 900))
            .unwrap_err()
            .to_string();
        assert!(err.contains("2x1 px"), "{err}");
        assert!(err.contains("1440x900"), "{err}");
    }
}
