//! 64 KiB block iteration and prefix fingerprints (§3.4).
//!
//! The corpus is never read in one piece: readers pull aligned blocks and keep
//! UTF-8 boundaries when they need text. Prefix fingerprints are a pure
//! dedupe/compare primitive; they never enter `hit_id` or `range_sha256`.

use crate::recall::hash::{sha256_hex, BLOCK_BYTES};
use crate::recall::port::model::PortError;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// `sha256` of the first `blocks * 64 KiB` bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrefixDigest {
    pub digest: String,
    pub blocks: usize,
}

#[derive(Debug, Clone)]
pub struct PrefixFingerprint {
    hasher: Sha256,
    blocks: usize,
}

impl Default for PrefixFingerprint {
    fn default() -> Self {
        Self::new()
    }
}

impl PrefixFingerprint {
    pub fn new() -> Self {
        PrefixFingerprint {
            hasher: Sha256::new(),
            blocks: 0,
        }
    }

    /// Push one aligned block (at most `BLOCK_BYTES` bytes).
    pub fn push_block(&mut self, block: &[u8]) {
        debug_assert!(block.len() <= BLOCK_BYTES);
        self.hasher.update(block);
        self.blocks += 1;
    }

    pub fn snapshot(&self) -> PrefixDigest {
        PrefixDigest {
            digest: to_hex(&self.hasher.clone().finalize()),
            blocks: self.blocks,
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Read aligned 64 KiB blocks, invoking `on_block` for each. Returns the total
/// number of bytes read. Short reads are retried; a zero read ends the stream.
pub fn read_aligned_blocks<R: Read>(
    reader: &mut R,
    mut on_block: impl FnMut(&[u8]) -> Result<(), PortError>,
) -> Result<u64, PortError> {
    let mut block = vec![0u8; BLOCK_BYTES];
    let mut filled = 0usize;
    let mut total = 0u64;
    loop {
        if filled == BLOCK_BYTES {
            on_block(&block[..filled])?;
            total += filled as u64;
            filled = 0;
        }
        let read = reader
            .read(&mut block[filled..])
            .map_err(|err| PortError::Io(err.to_string()))?;
        if read == 0 {
            if filled > 0 {
                on_block(&block[..filled])?;
                total += filled as u64;
            }
            return Ok(total);
        }
        filled += read;
    }
}

/// Number of bytes at the start of `block` that end on a UTF-8 boundary.
pub fn split_at_utf8_boundary(block: &[u8]) -> usize {
    if block.is_empty() {
        return 0;
    }
    let mut cut = block.len();
    let mut back = 0;
    while back < 4 && cut > 0 {
        if std::str::from_utf8(&block[..cut]).is_ok() {
            return cut;
        }
        cut -= 1;
        back += 1;
    }
    0
}

/// Compare two byte buffers: block fingerprints are computed first, and when a
/// block digest matches but its bytes do not (misaligned boundaries) the
/// comparison falls back to byte equality.
pub fn prefixes_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut offset = 0usize;
    while offset < a.len() {
        let end = (offset + BLOCK_BYTES).min(a.len());
        let block_a = &a[offset..end];
        let block_b = &b[offset..end];
        if block_a != block_b {
            let digest_a = sha256_hex(block_a);
            let digest_b = sha256_hex(block_b);
            if digest_a == digest_b {
                // Collision or misalignment: decide on bytes, never on digest.
                return a == b;
            }
            return false;
        }
        offset = end;
    }
    true
}

/// Read a bounded object in blocks, aborting before reading anything larger
/// than `limit` (so a single object can never exhaust memory or the budget).
pub fn read_bounded_file(
    path: &Path,
    limit: usize,
) -> Result<(Vec<u8>, PrefixDigest), PortError> {
    let metadata = std::fs::metadata(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            PortError::NotFound(path.display().to_string())
        } else {
            PortError::Io(err.to_string())
        }
    })?;
    if metadata.len() > limit as u64 {
        return Err(PortError::Corrupt(format!(
            "object at {} is {} bytes, above the {limit}-byte budget",
            path.display(),
            metadata.len()
        )));
    }
    let mut file = File::open(path).map_err(|err| PortError::Io(err.to_string()))?;
    let mut fingerprint = PrefixFingerprint::new();
    let mut collected = Vec::with_capacity(metadata.len() as usize);
    read_aligned_blocks(&mut file, |block| {
        fingerprint.push_block(block);
        collected.extend_from_slice(block);
        Ok(())
    })?;
    Ok((collected, fingerprint.snapshot()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_digest_matches_sha256_of_prefix() {
        let mut fingerprint = PrefixFingerprint::new();
        fingerprint.push_block(b"abc");
        assert_eq!(fingerprint.snapshot().digest, sha256_hex(b"abc"));
        fingerprint.push_block(b"def");
        assert_eq!(fingerprint.snapshot().digest, sha256_hex(b"abcdef"));
        assert_eq!(fingerprint.snapshot().blocks, 2);
    }

    #[test]
    fn blocks_are_aligned_and_bounded() {
        let data = vec![7u8; BLOCK_BYTES + 5];
        let mut seen = 0usize;
        let mut sizes = Vec::new();
        let total = read_aligned_blocks(&mut data.as_slice(), |block| {
            seen += block.len();
            sizes.push(block.len());
            Ok(())
        })
        .unwrap();
        assert_eq!(total as usize, data.len());
        assert_eq!(seen, data.len());
        assert_eq!(sizes, vec![BLOCK_BYTES, 5]);
    }

    #[test]
    fn utf8_boundary_split_backs_off_multibyte_tail() {
        let block = "abc中".as_bytes();
        assert_eq!(split_at_utf8_boundary(block), block.len());
        // A block cut inside the 3-byte character backs off to "abc".
        assert_eq!(split_at_utf8_boundary(&block[..5]), 3);
        assert_eq!(split_at_utf8_boundary(&block[..4]), 3);
    }

    #[test]
    fn byte_comparison_detects_same_length_different_content() {
        assert!(prefixes_equal(b"same", b"same"));
        assert!(!prefixes_equal(b"same", b"diff"));
        assert!(!prefixes_equal(b"short", b"longer"));
    }
}
