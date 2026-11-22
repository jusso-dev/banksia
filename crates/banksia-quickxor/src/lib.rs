//! Microsoft quickXorHash.
//!
//! OneDrive and SharePoint report this hash for every file. Banksia uses it to
//! verify downloads and to check that a restored file matches the backup.

use base64::Engine;

const WIDTH_IN_BITS: usize = 160;
const SHIFT: usize = 11;
const CELLS: usize = WIDTH_IN_BITS.div_ceil(64);

/// Incremental quickXorHash state.
#[derive(Clone, Debug, Default)]
pub struct QuickXor {
    data: [u64; CELLS],
    shift_so_far: usize,
    length_so_far: u64,
}

impl QuickXor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, input: &[u8]) {
        let mut vector_index = self.shift_so_far / 64;
        let mut vector_offset = self.shift_so_far % 64;
        let iterations = input.len().min(WIDTH_IN_BITS);
        for i in 0..iterations {
            let is_last_cell = vector_index == CELLS - 1;
            let bits_in_cell = if is_last_cell {
                WIDTH_IN_BITS - 64 * (CELLS - 1)
            } else {
                64
            };
            if vector_offset <= bits_in_cell - 8 {
                for byte in input[i..].iter().step_by(WIDTH_IN_BITS) {
                    self.data[vector_index] ^= u64::from(*byte) << vector_offset;
                }
            } else {
                let index2 = if is_last_cell { 0 } else { vector_index + 1 };
                let low = bits_in_cell - vector_offset;
                let xored = input[i..]
                    .iter()
                    .step_by(WIDTH_IN_BITS)
                    .fold(0u8, |acc, b| acc ^ b);
                self.data[vector_index] ^= u64::from(xored) << vector_offset;
                self.data[index2] ^= u64::from(xored) >> low;
            }
            vector_offset += SHIFT;
            if vector_offset >= bits_in_cell {
                vector_index = if is_last_cell { 0 } else { vector_index + 1 };
                vector_offset -= bits_in_cell;
            }
        }
        self.shift_so_far =
            (self.shift_so_far + SHIFT * (input.len() % WIDTH_IN_BITS)) % WIDTH_IN_BITS;
        self.length_so_far += input.len() as u64;
    }

    /// The 20-byte digest.
    pub fn digest(&self) -> [u8; 20] {
        let mut out = [0u8; 20];
        for (i, cell) in self.data.iter().enumerate() {
            let bytes = cell.to_le_bytes();
            let start = i * 8;
            let take = (20 - start).min(8);
            out[start..start + take].copy_from_slice(&bytes[..take]);
        }
        for (i, b) in self.length_so_far.to_le_bytes().iter().enumerate() {
            out[20 - 8 + i] ^= b;
        }
        out
    }

    /// Standard base64, as returned by Microsoft Graph in `file.hashes.quickXorHash`.
    pub fn finish_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.digest())
    }
}

/// Hash a whole buffer.
pub fn hash_base64(data: &[u8]) -> String {
    let mut h = QuickXor::new();
    h.update(data);
    h.finish_base64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn std_b64(url_safe: &str) -> String {
        url_safe.replace('-', "+").replace('_', "/")
    }

    fn seq() -> Vec<u8> {
        (0..100_000u32).map(|i| (i % 251) as u8).collect()
    }

    // Reference values computed with an independent implementation (rclone).
    #[test]
    fn known_answers() {
        assert_eq!(hash_base64(b""), "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        assert_eq!(hash_base64(b"a"), "YQAAAAAAAAAAAAAAAQAAAAAAAAA=");
        assert_eq!(hash_base64(&[b'x'; 1000]), "RS960Yte9KIXvehFx3nR8573vOc=");
        assert_eq!(
            hash_base64(b"The quick brown fox jumps over the lazy dog"),
            "bMSlbysmxJL6S75XwfMcQZOpcr4="
        );
        assert_eq!(hash_base64(&seq()), std_b64("w5L-yXEbQ4GhItZONbG3zV6jdSU="));
    }

    #[test]
    fn incremental_matches_one_shot() {
        let data = seq();
        for split in [1usize, 7, 159, 160, 161, 4096, 99_999] {
            let mut h = QuickXor::new();
            for part in data.chunks(split) {
                h.update(part);
            }
            assert_eq!(h.finish_base64(), hash_base64(&data), "split {split}");
        }
    }
}
