//! What the About dialog says (the macOS app's `About/` view): the version,
//! the exact commit it was built from, and where to go next.

/// `Cargo.toml`'s version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The short commit hash `build.rs` captured, or `unknown` outside a checkout.
pub const COMMIT: &str = env!("GIEST_GIT_COMMIT");

/// `debug` or `release`, which matters when someone reports a performance bug.
pub const PROFILE: &str = if cfg!(debug_assertions) {
    "debug"
} else {
    "release"
};

/// The links the dialog offers, as (label, URL).
pub const LINKS: &[(&str, &str)] = &[
    ("Ghostty", "https://ghostty.org"),
    ("Ghostty documentation", "https://ghostty.org/docs"),
    ("libghostty-rs", "https://github.com/Uzaaft/libghostty-rs"),
];

/// The one-line build description: `0.1.0 (abc1234def, release)`.
pub fn version_line() -> String {
    format!("{VERSION} ({COMMIT}, {PROFILE})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_names_version_commit_and_profile() {
        let v = version_line();
        assert!(v.starts_with(VERSION), "{v}");
        assert!(v.contains(COMMIT) && v.contains(PROFILE), "{v}");
        assert!(!COMMIT.is_empty());
    }

    #[test]
    fn links_are_https() {
        assert!(LINKS.iter().all(|(_, u)| u.starts_with("https://")));
    }
}
