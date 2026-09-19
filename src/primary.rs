//! An emulated PRIMARY selection.
//!
//! Windows has no X11-style selection clipboard, but three Ghostty features are
//! defined in terms of one: `copy-on-select = primary|both` writes to it, and
//! `middle-click-action = primary-paste` and the `paste_from_selection` action
//! read it. giest keeps it in-process — shared by every window, like the real
//! PRIMARY is shared by every X client — so those features behave as they do on
//! Linux between giest panes, without touching the system clipboard.
//!
//! Reads return `None` while it is empty, and callers that want Linux-like
//! "middle-click does something useful out of the box" fall back to the system
//! clipboard (see `App`'s middle-click handling). Every paste that reads it still
//! goes through `Session::paste_str`.

use std::sync::Mutex;

static PRIMARY: Mutex<String> = Mutex::new(String::new());

/// Replace the PRIMARY contents.
pub fn set(text: &str) {
    if let Ok(mut p) = PRIMARY.lock() {
        p.clear();
        p.push_str(text);
    }
}

/// The PRIMARY contents, or `None` if nothing has been selected yet.
pub fn get() -> Option<String> {
    PRIMARY.lock().ok().filter(|p| !p.is_empty()).map(|p| p.clone())
}

#[cfg(test)]
mod tests {
    #[test]
    fn set_then_get_round_trips() {
        super::set("hello");
        assert_eq!(super::get().as_deref(), Some("hello"));
        super::set("");
        assert_eq!(super::get(), None);
    }
}
