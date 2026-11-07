#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(clippy::all)]
#![allow(rustdoc::all)]

mod bindings;

use std::ops::Deref;

pub use bindings::*;

// Function-like and cast-expression macros bindgen cannot evaluate, declared
// by hand. Values are copied from the libghostty-vt 1.0 headers (`style.h`,
// `types.h`), whose layouts are frozen; the tests below pin them.

/// `GHOSTTY_STYLE_FLAG_*`: on/off attributes in [`Style::flags`] (bits 0-7).
pub const STYLE_FLAG_BOLD: StyleFlags = 1 << 0;
#[allow(missing_docs)]
pub const STYLE_FLAG_ITALIC: StyleFlags = 1 << 1;
#[allow(missing_docs)]
pub const STYLE_FLAG_FAINT: StyleFlags = 1 << 2;
#[allow(missing_docs)]
pub const STYLE_FLAG_BLINK: StyleFlags = 1 << 3;
#[allow(missing_docs)]
pub const STYLE_FLAG_INVERSE: StyleFlags = 1 << 4;
#[allow(missing_docs)]
pub const STYLE_FLAG_INVISIBLE: StyleFlags = 1 << 5;
#[allow(missing_docs)]
pub const STYLE_FLAG_STRIKETHROUGH: StyleFlags = 1 << 6;
#[allow(missing_docs)]
pub const STYLE_FLAG_OVERLINE: StyleFlags = 1 << 7;
/// `GHOSTTY_STYLE_UNDERLINE_MASK`: the bits of [`Style::flags`] holding the
/// underline style, a `GhosttySgrUnderline` shifted by
/// [`STYLE_UNDERLINE_SHIFT`].
pub const STYLE_UNDERLINE_MASK: StyleFlags = 0x7 << STYLE_UNDERLINE_SHIFT;

/// `GHOSTTY_IMAGE_LAYER_MASK_*`: one bit per [`ImageLayer`] value, for the
/// placement iterator's layer filter. Zero means no filter.
pub const IMAGE_LAYER_MASK_BELOW_BACKGROUND: u32 = 1 << ImageLayer::BELOW_BACKGROUND;
#[allow(missing_docs)]
pub const IMAGE_LAYER_MASK_BELOW_TEXT: u32 = 1 << ImageLayer::BELOW_TEXT;
#[allow(missing_docs)]
pub const IMAGE_LAYER_MASK_ABOVE_TEXT: u32 = 1 << ImageLayer::ABOVE_TEXT;

/// Initialize a "sized" FFI object.
#[macro_export]
macro_rules! sized {
    ($ty:ty) => {{
        let mut t = <$ty as ::std::default::Default>::default();
        t.size = ::std::mem::size_of::<$ty>();
        t
    }};
}

impl<S> From<S> for bindings::String
where
    S: Deref<Target = str>,
{
    fn from(value: S) -> Self {
        Self {
            ptr: value.as_ptr(),
            len: value.len(),
        }
    }
}

impl bindings::String {
    /// Borrow the string as raw bytes. A null pointer is treated as an empty
    /// string; the C API does not promise a non-null pointer for zero-length
    /// strings, and `slice::from_raw_parts` requires one even at length zero.
    ///
    /// # Safety
    ///
    /// The caller must uphold that the associated lifetime is valid
    /// with the given context behind the FFI string.
    pub unsafe fn to_bytes<'a>(self) -> &'a [u8] {
        if self.ptr.is_null() {
            return &[];
        }
        // SAFETY: To be upheld by caller
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// # Safety
    ///
    /// The caller must uphold that the associated lifetime is valid
    /// with the given context behind the FFI string, and that it contains
    /// valid UTF-8 data.
    pub unsafe fn to_str<'a>(self) -> &'a str {
        // SAFETY: To be upheld by caller
        unsafe { std::str::from_utf8_unchecked(self.to_bytes()) }
    }
}

#[cfg(test)]
mod tests {
    use super::bindings;

    #[test]
    fn hand_declared_constants_match_the_headers() {
        use crate::*;
        assert_eq!(STYLE_UNDERLINE_SHIFT, 8);
        assert_eq!(STYLE_UNDERLINE_MASK, 0x0700);
        let flags = [
            STYLE_FLAG_BOLD,
            STYLE_FLAG_ITALIC,
            STYLE_FLAG_FAINT,
            STYLE_FLAG_BLINK,
            STYLE_FLAG_INVERSE,
            STYLE_FLAG_INVISIBLE,
            STYLE_FLAG_STRIKETHROUGH,
            STYLE_FLAG_OVERLINE,
        ];
        for (bit, flag) in flags.into_iter().enumerate() {
            assert_eq!(flag, 1 << bit);
        }
        assert_eq!(IMAGE_LAYER_MASK_BELOW_BACKGROUND, 0b001);
        assert_eq!(IMAGE_LAYER_MASK_BELOW_TEXT, 0b010);
        assert_eq!(IMAGE_LAYER_MASK_ABOVE_TEXT, 0b100);
    }

    #[test]
    fn sized_macro_sets_the_size_field() {
        let colors = crate::sized!(bindings::RenderStateColors);
        assert_eq!(
            colors.size,
            std::mem::size_of::<bindings::RenderStateColors>()
        );
    }

    #[test]
    fn ffi_string_round_trips_static_str() {
        let raw = bindings::String::from("ghostty");
        assert_eq!(raw.len, "ghostty".len());
        // SAFETY: The source string is `'static` and valid UTF-8.
        assert_eq!(unsafe { raw.to_str() }, "ghostty");
    }

    #[test]
    fn ffi_string_round_trips_borrowed_string() {
        let owned = String::from("terminal");
        let raw = bindings::String::from(owned.as_str());
        // SAFETY: `owned` outlives the conversion and contains valid UTF-8.
        assert_eq!(unsafe { raw.to_str() }, "terminal");
    }
}
