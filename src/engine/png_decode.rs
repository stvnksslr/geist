//! PNG decoding for the kitty graphics protocol (`f=100`).
//!
//! libghostty parses the protocol but delegates PNG decoding to a hook, which
//! is unset by default — so without this, `f=100` transmissions (what `icat`
//! and most tools send) are rejected outright.
//!
//! The binding ships a `RustPngDecoder`, but it is unusable: its only field is
//! private with no constructor, and it reserves capacity without resizing, so
//! the `png` crate is handed a zero-length buffer. Hence our own impl.

use libghostty_vt::alloc::{Allocator, Bytes};
use libghostty_vt::kitty::graphics::{DecodePng, DecodedImage, set_png_decoder};

/// Refuse anything whose decoded size would be absurd. Kitty's own cap is
/// 10000 px per dimension, so 400 MB is already past what libghostty will
/// store — this just stops a hostile header causing a huge allocation before
/// that check runs.
const MAX_DECODED_BYTES: usize = 400 * 1024 * 1024;

struct PngDecoder {
    /// Reused across images so a stream of PNGs doesn't reallocate each time.
    buf: Vec<u8>,
}

impl DecodePng for PngDecoder {
    fn decode_png<'alloc>(
        &mut self,
        alloc: &'alloc Allocator<'_>,
        data: &[u8],
    ) -> Option<DecodedImage<'alloc>> {
        // png 0.18 requires `Read + Seek`; a slice is only `Read`.
        let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
        // Normalize to 8-bit RGBA: expand palettes/grayscale/low bit depths and
        // add an alpha channel, and drop 16-bit samples to 8.
        decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().ok()?;

        let size = reader.output_buffer_size()?;
        if size > MAX_DECODED_BYTES {
            return None;
        }
        // `reserve` alone leaves `len` at 0 and `next_frame` would see an empty
        // slice — the bug in the binding's own decoder.
        self.buf.clear();
        self.buf.resize(size, 0);
        let info = reader.next_frame(&mut self.buf).ok()?;

        // The transformations should guarantee this; check rather than trust,
        // so a surprising input yields a clean rejection instead of garbage
        // pixels (libghostty validates `len == w*h*4` and would reject anyway).
        if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
            return None;
        }
        let len = (info.width as usize)
            .checked_mul(info.height as usize)?
            .checked_mul(4)?;
        if info.buffer_size() != len || self.buf.len() < len {
            return None;
        }

        // The buffer handed back **must** come from libghostty's allocator: it
        // takes ownership and frees it with that allocator.
        let mut out = Bytes::new_with_alloc(alloc, len).ok()?;
        out.copy_from_slice(&self.buf[..len]);
        Some(DecodedImage {
            width: info.width,
            height: info.height,
            data: out,
        })
    }
}

thread_local! {
    /// Whether this thread has installed the decoder.
    ///
    /// Deliberately thread-local rather than a `std::sync::Once`:
    /// `set_png_decoder` stores the callback in a thread-local of its own and
    /// only ever invokes it on the thread that wrote to the terminal. A `Once`
    /// would install it for whichever thread got there first and silently leave
    /// every other thread without PNG support — which in practice means every
    /// `cargo test` after the first, since each test runs on its own thread.
    static INSTALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Install geist's PNG decoder on the current thread, once.
pub fn install() {
    INSTALLED.with(|done| {
        if done.get() {
            return;
        }
        if set_png_decoder(Some(Box::new(PngDecoder { buf: Vec::new() }))).is_ok() {
            done.set(true);
        }
    });
}
