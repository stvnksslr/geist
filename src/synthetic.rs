//! Deterministic synthetic VT-stream generators for benchmarks and the ConPTY
//! throughput harness. Mirrors Ghostty's `src/synthetic` approach: keep data
//! generation separate from measurement, and make it reproducible (seeded) so
//! runs are comparable across branches. No randomness from the clock — a small
//! LCG keeps the streams identical every run for a given seed.

/// A tiny deterministic PRNG (PCG-style LCG). Seeded, so generated streams are
/// byte-for-byte reproducible — never seed this from the clock.
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        // Avoid a zero internal state.
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next_u32(&mut self) -> u32 {
        // LCG constants from Knuth's MMIX.
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // Return the high bits (better distributed than the low bits).
        (self.0 >> 32) as u32
    }

    /// Uniform-ish value in `0..n`.
    fn below(&mut self, n: u32) -> u32 {
        self.next_u32() % n.max(1)
    }
}

/// A printable-ASCII stream of roughly `len` bytes: words of 1–12 printable
/// characters separated by spaces, with a CRLF roughly every ~80 columns. This
/// is the baseline "fast path" workload (no shaping, single-cell glyphs).
pub fn ascii(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Lcg::new(seed);
    let mut out = Vec::with_capacity(len + 16);
    let mut col = 0u32;
    while out.len() < len {
        let word = 1 + rng.below(12);
        for _ in 0..word {
            // Printable ASCII range 0x21..=0x7E (94 glyphs).
            out.push(0x21 + rng.below(94) as u8);
            col += 1;
        }
        out.push(b' ');
        col += 1;
        if col >= 80 {
            out.extend_from_slice(b"\r\n");
            col = 0;
        }
    }
    out
}

/// A UTF-8 stream mixing ASCII with multibyte codepoints (Latin-1 accents, CJK,
/// and the occasional emoji) so width/grapheme/shaping/fallback paths are
/// exercised. Roughly `len` bytes, CRLF every ~40 columns.
pub fn utf8(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Lcg::new(seed);
    let mut out = Vec::with_capacity(len + 16);
    let mut col = 0u32;
    // A small repertoire spanning 1–4 byte encodings.
    let samples: &[char] = &[
        'a', 'Z', '7', 'é', 'ü', 'ñ', 'ß', 'Ω', 'д', '中', '文', '日', '本', '語', '한', '글',
        '€', '→', '★', '😀', '🚀',
    ];
    while out.len() < len {
        let word = 1 + rng.below(8);
        for _ in 0..word {
            let c = samples[rng.below(samples.len() as u32) as usize];
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            col += 1;
        }
        out.push(b' ');
        col += 1;
        if col >= 40 {
            out.extend_from_slice(b"\r\n");
            col = 0;
        }
    }
    out
}

/// A stream of `count` OSC sequences (alternating window-title set and a color
/// query), interleaved with a little printable text. Exercises the OSC side
/// paths (title parsing, the OSC 52 scanner) at the host boundary.
pub fn osc(count: usize, seed: u64) -> Vec<u8> {
    let mut rng = Lcg::new(seed);
    let mut out = Vec::new();
    for i in 0..count {
        if i % 2 == 0 {
            // OSC 0 ; <title> BEL
            out.extend_from_slice(b"\x1b]0;");
            let n = 4 + rng.below(20);
            for _ in 0..n {
                out.push(0x41 + rng.below(26) as u8);
            }
            out.push(0x07);
        } else {
            // OSC 52 ; c ; <base64> ST  (clipboard set — drives the scanner)
            out.extend_from_slice(b"\x1b]52;c;aGVsbG8=\x1b\\");
        }
        out.extend_from_slice(b"x\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_are_deterministic_and_sized() {
        assert_eq!(ascii(1000, 1), ascii(1000, 1), "ascii must be reproducible");
        assert_ne!(ascii(1000, 1), ascii(1000, 2), "different seed → different");
        assert!(ascii(1000, 1).len() >= 1000);
        assert!(utf8(1000, 7).len() >= 1000);
        // utf8 stream must be valid UTF-8 (multibyte chars emitted whole).
        assert!(std::str::from_utf8(&utf8(2000, 3)).is_ok());
        // osc produces a non-trivial stream proportional to the count.
        assert!(osc(50, 9).len() > 50);
    }
}
