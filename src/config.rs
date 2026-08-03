//! Terminal configuration: fonts and the color theme. Built-in defaults that an
//! optional config file overrides.
//!
//! The file uses **Ghostty's config format** — `key = value` lines, kebab-case
//! keys, unquoted colors (`background = #1d1f21`), `#`-prefixed comment lines,
//! and a repeatable `palette` key — so values are transposable with a real
//! Ghostty config. It lives at `%APPDATA%\giest\config` (override the whole path
//! with `$GIEST_CONFIG`). Every key is optional; unset ones keep the built-in
//! default and an empty value (`key =`) resets that key to its default. Keys we
//! don't support are ignored with a warning, so a full Ghostty config can be
//! dropped in and the supported subset applies.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::engine::{BoldColor, CursorShape, Rgb};

/// What a right-click inside a terminal pane does. Ghostty `right-click-action`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightClickAction {
    /// Show a context menu (Copy/Paste/Split/Reset/Select All). The default.
    ContextMenu,
    /// Copy the current selection.
    Copy,
    /// Paste the clipboard.
    Paste,
    /// Copy if there is a selection, otherwise paste.
    CopyOrPaste,
    /// Do nothing.
    Ignore,
}

/// What a middle-click inside a terminal pane does. Ghostty `middle-click-action`.
/// Windows has no PRIMARY selection, so `primary-paste` reads the system clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MiddleClickAction {
    /// Paste the (system) clipboard. The default.
    PrimaryPaste,
    /// Do nothing.
    Ignore,
}

/// Window backdrop blur behind a translucent background. Ghostty
/// `background-blur`, whose grammar is `false` | `true` (the default intensity,
/// 20) | a nonnegative integer intensity | `macos-glass-regular` /
/// `macos-glass-clear` (macOS 26 glass, which off macOS just implies `true` —
/// Ghostty's own Linux path does the same).
///
/// giest maps this onto the Windows DWM backdrops, which have **no radius knob**,
/// so the intensity is only a two-bucket selector (mica below 10, acrylic at or
/// above it) rather than the true Gaussian sigma it is on macOS/KWin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundBlur {
    Off,
    /// Ghostty's `true` — the default intensity of 20.
    On,
    /// An explicit intensity. `Radius(0)` is *disabled*, matching Ghostty.
    Radius(u8),
}

impl BackgroundBlur {
    /// Ghostty's `cval`: the numeric intensity (`true` is 20, `false` is 0).
    pub fn intensity(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::On => 20,
            Self::Radius(r) => r,
        }
    }

    pub fn enabled(self) -> bool {
        self.intensity() > 0
    }
}

/// Which effects fire when a program rings the bell (BEL, `0x07`). Ghostty
/// `bell-features`, a packed-struct bitfield.
///
/// Note the defaults: `attention` and `title` are **on**, `border` is **off**.
/// giest historically flashed the pane border by default; matching Ghostty turns
/// that off, and `bell-features = border` restores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BellFeatures {
    /// Play the OS alert sound (`MessageBeep`).
    pub system: bool,
    /// Play the sound file at [`Config::bell_audio_path`].
    pub audio: bool,
    /// Request the user's attention while the window is unfocused — a taskbar
    /// flash on Windows.
    pub attention: bool,
    /// Prefix the window title with 🔔 until the window is refocused.
    pub title: bool,
    /// Flash a border around the pane that rang.
    pub border: bool,
}

impl Default for BellFeatures {
    fn default() -> Self {
        Self { system: false, audio: false, attention: true, title: true, border: false }
    }
}

impl BellFeatures {
    /// Every feature set to `on` — Ghostty's bare-boolean shorthand.
    fn all(on: bool) -> Self {
        Self { system: on, audio: on, attention: on, title: on, border: on }
    }
}

/// Parse a `bell-features` value the way Ghostty's `parsePackedStruct`
/// (`cli/args.zig`) does. Returns `None` for an invalid value, which the setter
/// turns into "keep the current value".
///
/// Three rules that are easy to get wrong, and all deliberate:
///  - a bare boolean sets **every** field. Ghostty's packed-struct parser uses a
///    *stricter* boolean set than its ordinary one — only `1/t/true/0/f/false`,
///    so `on`/`yes` are feature-name errors here, not booleans. Hence this does
///    not call [`parse_bool`].
///  - otherwise the result starts from the struct **defaults**, not from the
///    current config. So `bell-features = audio` still leaves `attention` and
///    `title` on, and a second `bell-features` line *replaces* the first rather
///    than accumulating onto it.
///  - one unrecognized token rejects the **whole** value (Ghostty returns
///    `InvalidValue` for the entry); we don't keep the features parsed so far.
fn parse_bell_features(value: &str) -> Option<BellFeatures> {
    let v = value.trim();
    match v {
        "1" | "t" | "true" => return Some(BellFeatures::all(true)),
        "0" | "f" | "false" => return Some(BellFeatures::all(false)),
        _ => {}
    }
    let mut out = BellFeatures::default();
    for tok in v.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let (name, on) = match tok.strip_prefix("no-") {
            Some(rest) => (rest, false),
            None => (tok, true),
        };
        match name.to_ascii_lowercase().as_str() {
            "system" => out.system = on,
            "audio" => out.audio = on,
            "attention" => out.attention = on,
            "title" => out.title = on,
            "border" => out.border = on,
            _ => return None,
        }
    }
    Some(out)
}

/// Wheel-distance multipliers. Ghostty `mouse-scroll-multiplier`.
///
/// Two numbers because the devices are different animals: a wheel emits chunky
/// notches, a trackpad emits a stream of small deltas, and one multiplier that
/// suits either ruins the other. Ghostty defaults to `3` for discrete and `1`
/// for precision.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MouseScrollMultiplier {
    /// Trackpads and other smooth-scroll devices.
    pub precision: f32,
    /// Notched mouse wheels.
    pub discrete: f32,
}

impl Default for MouseScrollMultiplier {
    fn default() -> Self {
        Self {
            precision: 1.0,
            discrete: 3.0,
        }
    }
}

/// Parse `mouse-scroll-multiplier`: a bare number sets **both**, or a
/// comma-separated list of `precision:`/`discrete:` prefixes sets them
/// individually (`precision:0.1,discrete:3`).
///
/// Values are clamped to Ghostty's `0.01..=10_000`, which it calls extreme at
/// both ends — the clamp only stops a typo turning the wheel into a no-op or a
/// teleport.
fn parse_scroll_multiplier(
    v: &str,
    current: MouseScrollMultiplier,
) -> Option<MouseScrollMultiplier> {
    let clamp = |n: f32| n.clamp(0.01, 10_000.0);
    if let Ok(n) = v.trim().parse::<f32>() {
        return Some(MouseScrollMultiplier {
            precision: clamp(n),
            discrete: clamp(n),
        });
    }
    let mut out = current;
    for tok in v.split(',') {
        let (name, value) = tok.trim().split_once(':')?;
        let n = clamp(value.trim().parse::<f32>().ok()?);
        match name.trim().to_ascii_lowercase().as_str() {
            "precision" => out.precision = n,
            "discrete" => out.discrete = n,
            _ => return None,
        }
    }
    Some(out)
}

/// When the viewport jumps back to the live edge. Ghostty `scroll-to-bottom`,
/// whose default is `keystroke, no-output`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollToBottom {
    /// On a key press that sends data to the shell.
    pub keystroke: bool,
    /// On new output arriving — off by default, because it fights you while
    /// you're reading scrollback of a still-running command.
    pub output: bool,
}

impl Default for ScrollToBottom {
    fn default() -> Self {
        Self {
            keystroke: true,
            output: false,
        }
    }
}

/// Parse `scroll-to-bottom`'s flag list, same grammar as `bell-features`.
fn parse_scroll_to_bottom(v: &str) -> Option<ScrollToBottom> {
    let mut out = ScrollToBottom::default();
    for tok in v.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let (name, on) = match tok.strip_prefix("no-") {
            Some(rest) => (rest, false),
            None => (tok, true),
        };
        match name.to_ascii_lowercase().as_str() {
            "keystroke" => out.keystroke = on,
            "output" => out.output = on,
            _ => return None,
        }
    }
    Some(out)
}

/// When a custom shader's animation loop runs. Ghostty
/// `custom-shader-animation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CustomShaderAnimation {
    /// Never animate — redraw only when the terminal itself changes.
    False,
    /// Animate while the window is focused. The default.
    True,
    /// Animate even unfocused, which costs CPU on every surface.
    Always,
}

impl CustomShaderAnimation {
    /// Whether to keep requesting repaints, given the window's focus.
    pub fn animates(self, focused: bool) -> bool {
        match self {
            Self::False => false,
            Self::True => focused,
            Self::Always => true,
        }
    }
}

/// When a finished command raises a notification. Ghostty
/// `notify-on-command-finish`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyOnCommandFinish {
    /// The default — opt in explicitly.
    Never,
    /// Only when the window isn't focused, i.e. only when you've looked away.
    Unfocused,
    Always,
}

/// How a finished command tells you. Ghostty `notify-on-command-finish-action`,
/// a `bell-features`-style flag list (`no-bell,notify`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotifyOnCommandFinishAction {
    pub bell: bool,
    pub notify: bool,
}

impl Default for NotifyOnCommandFinishAction {
    /// Ghostty's defaults: ring, don't notify.
    fn default() -> Self {
        Self {
            bell: true,
            notify: false,
        }
    }
}

impl NotifyOnCommandFinishAction {
    fn all(on: bool) -> Self {
        Self {
            bell: on,
            notify: on,
        }
    }
}

/// Parse a `notify-on-command-finish-action` value. Same grammar as
/// [`parse_bell_features`] — a comma-separated list starting from the defaults,
/// each name negatable with `no-`, or a bare bool for all of them. One unknown
/// name rejects the whole value (`None`), so a typo keeps the current setting
/// rather than silently applying half of what was written.
fn parse_notify_action(value: &str) -> Option<NotifyOnCommandFinishAction> {
    let v = value.trim();
    match v {
        "1" | "t" | "true" => return Some(NotifyOnCommandFinishAction::all(true)),
        "0" | "f" | "false" => return Some(NotifyOnCommandFinishAction::all(false)),
        _ => {}
    }
    let mut out = NotifyOnCommandFinishAction::default();
    for tok in v.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let (name, on) = match tok.strip_prefix("no-") {
            Some(rest) => (rest, false),
            None => (tok, true),
        };
        match name.to_ascii_lowercase().as_str() {
            "bell" => out.bell = on,
            "notify" => out.notify = on,
            _ => return None,
        }
    }
    Some(out)
}

/// Whether a finished command should raise its notification, given the mode and
/// whether the window is focused.
pub fn should_notify_on_finish(mode: NotifyOnCommandFinish, focused: bool) -> bool {
    match mode {
        NotifyOnCommandFinish::Never => false,
        NotifyOnCommandFinish::Unfocused => !focused,
        NotifyOnCommandFinish::Always => true,
    }
}

/// When to confirm before closing a surface. Ghostty `confirm-close-surface`,
/// whose values are `false` / `true` / `always`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmClose {
    /// Never confirm (`false`).
    Never,
    /// Confirm only when something looks like it's running (`true`, the default).
    WhenBusy,
    /// Always confirm, even at an idle prompt (`always`).
    Always,
}

/// Whether closing needs confirmation.
///
/// `busy` is `None` when giest can't tell — a shell that emits no OSC 133 prompt
/// marks — and that resolves to **confirm**, the conservative side: better an
/// extra prompt than a silently discarded running command.
pub fn needs_confirm(mode: ConfirmClose, busy: Option<bool>) -> bool {
    match mode {
        ConfirmClose::Never => false,
        ConfirmClose::Always => true,
        ConfirmClose::WhenBusy => busy.unwrap_or(true),
    }
}

/// Permission for a clipboard operation the *terminal program* asks for.
/// Ghostty `clipboard-read` / `clipboard-write`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardAccess {
    /// Perform it silently.
    Allow,
    /// Refuse it silently.
    Deny,
    /// Ask the user first.
    Ask,
}

/// The paste-safety knobs, grouped so [`Session`](crate::session::Session) can
/// hold one `Copy` value instead of five loose fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardPolicy {
    pub read: ClipboardAccess,
    pub write: ClipboardAccess,
    pub trim_trailing_spaces: bool,
    pub paste_protection: bool,
    pub paste_bracketed_safe: bool,
}

/// Whether `data` is unsafe to paste and so needs confirmation first.
///
/// A verbatim port of Ghostty's rule (`Surface.zig`, `completeClipboardPaste`),
/// including the order of the checks — it isn't arbitrary:
///
/// - Protection off ⇒ nothing is ever unsafe.
/// - **`ESC [ 201 ~` is never trusted while bracketed.** It closes a bracketed
///   paste early, so text after it lands as if typed. This is checked *before*
///   `paste_bracketed_safe`, which is the whole point: framing can't be relied
///   on when the payload can break out of the frame.
/// - Otherwise a bracketed paste is safe if the user trusts bracketing (the
///   default) — the running program has said it will treat the text as data.
/// - Otherwise unsafe if it contains a newline (a shell runs it immediately) or
///   that same end marker. Ghostty flags the marker even when bracketing is off:
///   its presence at all says something about who produced the data.
///
/// The `allow_unsafe` escape hatch Ghostty threads through here lives at the
/// call site instead — a confirmed paste simply skips this function.
pub fn paste_is_unsafe(policy: ClipboardPolicy, bracketed: bool, data: &str) -> bool {
    const PASTE_END: &str = "\x1b[201~";
    if !policy.paste_protection {
        return false;
    }
    if bracketed {
        if data.contains(PASTE_END) {
            return true;
        }
        if policy.paste_bracketed_safe {
            return false;
        }
    }
    data.contains('\n') || data.contains(PASTE_END)
}

/// When to show the grid-size overlay on resize. Ghostty `resize-overlay`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeOverlay {
    Always,
    Never,
    /// Show on every resize *except* a surface's first sizing. The default.
    AfterFirst,
}

/// Whether to show the scrollbar. Ghostty `scrollbar` — which has exactly these
/// two values, and no width/opacity/always knob. Don't add one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scrollbar {
    /// The platform's scrollbar. Ghostty's macOS apprt forces
    /// `scrollerStyle = .overlay` even when the OS prefers the legacy
    /// space-reserving style, so "system" means an auto-hiding overlay
    /// everywhere — which is also the Windows convention.
    System,
    Never,
}

/// How `background-image` is scaled to the terminal area. Ghostty
/// `background-image-fit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundImageFit {
    /// Uniform scale to the largest size fully inside the area. The default.
    Contain,
    /// Uniform scale to the smallest size that covers the area; the overflow is
    /// cropped.
    Cover,
    /// Fill the area exactly, ignoring the aspect ratio.
    Stretch,
    /// Leave the image at its own pixel size.
    None,
}

/// Where `background-image` sits when its fit leaves space. Ghostty
/// `background-image-position` — nine anchors, three per axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundImagePosition {
    TopLeft,
    TopCenter,
    TopRight,
    CenterLeft,
    /// Also spelled `center-center`, and the default.
    Center,
    CenterRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

/// Where the resize overlay sits within the pane. Ghostty
/// `resize-overlay-position`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeOverlayPosition {
    Center,
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

/// Whether a grid-size change should raise the overlay, given the configured
/// mode and whether this is the surface's *first* sizing.
///
/// `after-first` has to be honored explicitly here: a session is created at a
/// default size and immediately re-fit to the real window, so the resize edge
/// always fires once on the first frame and every new tab or split would flash a
/// spurious size. (Ghostty's GTK apprt happens to treat `after-first` the same as
/// `always`, because its scheduler only ever runs from a resize signal — giest
/// matches the *documented* behavior instead.)
pub fn show_resize_overlay(mode: ResizeOverlay, first: bool) -> bool {
    match mode {
        ResizeOverlay::Never => false,
        ResizeOverlay::Always => true,
        ResizeOverlay::AfterFirst => !first,
    }
}

/// Precision of an OSC color *report* (the reply to `OSC 10/11/12 ; ?`).
/// Ghostty `osc-color-report-format`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OscColorReportFormat {
    /// Don't answer color queries at all.
    None,
    /// `rgb:rr/gg/bb` — the raw 8-bit channels.
    Bits8,
    /// `rgb:rrrr/gggg/bbbb`, each channel scaled by 257. The default.
    Bits16,
}

/// User-facing configuration applied at startup.
#[derive(Clone, Debug)]
pub struct Config {
    /// Logical font size in points (scaled by the display DPI for the atlas).
    /// Ghostty `font-size`.
    pub font_points: f32,
    /// Primary font family (name or file path); `None` keeps the built-in
    /// JetBrains Mono. Ghostty `font-family`. *(Applied at startup; changing it
    /// needs a restart — config reload re-applies colors/size, not the font.)*
    pub font_family: Option<String>,
    /// Per-style family overrides; each `None` falls back to `font_family`.
    /// Ghostty `font-family-bold` / `-italic` / `-bold-italic`.
    pub font_family_bold: Option<String>,
    pub font_family_italic: Option<String>,
    pub font_family_bold_italic: Option<String>,
    /// OpenType feature specs applied while shaping, e.g. `-calt` (disable
    /// ligatures/contextual alternates), `ss01`, `cv01=2`. Repeatable. Ghostty
    /// `font-feature`.
    pub font_features: Vec<String>,
    /// Default foreground (text) color. Ghostty `foreground`.
    pub fg: Rgb,
    /// Default background color. Ghostty `background`.
    pub bg: Rgb,
    /// The 256-color palette (indices 0–15 = ANSI, 16–231 = color cube,
    /// 232–255 = grayscale ramp). Ghostty `palette`.
    pub palette: [Rgb; 256],
    /// Logical-point padding on the left/right of the grid. Ghostty
    /// `window-padding-x`.
    pub padding_x: f32,
    /// Logical-point padding above/below the grid. Ghostty `window-padding-y`.
    pub padding_y: f32,
    /// Coverage gamma applied to text antialiasing in the glyph shader; values
    /// above 1 thicken light-on-dark text that linear blending renders too thin.
    /// giest-specific (`text-gamma`); Ghostty has no equivalent.
    pub text_gamma: f32,
    /// Cursor color; `None` defers to the running program / engine default.
    /// Ghostty `cursor-color`.
    pub cursor: Option<Rgb>,
    /// Default cursor shape used until the running program overrides it with
    /// DECSCUSR. Ghostty `cursor-style` (block / bar / underline; the renderer
    /// also draws a hollow block for unfocused panes regardless of this).
    pub cursor_style: CursorShape,
    /// Whether the default cursor blinks. `None` means unset, which follows
    /// Ghostty's default of *blinking* (its stream handler uses
    /// `default_cursor_blink orelse true`); `Some(b)` forces it. Applies only
    /// while the program hasn't selected its own cursor style via DECSCUSR.
    /// Ghostty `cursor-style-blink`.
    pub cursor_style_blink: Option<bool>,
    /// Foreground policy for bold text. Ghostty `bold-color` (and the deprecated
    /// `bold-is-bright`, which maps to [`BoldColor::Bright`]).
    pub bold_color: BoldColor,
    /// Minimum fg/bg contrast ratio (WCAG, `1.0..=21.0`; `1.0` = off). Ghostty
    /// `minimum-contrast`.
    pub min_contrast: f32,
    /// Window background opacity (`0.0` transparent … `1.0` opaque). Ghostty
    /// `background-opacity`. Only cells left on the *default* background go
    /// translucent; see [`Config::background_opacity_cells`].
    ///
    /// *Whether the window can be transparent at all is fixed at startup (the
    /// framebuffer is requested opaque when this is `1.0`), so crossing the `1.0`
    /// line needs a restart — as it does on Ghostty/macOS. The value itself
    /// live-reloads.*
    pub background_opacity: f32,
    /// Extend `background-opacity` to cells that set their own background color.
    /// Ghostty `background-opacity-cells` (default false, i.e. programs that
    /// repaint their background — Neovim, tmux — stay opaque by design).
    pub background_opacity_cells: bool,
    /// Opacity of an *unfocused* split, dimmed so the focused one stands out.
    /// `1.0` disables the effect. Ghostty `unfocused-split-opacity`, including its
    /// unusual `0.15` floor (fully transparent looks broken, so it is disallowed).
    pub unfocused_split_opacity: f32,
    /// Color of the rectangle painted over an unfocused split to dim it; `None`
    /// uses the pane's background. Ghostty `unfocused-split-fill`.
    pub unfocused_split_fill: Option<Rgb>,
    /// Cursor opacity (`0.0`…`1.0`). Ghostty `cursor-opacity`. Applies only to a
    /// focused pane's solid cursor; an unfocused hollow cursor stays opaque.
    pub cursor_opacity: f32,
    /// Opacity of faint/dim (SGR 2) text. Ghostty `faint-opacity`.
    pub faint_opacity: f32,
    /// Backdrop blur behind a translucent window. Ghostty `background-blur`; on
    /// Windows this drives the DWM acrylic/mica backdrop. Has no visible effect
    /// unless `background-opacity` is below `1.0` — the blur shows *through* the
    /// window, so there must be something to see through.
    pub background_blur: BackgroundBlur,
    /// Path to a PNG or JPEG painted behind the grid. Ghostty `background-image`;
    /// a relative path resolves against the config directory. Stored raw and
    /// resolved at the use site, like [`Self::bell_audio_path`].
    pub background_image: Option<String>,
    /// Opacity of that image *relative to* `background-opacity`. Ghostty
    /// `background-image-opacity`: `1.0` lays the image over the background
    /// color and then applies `background-opacity` to the pair; below `1.0`
    /// mixes it with the color first. Values **above** `1.0` are legal and make
    /// the image more opaque than the color behind it (Ghostty's own example:
    /// `background-opacity = 0.5` with `1.5` here gives the image `0.75`).
    pub background_image_opacity: f32,
    /// Where the image sits when it doesn't fill the area. Ghostty
    /// `background-image-position`.
    pub background_image_position: BackgroundImagePosition,
    /// How the image is scaled to the terminal area. Ghostty
    /// `background-image-fit`.
    pub background_image_fit: BackgroundImageFit,
    /// Tile the image to fill the space a non-covering fit leaves. Ghostty
    /// `background-image-repeat`.
    pub background_image_repeat: bool,
    /// Maximum scrollback *lines* retained per pane. Ghostty's `scrollback-limit`
    /// is expressed in bytes; giest's underlying VT engine takes a line count, so
    /// the key name matches but the unit is lines.
    pub scrollback_limit: usize,
    /// Total bytes of image data (kitty graphics) retained per terminal screen.
    /// Ghostty `image-storage-limit`.
    ///
    /// **Zero disables the image protocols entirely** and deletes everything
    /// already stored — that is Ghostty's documented behavior, and it is also
    /// libghostty's starting state, so inline images do not work at all until
    /// this is applied. The limit is per screen, so the effective budget per
    /// pane is double (primary + alternate).
    pub image_storage_limit: u32,
    /// Background color of selected cells. Ghostty `selection-background`.
    pub selection_bg: Rgb,
    /// Text color over a selection; `None` keeps each cell's own foreground.
    /// Ghostty `selection-foreground`.
    pub selection_fg: Option<Rgb>,
    /// Copy a selection to the clipboard as soon as it is made. Ghostty
    /// `copy-on-select` (an enum there; Windows has no primary selection, so
    /// `clipboard`/`primary`/`true` all map to true).
    pub copy_on_select: bool,
    /// What a right-click in a terminal pane does. Ghostty `right-click-action`.
    pub right_click_action: RightClickAction,
    /// What a middle-click in a terminal pane does. Ghostty `middle-click-action`.
    pub middle_click_action: MiddleClickAction,
    /// Configured default shell (name or path); `None` auto-detects. Ghostty
    /// `command`.
    pub shell: Option<String>,
    /// User keybind overrides as raw `(trigger, action)` pairs, applied over the
    /// built-in keymap by the app (`crate::keybind::Keymap::from_config`).
    /// Ghostty `keybind` (repeatable).
    pub keybinds: Vec<(String, String)>,
    /// Precision of OSC color-query replies. Ghostty `osc-color-report-format`.
    pub osc_color_report_format: OscColorReportFormat,
    /// When to confirm before closing a pane/tab/window. Ghostty
    /// `confirm-close-surface`.
    pub confirm_close: ConfirmClose,
    /// When to show the grid-size overlay on resize. Ghostty `resize-overlay`.
    pub resize_overlay: ResizeOverlay,
    /// Where that overlay sits in the pane. Ghostty `resize-overlay-position`.
    pub resize_overlay_position: ResizeOverlayPosition,
    /// How long the overlay stays up, in milliseconds. Ghostty
    /// `resize-overlay-duration`; clamped to a range that's actually perceivable.
    pub resize_overlay_duration_ms: u64,
    /// Whether panes show a scrollbar. Ghostty `scrollbar`.
    pub scrollbar: Scrollbar,
    /// Clipboard permissions and paste protection. Ghostty `clipboard-read`,
    /// `clipboard-write`, `clipboard-trim-trailing-spaces`,
    /// `clipboard-paste-protection`, `clipboard-paste-bracketed-safe`.
    pub clipboard: ClipboardPolicy,
    /// Whether programs may raise desktop notifications (`OSC 9`, `OSC 777`).
    /// Ghostty `desktop-notifications`; `true` by default there and here.
    pub desktop_notifications: bool,
    /// Initial window size in terminal **cells**; `0` means "let the OS decide".
    /// Ghostty `window-width` / `window-height`, including its 10×4 minimum.
    /// Applies to a new window only — resizing later is the user's business.
    pub window_width: u32,
    pub window_height: u32,
    /// Initial window position in pixels from the primary monitor's top-left.
    /// Ghostty `window-position-x` / `-y`; **both** must be set or neither
    /// applies, which is upstream's rule.
    pub window_position_x: Option<i16>,
    pub window_position_y: Option<i16>,
    /// Hide the pointer while typing, until the mouse moves again. Ghostty
    /// `mouse-hide-while-typing`; `false` by default there and here.
    pub mouse_hide_while_typing: bool,
    /// Whether programs may receive mouse events at all. Ghostty
    /// `mouse-reporting`; `false` makes the mouse always select, whatever the
    /// program asks for. Runtime-toggleable via `toggle_mouse_reporting`.
    pub mouse_reporting: bool,
    /// Wheel-distance multipliers. Ghostty `mouse-scroll-multiplier`.
    pub mouse_scroll_multiplier: MouseScrollMultiplier,
    /// When the viewport snaps back to the live edge. Ghostty
    /// `scroll-to-bottom`.
    pub scroll_to_bottom: ScrollToBottom,
    /// Focus the split under the pointer without clicking. Ghostty
    /// `focus-follows-mouse`.
    pub focus_follows_mouse: bool,
    /// Shadertoy-format GLSL post-process shaders, in the order they run.
    /// Ghostty `custom-shader` (repeatable); relative paths resolve against the
    /// config directory.
    pub custom_shaders: Vec<String>,
    /// Whether a custom shader animates. Ghostty `custom-shader-animation`.
    pub custom_shader_animation: CustomShaderAnimation,
    /// Whether programs may drive a progress indicator with ConEmu's `OSC 9;4`.
    /// Ghostty `progress-style` (a bool despite the name); `true` by default.
    pub progress_style: bool,
    /// When a finished command notifies. Ghostty `notify-on-command-finish`;
    /// `never` by default there and here, so this is opt-in.
    pub notify_on_command_finish: NotifyOnCommandFinish,
    /// How it notifies. Ghostty `notify-on-command-finish-action`.
    pub notify_on_command_finish_action: NotifyOnCommandFinishAction,
    /// How long a command must have run to be worth reporting. Ghostty
    /// `notify-on-command-finish-after`, default 5 s.
    pub notify_on_command_finish_after_ms: u64,
    /// Which bell effects fire on BEL. Ghostty `bell-features`.
    pub bell: BellFeatures,
    /// Sound file played when `bell-features` includes `audio`. Ghostty
    /// `bell-audio-path`; a relative path resolves against the config directory.
    pub bell_audio_path: Option<String>,
    /// Ghostty `bell-audio-volume`. **Parsed and stored but not honored**: the
    /// Windows playback path (`PlaySoundW`) has no volume parameter, so honoring
    /// this needs a real audio backend. Kept so a Ghostty config neither warns
    /// nor silently loses the value.
    pub bell_audio_volume: f32,
    /// Whether a new tab inherits the focused pane's working directory (via OSC
    /// 7). Ghostty `tab-inherit-working-directory`.
    pub tab_inherit_working_directory: bool,
    /// Same, for a new split. Ghostty `split-inherit-working-directory`.
    pub split_inherit_working_directory: bool,
    /// Same, for a new window. Ghostty `window-inherit-working-directory`.
    pub window_inherit_working_directory: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_points: 16.0,
            font_family: None,
            font_family_bold: None,
            font_family_italic: None,
            font_family_bold_italic: None,
            font_features: Vec::new(),
            fg: Rgb::new(0xc5, 0xc8, 0xc6),
            bg: Rgb::new(0x10, 0x12, 0x18),
            palette: xterm_palette(GIEST_ANSI16),
            padding_x: 20.0,
            padding_y: 2.0,
            text_gamma: 1.3,
            cursor: None,
            cursor_style: CursorShape::Block,
            cursor_style_blink: None,
            bold_color: BoldColor::None,
            min_contrast: 1.0,
            background_opacity: 1.0,
            background_opacity_cells: false,
            unfocused_split_opacity: 0.7,
            unfocused_split_fill: None,
            cursor_opacity: 1.0,
            faint_opacity: 0.5,
            background_blur: BackgroundBlur::Off,
            background_image: None,
            background_image_opacity: 1.0,
            background_image_position: BackgroundImagePosition::Center,
            background_image_fit: BackgroundImageFit::Contain,
            background_image_repeat: false,
            scrollback_limit: 10_000,
            // Ghostty's default: 320 MB (decimal), per screen.
            image_storage_limit: 320 * 1000 * 1000,
            selection_bg: Rgb::new(0x38, 0x5a, 0x9c),
            selection_fg: None,
            copy_on_select: false,
            right_click_action: RightClickAction::ContextMenu,
            middle_click_action: MiddleClickAction::PrimaryPaste,
            shell: None,
            keybinds: Vec::new(),
            osc_color_report_format: OscColorReportFormat::Bits16,
            confirm_close: ConfirmClose::WhenBusy,
            resize_overlay: ResizeOverlay::AfterFirst,
            resize_overlay_position: ResizeOverlayPosition::Center,
            resize_overlay_duration_ms: 750,
            scrollbar: Scrollbar::System,
            // Ghostty's defaults exactly: reads are asked for (they leak the
            // clipboard *out*), writes are allowed (they only overwrite it).
            clipboard: ClipboardPolicy {
                read: ClipboardAccess::Ask,
                write: ClipboardAccess::Allow,
                trim_trailing_spaces: true,
                paste_protection: true,
                paste_bracketed_safe: true,
            },
            desktop_notifications: true,
            window_width: 0,
            window_height: 0,
            window_position_x: None,
            window_position_y: None,
            mouse_hide_while_typing: false,
            mouse_reporting: true,
            mouse_scroll_multiplier: MouseScrollMultiplier::default(),
            scroll_to_bottom: ScrollToBottom::default(),
            focus_follows_mouse: false,
            custom_shaders: Vec::new(),
            custom_shader_animation: CustomShaderAnimation::True,
            progress_style: true,
            notify_on_command_finish: NotifyOnCommandFinish::Never,
            notify_on_command_finish_action: NotifyOnCommandFinishAction::default(),
            notify_on_command_finish_after_ms: 5_000,
            bell: BellFeatures::default(),
            bell_audio_path: None,
            bell_audio_volume: 0.5,
            tab_inherit_working_directory: true,
            split_inherit_working_directory: true,
            window_inherit_working_directory: true,
        }
    }
}

impl Config {
    /// Load configuration, applying the config file's overrides over the
    /// defaults. A missing file is fine; unreadable lines are logged and skipped.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_ghostty_config(&text),
            Err(_) => Self::default(),
        }
    }

    /// Build a [`Config`] by applying a Ghostty-format config body over the
    /// built-in defaults. This is the entry point [`load`](Self::load) uses after
    /// reading the file; it is public so integration tests and tooling can drive
    /// the parser directly without touching the filesystem.
    pub fn from_ghostty_config(text: &str) -> Self {
        let mut cfg = Self::default();
        // Resolve `theme = ...` first so the theme's colors/palette form a base
        // that the user's own keys then override, regardless of line order
        // (matching Ghostty, where an explicit `background` wins over the theme).
        if let Some(spec) = config_value(text, "theme") {
            cfg.apply_theme_spec(&spec);
        }
        cfg.parse(text);
        cfg
    }

    /// Apply a `theme = ...` spec: resolve it to a theme file and parse that
    /// file's keys into this config as a base. Supports a bare theme name (looked
    /// up in the config dir's `themes/`), an explicit path, and Ghostty's
    /// `light:Foo,dark:Bar` dual form (the dark variant is used; system
    /// appearance switching is a follow-up). Unresolvable themes are ignored.
    fn apply_theme_spec(&mut self, spec: &str) {
        let name = select_theme_variant(spec);
        if name.is_empty() {
            return;
        }
        let Some(path) = resolve_theme_path(&name) else {
            eprintln!("giest: theme '{name}' not found (looked in <config-dir>/themes/)");
            return;
        };
        match std::fs::read_to_string(&path) {
            // Theme files are giest/Ghostty config bodies (palette/fg/bg/…). A
            // nested `theme` key inside a theme file is skipped by `apply`, so
            // this cannot recurse.
            Ok(body) => self.parse(&body),
            Err(e) => eprintln!("giest: could not read theme file {}: {e}", path.display()),
        }
    }

    /// Parse a Ghostty-format config body, applying each `key = value` line.
    /// Blank lines and `#` comment lines are skipped; malformed lines and
    /// unknown keys are logged and ignored (the rest of the file still applies).
    fn parse(&mut self, text: &str) {
        let defaults = Config::default();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                eprintln!("giest: ignoring malformed config line {}: {raw}", i + 1);
                continue;
            };
            self.apply(key.trim(), unquote(value.trim()), &defaults);
        }
    }

    /// Apply a single `key`/`value` pair via the declarative [`SETTERS`] table.
    /// An empty `value` resets the key to its `defaults` value (Ghostty
    /// semantics); an unparseable value keeps the current setting. `theme` is
    /// resolved out-of-band in [`Self::from_ghostty_config`], so it is skipped
    /// here (this also stops a theme file's own `theme` key from recursing).
    fn apply(&mut self, key: &str, value: &str, defaults: &Config) {
        if key == "theme" {
            return;
        }
        match SETTERS.iter().find(|(k, _)| *k == key) {
            Some((_, set)) => set(self, value, defaults),
            None => eprintln!("giest: ignoring unsupported config key '{key}'"),
        }
    }
}

/// Applies one `key = value` to a [`Config`]. `value` is already trimmed and
/// unquoted; an empty value resets the key to its default (read from the
/// `defaults` config) per Ghostty semantics.
type Setter = fn(&mut Config, &str, &Config);

/// Declarative key → setter table. Adding a config key is one entry here plus a
/// field on [`Config`] and its default — the parser, reset-on-empty, and the
/// "unsupported key" handling all come for free. Each closure is non-capturing
/// so it coerces to a plain `fn` pointer.
const SETTERS: &[(&str, Setter)] = &[
    ("font-size", |c, v, d| {
        if v.is_empty() {
            c.font_points = d.font_points;
        } else if let Ok(n) = v.parse::<f32>() {
            if n > 0.0 {
                c.font_points = n;
            }
        }
    }),
    ("font-family", |c, v, d| {
        c.font_family = opt_string(v, &d.font_family)
    }),
    ("font-family-bold", |c, v, d| {
        c.font_family_bold = opt_string(v, &d.font_family_bold)
    }),
    ("font-family-italic", |c, v, d| {
        c.font_family_italic = opt_string(v, &d.font_family_italic)
    }),
    ("font-family-bold-italic", |c, v, d| {
        c.font_family_bold_italic = opt_string(v, &d.font_family_bold_italic)
    }),
    ("font-feature", |c, v, d| {
        // Repeatable: each value is a *comma*-separated list (Ghostty splits only
        // on commas, never whitespace, so a feature's own value syntax may contain
        // spaces, e.g. `liga off`). An empty value resets to the built-in defaults.
        if v.is_empty() {
            c.font_features = d.font_features.clone();
        } else {
            for tok in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                c.font_features.push(tok.to_string());
            }
        }
    }),
    ("foreground", |c, v, d| c.fg = color(v, d.fg, c.fg)),
    ("background", |c, v, d| c.bg = color(v, d.bg, c.bg)),
    ("cursor-color", |c, v, d| c.cursor = opt_color(v, d.cursor, c.cursor)),
    ("cursor-style", |c, v, d| {
        c.cursor_style = match v.to_ascii_lowercase().as_str() {
            "" => d.cursor_style,
            "block" => CursorShape::Block,
            "bar" => CursorShape::Bar,
            "underline" => CursorShape::Underline,
            // Ghostty accepts `block_hollow` as an explicit style too.
            "block_hollow" | "block-hollow" => CursorShape::HollowBlock,
            _ => c.cursor_style,
        }
    }),
    ("cursor-style-blink", |c, v, d| {
        c.cursor_style_blink = match v.to_ascii_lowercase().as_str() {
            "" => d.cursor_style_blink,
            "true" | "1" | "on" | "yes" => Some(true),
            "false" | "0" | "off" | "no" => Some(false),
            _ => c.cursor_style_blink,
        }
    }),
    ("bold-color", |c, v, d| {
        c.bold_color = match v.to_ascii_lowercase().as_str() {
            "" => d.bold_color,
            "bright" => BoldColor::Bright,
            _ => parse_color(v).map(BoldColor::Color).unwrap_or(c.bold_color),
        }
    }),
    ("bold-is-bright", |c, v, d| {
        // Deprecated Ghostty alias merged into `bold-color`: true ⇒ bright.
        c.bold_color = match v.to_ascii_lowercase().as_str() {
            "" => d.bold_color,
            "true" | "1" | "on" | "yes" => BoldColor::Bright,
            "false" | "0" | "off" | "no" => BoldColor::None,
            _ => c.bold_color,
        }
    }),
    ("minimum-contrast", |c, v, d| {
        c.min_contrast = ratio(v, d.min_contrast, c.min_contrast, 1.0, 21.0)
    }),
    ("background-opacity", |c, v, d| {
        c.background_opacity = ratio(v, d.background_opacity, c.background_opacity, 0.0, 1.0)
    }),
    ("background-opacity-cells", |c, v, d| {
        c.background_opacity_cells = if v.is_empty() {
            d.background_opacity_cells
        } else {
            parse_bool(v, c.background_opacity_cells)
        }
    }),
    ("unfocused-split-opacity", |c, v, d| {
        // Ghostty's floor is 0.15, not 0: a fully transparent split "looks very
        // weird", so it clamps up rather than allowing it.
        c.unfocused_split_opacity =
            ratio(v, d.unfocused_split_opacity, c.unfocused_split_opacity, 0.15, 1.0)
    }),
    ("unfocused-split-fill", |c, v, d| {
        c.unfocused_split_fill = opt_color(v, d.unfocused_split_fill, c.unfocused_split_fill)
    }),
    ("cursor-opacity", |c, v, d| {
        c.cursor_opacity = ratio(v, d.cursor_opacity, c.cursor_opacity, 0.0, 1.0)
    }),
    ("faint-opacity", |c, v, d| {
        c.faint_opacity = ratio(v, d.faint_opacity, c.faint_opacity, 0.0, 1.0)
    }),
    ("background-blur", |c, v, d| {
        // Ghostty's parse order — bool first, then the macOS glass names, then an
        // integer intensity. Its `parseBool` accepts only `1`/`t`/`true` (and the
        // `0`/`f`/`false` forms), so `1` means *true* (intensity 20) and only `2`
        // and up reach the numeric branch.
        c.background_blur = match v.to_ascii_lowercase().as_str() {
            "" => d.background_blur,
            "1" | "t" | "true" => BackgroundBlur::On,
            "0" | "f" | "false" => BackgroundBlur::Off,
            // macOS 26 glass effects: no Windows equivalent, and Ghostty itself
            // treats them as plain `true` off macOS since both imply some blur.
            "macos-glass-regular" | "macos-glass-clear" => BackgroundBlur::On,
            _ => v.parse::<u8>().map(BackgroundBlur::Radius).unwrap_or(c.background_blur),
        }
    }),
    ("background-image", |c, v, d| {
        c.background_image = opt_string(v, &d.background_image)
    }),
    ("background-image-opacity", |c, v, d| {
        // *Not* a 0..1 ratio: Ghostty documents values above 1.0 as meaningful
        // (they make the image more opaque than the color it sits on). Only the
        // negative side is nonsense, so that's the only end clamped — the upper
        // bound is a sanity cap, not a semantic one.
        c.background_image_opacity = ratio(
            v,
            d.background_image_opacity,
            c.background_image_opacity,
            0.0,
            16.0,
        )
    }),
    ("background-image-position", |c, v, d| {
        c.background_image_position = match v.to_ascii_lowercase().as_str() {
            "" => d.background_image_position,
            "top-left" => BackgroundImagePosition::TopLeft,
            "top-center" => BackgroundImagePosition::TopCenter,
            "top-right" => BackgroundImagePosition::TopRight,
            "center-left" => BackgroundImagePosition::CenterLeft,
            // Ghostty's enum has both spellings and treats them identically.
            "center" | "center-center" => BackgroundImagePosition::Center,
            "center-right" => BackgroundImagePosition::CenterRight,
            "bottom-left" => BackgroundImagePosition::BottomLeft,
            "bottom-center" => BackgroundImagePosition::BottomCenter,
            "bottom-right" => BackgroundImagePosition::BottomRight,
            _ => c.background_image_position,
        }
    }),
    ("background-image-fit", |c, v, d| {
        c.background_image_fit = match v.to_ascii_lowercase().as_str() {
            "" => d.background_image_fit,
            "contain" => BackgroundImageFit::Contain,
            "cover" => BackgroundImageFit::Cover,
            "stretch" => BackgroundImageFit::Stretch,
            "none" => BackgroundImageFit::None,
            _ => c.background_image_fit,
        }
    }),
    ("background-image-repeat", |c, v, d| {
        c.background_image_repeat = if v.is_empty() {
            d.background_image_repeat
        } else {
            parse_bool(v, c.background_image_repeat)
        }
    }),
    ("window-padding-x", |c, v, d| {
        c.padding_x = padding(v, d.padding_x, c.padding_x)
    }),
    ("window-padding-y", |c, v, d| {
        c.padding_y = padding(v, d.padding_y, c.padding_y)
    }),
    ("text-gamma", |c, v, d| {
        c.text_gamma = ratio(v, d.text_gamma, c.text_gamma, 0.5, 3.0)
    }),
    ("palette", |c, v, d| {
        if v.is_empty() {
            c.palette = d.palette;
        } else if let Some((idx, col)) = parse_palette_entry(v) {
            c.palette[idx as usize] = col;
        } else {
            eprintln!("giest: ignoring bad palette entry: {v}");
        }
    }),
    ("scrollback-limit", |c, v, d| {
        if v.is_empty() {
            c.scrollback_limit = d.scrollback_limit;
        } else if let Ok(n) = v.parse() {
            c.scrollback_limit = n;
        }
    }),
    ("image-storage-limit", |c, v, d| {
        if v.is_empty() {
            c.image_storage_limit = d.image_storage_limit;
        } else if let Ok(n) = v.parse() {
            c.image_storage_limit = n;
        }
    }),
    ("selection-background", |c, v, d| {
        c.selection_bg = color(v, d.selection_bg, c.selection_bg)
    }),
    ("selection-foreground", |c, v, d| {
        c.selection_fg = opt_color(v, d.selection_fg, c.selection_fg)
    }),
    ("copy-on-select", |c, v, d| {
        c.copy_on_select = match v.to_ascii_lowercase().as_str() {
            "" => d.copy_on_select,
            "false" | "0" | "off" | "no" => false,
            "true" | "1" | "on" | "yes" | "clipboard" | "primary" => true,
            _ => c.copy_on_select,
        }
    }),
    ("right-click-action", |c, v, d| {
        c.right_click_action = match v.to_ascii_lowercase().as_str() {
            "" => d.right_click_action,
            "context-menu" => RightClickAction::ContextMenu,
            "copy" => RightClickAction::Copy,
            "paste" => RightClickAction::Paste,
            "copy-or-paste" => RightClickAction::CopyOrPaste,
            "ignore" => RightClickAction::Ignore,
            _ => c.right_click_action,
        }
    }),
    ("middle-click-action", |c, v, d| {
        c.middle_click_action = match v.to_ascii_lowercase().as_str() {
            "" => d.middle_click_action,
            "primary-paste" => MiddleClickAction::PrimaryPaste,
            "ignore" => MiddleClickAction::Ignore,
            _ => c.middle_click_action,
        }
    }),
    ("command", |c, v, d| {
        c.shell = if v.is_empty() {
            d.shell.clone()
        } else {
            Some(v.to_string())
        }
    }),
    ("keybind", |c, v, d| {
        // Repeatable: each `keybind = <trigger>=<action>` appends one override.
        // An empty value (or `clear`) resets to the built-in keymap.
        if v.is_empty() || v.eq_ignore_ascii_case("clear") {
            c.keybinds = d.keybinds.clone();
        } else if let Some((trigger, action)) = v.split_once('=') {
            c.keybinds
                .push((trigger.trim().to_string(), action.trim().to_string()));
        } else {
            eprintln!("giest: ignoring malformed keybind (expected 'trigger=action'): {v}");
        }
    }),
    ("confirm-close-surface", |c, v, d| {
        c.confirm_close = match v.to_ascii_lowercase().as_str() {
            "" => d.confirm_close,
            "false" | "no" | "off" | "0" => ConfirmClose::Never,
            "true" | "yes" | "on" | "1" => ConfirmClose::WhenBusy,
            "always" => ConfirmClose::Always,
            _ => c.confirm_close,
        }
    }),
    ("resize-overlay", |c, v, d| {
        c.resize_overlay = match v.to_ascii_lowercase().as_str() {
            "" => d.resize_overlay,
            "always" => ResizeOverlay::Always,
            "never" => ResizeOverlay::Never,
            "after-first" => ResizeOverlay::AfterFirst,
            _ => c.resize_overlay,
        }
    }),
    ("resize-overlay-position", |c, v, d| {
        c.resize_overlay_position = match v.to_ascii_lowercase().as_str() {
            "" => d.resize_overlay_position,
            "center" => ResizeOverlayPosition::Center,
            "top-left" => ResizeOverlayPosition::TopLeft,
            "top-center" => ResizeOverlayPosition::TopCenter,
            "top-right" => ResizeOverlayPosition::TopRight,
            "bottom-left" => ResizeOverlayPosition::BottomLeft,
            "bottom-center" => ResizeOverlayPosition::BottomCenter,
            "bottom-right" => ResizeOverlayPosition::BottomRight,
            _ => c.resize_overlay_position,
        }
    }),
    ("resize-overlay-duration", |c, v, d| {
        c.resize_overlay_duration_ms = if v.is_empty() {
            d.resize_overlay_duration_ms
        } else {
            // Clamped: below ~250 ms the overlay is gone before it registers, and
            // an unbounded value would pin it on screen indefinitely.
            parse_duration_ms(v)
                .map(|ms| ms.clamp(250, 60_000))
                .unwrap_or(c.resize_overlay_duration_ms)
        }
    }),
    ("clipboard-read", |c, v, d| {
        c.clipboard.read = parse_clipboard_access(v, d.clipboard.read, c.clipboard.read);
    }),
    ("clipboard-write", |c, v, d| {
        c.clipboard.write = parse_clipboard_access(v, d.clipboard.write, c.clipboard.write);
    }),
    ("clipboard-trim-trailing-spaces", |c, v, d| {
        c.clipboard.trim_trailing_spaces = parse_bool(v, d.clipboard.trim_trailing_spaces);
    }),
    ("clipboard-paste-protection", |c, v, d| {
        c.clipboard.paste_protection = parse_bool(v, d.clipboard.paste_protection);
    }),
    ("clipboard-paste-bracketed-safe", |c, v, d| {
        c.clipboard.paste_bracketed_safe = parse_bool(v, d.clipboard.paste_bracketed_safe);
    }),
    ("scrollbar", |c, v, d| {
        c.scrollbar = match v.to_ascii_lowercase().as_str() {
            "" => d.scrollbar,
            "system" => Scrollbar::System,
            "never" => Scrollbar::Never,
            _ => c.scrollbar,
        }
    }),
    ("osc-color-report-format", |c, v, d| {
        c.osc_color_report_format = match v.to_ascii_lowercase().as_str() {
            "" => d.osc_color_report_format,
            "none" => OscColorReportFormat::None,
            "8-bit" => OscColorReportFormat::Bits8,
            "16-bit" => OscColorReportFormat::Bits16,
            _ => c.osc_color_report_format,
        }
    }),
    ("desktop-notifications", |c, v, d| {
        c.desktop_notifications = if v.is_empty() {
            d.desktop_notifications
        } else {
            parse_bool(v, c.desktop_notifications)
        }
    }),
    // Ghostty enforces a 10x4 floor on a *set* size; zero stays zero, meaning
    // "unset". A window narrower than that is unusable rather than merely small.
    ("window-width", |c, v, d| {
        c.window_width = cells(v, d.window_width, c.window_width, 10)
    }),
    ("window-height", |c, v, d| {
        c.window_height = cells(v, d.window_height, c.window_height, 4)
    }),
    ("window-position-x", |c, v, d| {
        c.window_position_x = coord(v, d.window_position_x, c.window_position_x)
    }),
    ("window-position-y", |c, v, d| {
        c.window_position_y = coord(v, d.window_position_y, c.window_position_y)
    }),
    ("mouse-hide-while-typing", |c, v, d| {
        c.mouse_hide_while_typing = if v.is_empty() {
            d.mouse_hide_while_typing
        } else {
            parse_bool(v, c.mouse_hide_while_typing)
        }
    }),
    ("mouse-reporting", |c, v, d| {
        c.mouse_reporting = if v.is_empty() {
            d.mouse_reporting
        } else {
            parse_bool(v, c.mouse_reporting)
        }
    }),
    ("mouse-scroll-multiplier", |c, v, d| {
        c.mouse_scroll_multiplier = if v.is_empty() {
            d.mouse_scroll_multiplier
        } else {
            parse_scroll_multiplier(v, c.mouse_scroll_multiplier)
                .unwrap_or(c.mouse_scroll_multiplier)
        }
    }),
    ("scroll-to-bottom", |c, v, d| {
        c.scroll_to_bottom = if v.is_empty() {
            d.scroll_to_bottom
        } else {
            parse_scroll_to_bottom(v).unwrap_or(c.scroll_to_bottom)
        }
    }),
    ("focus-follows-mouse", |c, v, d| {
        c.focus_follows_mouse = if v.is_empty() {
            d.focus_follows_mouse
        } else {
            parse_bool(v, c.focus_follows_mouse)
        }
    }),
    ("custom-shader", |c, v, d| {
        // Repeatable, like `palette` and `font-feature`: each line appends, and
        // an empty value resets the whole list.
        if v.is_empty() {
            c.custom_shaders = d.custom_shaders.clone();
        } else {
            c.custom_shaders.push(v.to_string());
        }
    }),
    ("custom-shader-animation", |c, v, d| {
        c.custom_shader_animation = match v.to_ascii_lowercase().as_str() {
            "" => d.custom_shader_animation,
            "true" | "1" | "on" | "yes" => CustomShaderAnimation::True,
            "false" | "0" | "off" | "no" => CustomShaderAnimation::False,
            "always" => CustomShaderAnimation::Always,
            _ => c.custom_shader_animation,
        }
    }),
    ("progress-style", |c, v, d| {
        c.progress_style = if v.is_empty() {
            d.progress_style
        } else {
            parse_bool(v, c.progress_style)
        }
    }),
    ("notify-on-command-finish", |c, v, d| {
        c.notify_on_command_finish = match v.to_ascii_lowercase().as_str() {
            "" => d.notify_on_command_finish,
            "never" => NotifyOnCommandFinish::Never,
            "unfocused" => NotifyOnCommandFinish::Unfocused,
            "always" => NotifyOnCommandFinish::Always,
            _ => c.notify_on_command_finish,
        }
    }),
    ("notify-on-command-finish-action", |c, v, d| {
        c.notify_on_command_finish_action = if v.is_empty() {
            d.notify_on_command_finish_action
        } else {
            parse_notify_action(v).unwrap_or(c.notify_on_command_finish_action)
        }
    }),
    ("notify-on-command-finish-after", |c, v, d| {
        c.notify_on_command_finish_after_ms = if v.is_empty() {
            d.notify_on_command_finish_after_ms
        } else {
            // Unclamped at the top (a user may legitimately only want to hear
            // about hour-long jobs), but floored at zero-is-zero: Ghostty
            // accepts `0` and means "every command".
            parse_duration_ms(v).unwrap_or(c.notify_on_command_finish_after_ms)
        }
    }),
    ("bell-features", |c, v, d| {
        c.bell = if v.is_empty() { d.bell } else { parse_bell_features(v).unwrap_or(c.bell) }
    }),
    ("bell-audio-path", |c, v, d| {
        c.bell_audio_path = opt_string(v, &d.bell_audio_path)
    }),
    ("bell-audio-volume", |c, v, d| {
        c.bell_audio_volume = ratio(v, d.bell_audio_volume, c.bell_audio_volume, 0.0, 1.0)
    }),
    // The three inheritance keys are separate in Ghostty and read by one shared
    // decision table (`crate::app::should_inherit_cwd`), so they can't drift.
    ("tab-inherit-working-directory", |c, v, d| {
        c.tab_inherit_working_directory = parse_bool(v, d.tab_inherit_working_directory);
    }),
    ("split-inherit-working-directory", |c, v, d| {
        c.split_inherit_working_directory = parse_bool(v, d.split_inherit_working_directory);
    }),
    ("window-inherit-working-directory", |c, v, d| {
        c.window_inherit_working_directory = parse_bool(v, d.window_inherit_working_directory);
    }),
];

/// Parse a `clipboard-read`/`-write` value: `allow` / `deny` / `ask`. An empty
/// value resets to `default`; anything unrecognized keeps `current`, matching
/// every other enum key.
fn parse_clipboard_access(
    v: &str,
    default: ClipboardAccess,
    current: ClipboardAccess,
) -> ClipboardAccess {
    match v.to_ascii_lowercase().as_str() {
        "" => default,
        "allow" => ClipboardAccess::Allow,
        "deny" => ClipboardAccess::Deny,
        "ask" => ClipboardAccess::Ask,
        _ => current,
    }
}

/// Parse a Ghostty-style boolean (`true`/`false`, `yes`/`no`, `on`/`off`, `1`/`0`),
/// returning `default` for an empty or unrecognized value.
fn parse_bool(v: &str, default: bool) -> bool {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => true,
        "false" | "no" | "off" | "0" => false,
        _ => default,
    }
}

/// Resolve the config file path: `$GIEST_CONFIG` if set, else
/// `%APPDATA%\giest\config` (Ghostty names its file `config`, no extension).
/// Public so the command palette's "Open Config" can reveal it.
pub fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GIEST_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("giest").join("config"))
}

/// The directory the config file lives in — what a relative `Path`-valued key
/// resolves against. `None` when there is no config path at all.
pub fn config_dir() -> Option<PathBuf> {
    config_path().and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Resolve a `Path`-valued config key (`background-image`, `bell-audio-path`)
/// against `config_dir`.
///
/// Relative paths resolve against the config file's own directory — Ghostty
/// resolves its `Path` values the same way — so a config can ship an image or a
/// sound beside itself. An empty/whitespace value means "unset", not "the
/// config directory".
pub fn resolve_path(raw: &str, config_dir: Option<&Path>) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    Some(match config_dir {
        Some(dir) => dir.join(p),
        None => p.to_path_buf(),
    })
}

/// Return the last `key = value` value in a config body, or `None`. Used to find
/// `theme` before the main parse pass (last-wins, like every other key).
fn config_value(text: &str, key: &str) -> Option<String> {
    let mut found = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                found = Some(unquote(v.trim()).to_string());
            }
        }
    }
    found
}

/// Choose the theme name from a `theme` spec. A bare name (or path) is returned
/// as-is; Ghostty's `light:Foo,dark:Bar` dual form resolves to the dark variant
/// (giest renders dark by default; wiring system light/dark switching is a
/// follow-up).
fn select_theme_variant(spec: &str) -> String {
    if spec.contains("light:") || spec.contains("dark:") {
        let (mut light, mut dark) = (None, None);
        for part in spec.split(',') {
            let part = part.trim();
            if let Some(rest) = part.strip_prefix("dark:") {
                dark = Some(rest.trim().to_string());
            } else if let Some(rest) = part.strip_prefix("light:") {
                light = Some(rest.trim().to_string());
            }
        }
        return dark.or(light).unwrap_or_default();
    }
    spec.to_string()
}

/// Resolve a theme name to a file: an explicit existing path wins, otherwise
/// `<config-dir>/themes/<name>` (Ghostty's `themes/` convention). `None` if no
/// such file exists.
fn resolve_theme_path(name: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(name);
    if direct.is_file() {
        return Some(direct);
    }
    let dir = config_path()?.parent()?.to_path_buf();
    let p = dir.join("themes").join(name);
    p.is_file().then_some(p)
}

/// Strip a single pair of matching surrounding quotes, if present. Ghostty
/// values are unquoted, but tolerating quotes eases pasting TOML-style values.
fn unquote(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Resolve an optional string field: an empty value resets to `default`,
/// anything else is taken verbatim. Used for the `font-family*` keys.
fn opt_string(value: &str, default: &Option<String>) -> Option<String> {
    if value.is_empty() {
        default.clone()
    } else {
        Some(value.to_string())
    }
}

/// Resolve a required color field: empty resets to `default`, a valid hex sets
/// it, anything else keeps `current`.
fn color(value: &str, default: Rgb, current: Rgb) -> Rgb {
    if value.is_empty() {
        default
    } else {
        parse_color(value).unwrap_or(current)
    }
}

/// Resolve an optional color field (cursor / selection foreground): empty resets
/// to `default`, a valid hex sets `Some`, anything else keeps `current`.
fn opt_color(value: &str, default: Option<Rgb>, current: Option<Rgb>) -> Option<Rgb> {
    if value.is_empty() {
        default
    } else {
        parse_color(value).map(Some).unwrap_or(current)
    }
}

/// Resolve a padding field: empty resets to `default`, a valid non-negative
/// number sets it (clamped at 0), anything else keeps `current`.
/// Resolve a cell-count field (`window-width`/`-height`): empty resets, `0`
/// means "unset", and any other value is floored at `min` — Ghostty's rule, and
/// a window below it is unusable rather than merely small.
fn cells(value: &str, default: u32, current: u32, min: u32) -> u32 {
    if value.is_empty() {
        return default;
    }
    match value.parse::<u32>() {
        Ok(0) => 0,
        Ok(n) => n.max(min),
        Err(_) => current,
    }
}

/// Resolve an optional pixel coordinate (`window-position-x`/`-y`): empty
/// resets, a valid integer sets it, anything else keeps the current value.
/// Negative is legal — a second monitor left of the primary has negative x.
fn coord(value: &str, default: Option<i16>, current: Option<i16>) -> Option<i16> {
    if value.is_empty() {
        return default;
    }
    value.parse::<i16>().ok().map(Some).unwrap_or(current)
}

fn padding(value: &str, default: f32, current: f32) -> f32 {
    if value.is_empty() {
        default
    } else {
        value.parse::<f32>().map(|p| p.max(0.0)).unwrap_or(current)
    }
}

/// Resolve a clamped ratio field (the various opacities, contrast, gamma): empty
/// resets to `default`, a valid number is clamped into `min..=max`, anything else
/// keeps `current`. Ghostty clamps out-of-range ratios rather than rejecting
/// them, so `background-opacity = 2` means fully opaque, not a parse error.
fn ratio(value: &str, default: f32, current: f32, min: f32, max: f32) -> f32 {
    if value.is_empty() {
        default
    } else {
        value.parse::<f32>().map(|n| n.clamp(min, max)).unwrap_or(current)
    }
}

/// Parse Ghostty's `Duration` grammar into milliseconds.
///
/// A duration is a series of number+unit pairs which **add**, so `1h30m` is 90
/// minutes and even `1h1h` is 2 hours. Units: `y d w h m s ms us`/`µs` `ns`.
/// giest additionally accepts a bare integer as milliseconds — a superset that
/// can't collide, since Ghostty requires a unit on every component.
///
/// Sub-millisecond components are parsed and contribute 0 ms rather than being
/// rejected, so a valid Ghostty config doesn't warn.
fn parse_duration_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Bare integer → milliseconds.
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let mut total_ns: u128 = 0;
    let mut rest = s;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        if digits == 0 {
            return None; // a unit with no number
        }
        let (num, tail) = rest.split_at(digits);
        let n: u128 = num.parse().ok()?;
        let unit_len = tail
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(tail.len());
        let (unit, tail) = tail.split_at(unit_len);
        let ns_per: u128 = match unit {
            "y" => 365 * 24 * 60 * 60 * 1_000_000_000,
            "d" => 24 * 60 * 60 * 1_000_000_000,
            "w" => 7 * 24 * 60 * 60 * 1_000_000_000,
            "h" => 60 * 60 * 1_000_000_000,
            "m" => 60 * 1_000_000_000,
            "s" => 1_000_000_000,
            "ms" => 1_000_000,
            "us" | "µs" => 1_000,
            "ns" => 1,
            _ => return None,
        };
        total_ns = total_ns.saturating_add(n.saturating_mul(ns_per));
        rest = tail;
    }
    Some((total_ns / 1_000_000).min(u64::MAX as u128) as u64)
}

/// Parse a palette override entry of the form `<index>=#rrggbb` into its 0–255
/// palette index and color. Returns `None` for malformed entries or an
/// out-of-range index.
fn parse_palette_entry(s: &str) -> Option<(u8, Rgb)> {
    let (idx, color) = s.split_once('=')?;
    let idx: u8 = idx.trim().parse().ok()?;
    let color = parse_color(color.trim())?;
    Some((idx, color))
}

/// Parse a color value the way Ghostty's `Color.parseCLI` does: an X11 color
/// name (case-insensitive) first, then a hex literal. Used by every color key so
/// values stay transposable with a real Ghostty config.
fn parse_color(s: &str) -> Option<Rgb> {
    x11_colors()
        .get(s.trim().to_ascii_lowercase().as_str())
        .copied()
        .or_else(|| parse_hex(s))
}

/// Parse a hex color: `#rrggbb`/`rrggbb`, or the 3-digit short form
/// `#rgb`/`rgb` (each nibble duplicated, e.g. `#f80` → `#ff8800`), matching
/// Ghostty's `Color.fromHex`.
fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.strip_prefix('#').unwrap_or(s);
    match h.len() {
        6 => {
            let r = u8::from_str_radix(&h[0..2], 16).ok()?;
            let g = u8::from_str_radix(&h[2..4], 16).ok()?;
            let b = u8::from_str_radix(&h[4..6], 16).ok()?;
            Some(Rgb::new(r, g, b))
        }
        3 => {
            // Duplicate each nibble: 0xf → 0xff (× 17).
            let nib = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).ok().map(|v| v * 17);
            Some(Rgb::new(nib(0)?, nib(1)?, nib(2)?))
        }
        _ => None,
    }
}

/// The X11 color-name table (Ghostty's vendored `rgb.txt`), keyed by lowercased
/// name for case-insensitive lookup. Built once on first use. The file lists
/// each color as `R G B<TAB>name`, with both spaced and CamelCase spellings of
/// multi-word names (`ghost white` and `GhostWhite`), so case-insensitive keys
/// cover the space-collapsed form too.
fn x11_colors() -> &'static HashMap<String, Rgb> {
    static MAP: OnceLock<HashMap<String, Rgb>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = HashMap::new();
        for line in include_str!("res/rgb.txt").lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            let (Some(r), Some(g), Some(b)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            let (Ok(r), Ok(g), Ok(b)) = (r.parse::<u8>(), g.parse::<u8>(), b.parse::<u8>()) else {
                continue;
            };
            let name = it.collect::<Vec<_>>().join(" ");
            if !name.is_empty() {
                m.insert(name.to_ascii_lowercase(), Rgb::new(r, g, b));
            }
        }
        m
    })
}

/// The 16 ANSI colors (a calm dark scheme). 0–7 normal, 8–15 bright.
const GIEST_ANSI16: [Rgb; 16] = [
    Rgb::new(0x10, 0x12, 0x18), // black
    Rgb::new(0xcc, 0x66, 0x66), // red
    Rgb::new(0xb5, 0xbd, 0x68), // green
    Rgb::new(0xf0, 0xc6, 0x74), // yellow
    Rgb::new(0x81, 0xa2, 0xbe), // blue
    Rgb::new(0xb2, 0x94, 0xbb), // magenta
    Rgb::new(0x8a, 0xbe, 0xb7), // cyan
    Rgb::new(0xc5, 0xc8, 0xc6), // white
    Rgb::new(0x66, 0x6a, 0x73), // bright black
    Rgb::new(0xd5, 0x4e, 0x53), // bright red
    Rgb::new(0xb9, 0xca, 0x4a), // bright green
    Rgb::new(0xe7, 0xc5, 0x47), // bright yellow
    Rgb::new(0x7a, 0xa6, 0xda), // bright blue
    Rgb::new(0xc3, 0x97, 0xd8), // bright magenta
    Rgb::new(0x70, 0xc0, 0xb1), // bright cyan
    Rgb::new(0xea, 0xea, 0xea), // bright white
];

/// Build the standard 256-color palette from the 16 base ANSI colors.
fn xterm_palette(base16: [Rgb; 16]) -> [Rgb; 256] {
    let mut p = [Rgb::default(); 256];
    p[..16].copy_from_slice(&base16);

    // 16..=231: 6×6×6 RGB cube.
    let steps = [0u8, 95, 135, 175, 215, 255];
    let mut idx = 16;
    for r in 0..6 {
        for g in 0..6 {
            for b in 0..6 {
                p[idx] = Rgb::new(steps[r], steps[g], steps[b]);
                idx += 1;
            }
        }
    }

    // 232..=255: 24-step grayscale ramp.
    for i in 0..24 {
        let v = (8 + i * 10) as u8;
        p[232 + i] = Rgb::new(v, v, v);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a Ghostty-format config body over the defaults.
    fn parsed(body: &str) -> Config {
        Config::from_ghostty_config(body)
    }

    #[test]
    fn palette_is_well_formed() {
        let c = Config::default();
        // ANSI red at index 1, cube corner white at 231, grayscale endpoints.
        assert_eq!(c.palette[1], GIEST_ANSI16[1]);
        assert_eq!(c.palette[231], Rgb::new(255, 255, 255));
        assert_eq!(c.palette[16], Rgb::new(0, 0, 0));
        assert_eq!(c.palette[232], Rgb::new(8, 8, 8));
    }

    #[test]
    fn parses_hex_colors() {
        assert_eq!(parse_hex("#0a141e"), Some(Rgb::new(10, 20, 30)));
        assert_eq!(parse_hex("0a141e"), Some(Rgb::new(10, 20, 30)));
        // 3-digit short form: each nibble is duplicated (Ghostty's fromHex).
        assert_eq!(parse_hex("#f80"), Some(Rgb::new(0xff, 0x88, 0x00)));
        assert_eq!(parse_hex("fff"), Some(Rgb::new(255, 255, 255)));
        assert_eq!(parse_hex("#345"), Some(Rgb::new(0x33, 0x44, 0x55)));
        assert_eq!(parse_hex("nothex"), None);
        assert_eq!(parse_hex("#gg0"), None);
    }

    #[test]
    fn parses_x11_named_colors() {
        // Ghostty resolves color values through an X11 name table before hex.
        assert_eq!(parse_color("black"), Some(Rgb::new(0, 0, 0)));
        assert_eq!(parse_color("red"), Some(Rgb::new(255, 0, 0)));
        assert_eq!(parse_color("white"), Some(Rgb::new(255, 255, 255)));
        // Case-insensitive; the CamelCase spelling collapses spaces.
        assert_eq!(parse_color("ForestGreen"), Some(Rgb::new(34, 139, 34)));
        assert_eq!(parse_color("forestgreen"), Some(Rgb::new(34, 139, 34)));
        assert_eq!(parse_color("medium spring green"), Some(Rgb::new(0, 250, 154)));
        // Hex still works through the same entry point (incl. short form).
        assert_eq!(parse_color("#ff8800"), Some(Rgb::new(0xff, 0x88, 0x00)));
        assert_eq!(parse_color("#f80"), Some(Rgb::new(0xff, 0x88, 0x00)));
        // Unknown names fall through to None.
        assert_eq!(parse_color("nosuchcolor"), None);
    }

    #[test]
    fn color_keys_accept_named_colors() {
        // The named-color path backs every color key, like Ghostty.
        assert_eq!(parsed("foreground = red").fg, Rgb::new(255, 0, 0));
        assert_eq!(parsed("background = black").bg, Rgb::new(0, 0, 0));
        assert_eq!(parsed("cursor-color = blue").cursor, Some(Rgb::new(0, 0, 255)));
        assert_eq!(
            parsed("bold-color = black").bold_color,
            BoldColor::Color(Rgb::new(0, 0, 0))
        );
        assert_eq!(parsed("palette = 1=red").palette[1], Rgb::new(255, 0, 0));
    }

    #[test]
    fn ghostty_keys_override_defaults() {
        // Unquoted, kebab-case, Ghostty-style colors.
        let c = parsed("font-size = 18\nforeground = #ff8800\nbackground = #101010");
        assert_eq!(c.font_points, 18.0);
        assert_eq!(c.fg, Rgb::new(0xff, 0x88, 0x00));
        assert_eq!(c.bg, Rgb::new(0x10, 0x10, 0x10));
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let c = parsed("# a comment\n\n   # indented comment\nfont-size = 20\n");
        assert_eq!(c.font_points, 20.0);
    }

    #[test]
    fn parses_palette_entries() {
        assert_eq!(
            parse_palette_entry("0=#1d1f21"),
            Some((0, Rgb::new(0x1d, 0x1f, 0x21)))
        );
        assert_eq!(
            parse_palette_entry(" 15 = #eaeaea "),
            Some((15, Rgb::new(0xea, 0xea, 0xea)))
        );
        assert_eq!(
            parse_palette_entry("255=#000000"),
            Some((255, Rgb::new(0, 0, 0)))
        );
        assert_eq!(
            parse_palette_entry("256=#000000"),
            None,
            "index out of range"
        );
        assert_eq!(parse_palette_entry("nope"), None);
    }

    #[test]
    fn repeated_palette_keys_accumulate() {
        // Ghostty repeats `palette = N=#hex`, one entry per line.
        let c = parsed("cursor-color = #ff0000\npalette = 1=#abcdef\npalette = 232=#0a0a0a");
        assert_eq!(c.cursor, Some(Rgb::new(0xff, 0, 0)));
        assert_eq!(c.palette[1], Rgb::new(0xab, 0xcd, 0xef));
        assert_eq!(c.palette[232], Rgb::new(0x0a, 0x0a, 0x0a));
        // Untouched indices keep their default.
        assert_eq!(c.palette[2], GIEST_ANSI16[2]);
    }

    #[test]
    fn window_padding_keys_apply() {
        let c = parsed("window-padding-x = 12\nwindow-padding-y = 6");
        assert_eq!(c.padding_x, 12.0);
        assert_eq!(c.padding_y, 6.0);
    }

    #[test]
    fn scrollback_limit_overrides() {
        let c = Config::default();
        assert_eq!(c.scrollback_limit, 10_000);
        assert_eq!(parsed("scrollback-limit = 50000").scrollback_limit, 50_000);
    }

    #[test]
    fn image_storage_limit_defaults_to_ghosttys_value() {
        // Ghostty's default is 320 MB *decimal*, not 320 MiB.
        assert_eq!(Config::default().image_storage_limit, 320_000_000);
        assert_eq!(
            parsed("image-storage-limit = 64000000").image_storage_limit,
            64_000_000
        );
        // Zero is meaningful: it disables the image protocols entirely.
        assert_eq!(parsed("image-storage-limit = 0").image_storage_limit, 0);
        // Empty resets; garbage keeps the current value.
        assert_eq!(
            parsed("image-storage-limit = 0\nimage-storage-limit =").image_storage_limit,
            320_000_000
        );
        assert_eq!(
            parsed("image-storage-limit = 0\nimage-storage-limit = lots").image_storage_limit,
            0
        );
    }

    #[test]
    fn selection_and_copy_on_select_overrides() {
        let c = parsed(
            "selection-background = #385a9c\nselection-foreground = #ffffff\ncopy-on-select = true",
        );
        assert_eq!(c.selection_bg, Rgb::new(0x38, 0x5a, 0x9c));
        assert_eq!(c.selection_fg, Some(Rgb::new(0xff, 0xff, 0xff)));
        assert!(c.copy_on_select);
    }

    #[test]
    fn inherit_working_directory_keys_default_true_and_override() {
        let d = Config::default();
        assert!(d.tab_inherit_working_directory);
        assert!(d.split_inherit_working_directory);
        assert!(d.window_inherit_working_directory);

        assert!(!parsed("tab-inherit-working-directory = false").tab_inherit_working_directory);
        assert!(!parsed("split-inherit-working-directory = false").split_inherit_working_directory);
        assert!(
            !parsed("window-inherit-working-directory = false").window_inherit_working_directory
        );

        // An empty value resets to the default (true).
        assert!(parsed("tab-inherit-working-directory =").tab_inherit_working_directory);
        assert!(parsed("split-inherit-working-directory =").split_inherit_working_directory);

        // Each key is independent — turning one off must not disturb the others.
        let c = parsed("split-inherit-working-directory = false");
        assert!(c.tab_inherit_working_directory && c.window_inherit_working_directory);
    }

    #[test]
    fn copy_on_select_accepts_ghostty_enum() {
        assert!(parsed("copy-on-select = clipboard").copy_on_select);
        assert!(parsed("copy-on-select = primary").copy_on_select);
        assert!(!parsed("copy-on-select = false").copy_on_select);
    }

    #[test]
    fn click_action_defaults() {
        let d = Config::default();
        assert_eq!(d.right_click_action, RightClickAction::ContextMenu);
        assert_eq!(d.middle_click_action, MiddleClickAction::PrimaryPaste);
        // Empty config keeps the defaults.
        assert_eq!(parsed("").right_click_action, RightClickAction::ContextMenu);
        assert_eq!(
            parsed("").middle_click_action,
            MiddleClickAction::PrimaryPaste
        );
    }

    #[test]
    fn right_click_action_parses_each_value() {
        assert_eq!(
            parsed("right-click-action = context-menu").right_click_action,
            RightClickAction::ContextMenu
        );
        assert_eq!(
            parsed("right-click-action = copy").right_click_action,
            RightClickAction::Copy
        );
        assert_eq!(
            parsed("right-click-action = paste").right_click_action,
            RightClickAction::Paste
        );
        assert_eq!(
            parsed("right-click-action = copy-or-paste").right_click_action,
            RightClickAction::CopyOrPaste
        );
        assert_eq!(
            parsed("right-click-action = ignore").right_click_action,
            RightClickAction::Ignore
        );
        // Empty resets to default; garbage keeps the current (here, the default).
        assert_eq!(
            parsed("right-click-action =").right_click_action,
            RightClickAction::ContextMenu
        );
        assert_eq!(
            parsed("right-click-action = nonsense").right_click_action,
            RightClickAction::ContextMenu
        );
    }

    #[test]
    fn middle_click_action_parses_each_value() {
        assert_eq!(
            parsed("middle-click-action = primary-paste").middle_click_action,
            MiddleClickAction::PrimaryPaste
        );
        assert_eq!(
            parsed("middle-click-action = ignore").middle_click_action,
            MiddleClickAction::Ignore
        );
        assert_eq!(
            parsed("middle-click-action =").middle_click_action,
            MiddleClickAction::PrimaryPaste
        );
        assert_eq!(
            parsed("middle-click-action = nonsense").middle_click_action,
            MiddleClickAction::PrimaryPaste
        );
    }

    #[test]
    fn command_sets_shell() {
        assert_eq!(parsed("command = pwsh").shell.as_deref(), Some("pwsh"));
        // Quoted values are tolerated for paths with spaces.
        assert_eq!(
            parsed("command = \"C:\\Program Files\\bash.exe\"")
                .shell
                .as_deref(),
            Some("C:\\Program Files\\bash.exe")
        );
    }

    #[test]
    fn unknown_keys_are_ignored_not_fatal() {
        // A full Ghostty config has many keys we don't support; they must not
        // discard the keys we do support.
        let c = parsed("theme = catppuccin\nfont-family = JetBrains Mono\nfont-size = 14");
        assert_eq!(c.font_points, 14.0);
    }

    #[test]
    fn empty_value_resets_to_default() {
        let mut c = Config::default();
        c.parse("font-size = 30\ncursor-color = #ff0000");
        assert_eq!(c.font_points, 30.0);
        assert_eq!(c.cursor, Some(Rgb::new(0xff, 0, 0)));
        // An empty value restores the default (Ghostty semantics).
        c.parse("font-size =\ncursor-color =");
        assert_eq!(c.font_points, Config::default().font_points);
        assert_eq!(c.cursor, None);
    }

    #[test]
    fn empty_config_keeps_defaults() {
        let c = parsed("");
        let d = Config::default();
        assert_eq!((c.font_points, c.fg, c.bg), (d.font_points, d.fg, d.bg));
    }

    #[test]
    fn text_gamma_overrides_and_clamps() {
        assert_eq!(Config::default().text_gamma, 1.3);
        assert_eq!(parsed("").text_gamma, 1.3);
        assert_eq!(parsed("text-gamma = 1.6").text_gamma, 1.6);
        // Out-of-range values clamp to [0.5, 3.0].
        assert_eq!(parsed("text-gamma = 10.0").text_gamma, 3.0);
        assert_eq!(parsed("text-gamma = 0.1").text_gamma, 0.5);
    }

    #[test]
    fn confirm_close_surface_parses_ghostty_enum() {
        assert_eq!(Config::default().confirm_close, ConfirmClose::WhenBusy);
        assert_eq!(parsed("confirm-close-surface = false").confirm_close, ConfirmClose::Never);
        assert_eq!(parsed("confirm-close-surface = true").confirm_close, ConfirmClose::WhenBusy);
        assert_eq!(parsed("confirm-close-surface = always").confirm_close, ConfirmClose::Always);
        assert_eq!(
            parsed("confirm-close-surface = false\nconfirm-close-surface = bogus").confirm_close,
            ConfirmClose::Never
        );
        assert_eq!(
            parsed("confirm-close-surface = false\nconfirm-close-surface =").confirm_close,
            ConfirmClose::WhenBusy
        );
    }

    #[test]
    fn confirm_close_decision_table() {
        use super::needs_confirm as nc;
        // `false` never asks, whatever the pane is doing.
        for busy in [None, Some(true), Some(false)] {
            assert!(!nc(ConfirmClose::Never, busy), "{busy:?}");
            assert!(nc(ConfirmClose::Always, busy), "{busy:?}");
        }
        // `true` asks only when something is (or might be) running.
        assert!(nc(ConfirmClose::WhenBusy, Some(true)));
        assert!(!nc(ConfirmClose::WhenBusy, Some(false)));
        // Unknown → confirm: a shell with no OSC 133 marks must not lose work.
        assert!(nc(ConfirmClose::WhenBusy, None));
    }

    #[test]
    fn resize_overlay_keys_parse_ghostty_values() {
        let d = Config::default();
        assert_eq!(d.resize_overlay, ResizeOverlay::AfterFirst);
        assert_eq!(d.resize_overlay_position, ResizeOverlayPosition::Center);
        assert_eq!(d.resize_overlay_duration_ms, 750);

        assert_eq!(parsed("resize-overlay = always").resize_overlay, ResizeOverlay::Always);
        assert_eq!(parsed("resize-overlay = never").resize_overlay, ResizeOverlay::Never);
        assert_eq!(
            parsed("resize-overlay-position = bottom-right").resize_overlay_position,
            ResizeOverlayPosition::BottomRight
        );
        // Garbage keeps the current value; empty resets.
        assert_eq!(
            parsed("resize-overlay = never\nresize-overlay = bogus").resize_overlay,
            ResizeOverlay::Never
        );
        assert_eq!(
            parsed("resize-overlay = never\nresize-overlay =").resize_overlay,
            ResizeOverlay::AfterFirst
        );
    }

    /// The default policy, so each test below varies one knob at a time.
    fn policy() -> ClipboardPolicy {
        Config::default().clipboard
    }

    #[test]
    fn clipboard_defaults_match_ghostty() {
        let p = policy();
        // Reads leak the clipboard *out* to the program, so they're asked for;
        // writes only overwrite it, so they're allowed.
        assert_eq!(p.read, ClipboardAccess::Ask);
        assert_eq!(p.write, ClipboardAccess::Allow);
        assert!(p.trim_trailing_spaces);
        assert!(p.paste_protection);
        assert!(p.paste_bracketed_safe);
    }

    #[test]
    fn clipboard_access_keys_parse_ghostty_values() {
        assert_eq!(parsed("clipboard-read = allow").clipboard.read, ClipboardAccess::Allow);
        assert_eq!(parsed("clipboard-read = deny").clipboard.read, ClipboardAccess::Deny);
        assert_eq!(parsed("clipboard-write = ask").clipboard.write, ClipboardAccess::Ask);
        // Garbage keeps the current value; empty resets to the default.
        assert_eq!(
            parsed("clipboard-write = deny\nclipboard-write = maybe")
                .clipboard
                .write,
            ClipboardAccess::Deny
        );
        assert_eq!(
            parsed("clipboard-write = deny\nclipboard-write =").clipboard.write,
            ClipboardAccess::Allow
        );
    }

    #[test]
    fn plain_text_is_always_safe_to_paste() {
        assert!(!paste_is_unsafe(policy(), false, "hello"));
        assert!(!paste_is_unsafe(policy(), true, "hello"));
        // A trailing-newline-free multi-token command is still one line.
        assert!(!paste_is_unsafe(policy(), false, "git commit -m 'x'"));
    }

    #[test]
    fn newlines_are_unsafe_unless_the_program_brackets_the_paste() {
        // Unbracketed, a newline is a command the shell runs immediately.
        assert!(paste_is_unsafe(policy(), false, "ls\nrm -rf /"));
        assert!(paste_is_unsafe(policy(), false, "trailing\n"));
        // Bracketed, the program has promised to treat it as data.
        assert!(!paste_is_unsafe(policy(), true, "ls\nrm -rf /"));
    }

    #[test]
    fn the_end_marker_is_never_trusted_even_when_bracketed() {
        // This is the attack `clipboard-paste-bracketed-safe` cannot defend
        // against: the payload closes the bracket itself, so everything after
        // it arrives as if typed. Checked *before* the bracketed-safe bail-out.
        let attack = "safe\x1b[201~rm -rf /";
        assert!(paste_is_unsafe(policy(), true, attack));
        assert!(paste_is_unsafe(policy(), false, attack));
        // And still flagged with bracketed-safe explicitly on.
        let trusting = ClipboardPolicy {
            paste_bracketed_safe: true,
            ..policy()
        };
        assert!(paste_is_unsafe(trusting, true, attack));
    }

    #[test]
    fn distrusting_bracketing_falls_back_to_the_plain_rule() {
        let strict = ClipboardPolicy {
            paste_bracketed_safe: false,
            ..policy()
        };
        // Bracketing no longer excuses a newline…
        assert!(paste_is_unsafe(strict, true, "ls\nrm -rf /"));
        // …but plain text is still fine.
        assert!(!paste_is_unsafe(strict, true, "hello"));
    }

    #[test]
    fn disabling_protection_allows_everything() {
        let off = ClipboardPolicy {
            paste_protection: false,
            ..policy()
        };
        assert!(!paste_is_unsafe(off, false, "ls\nrm -rf /"));
        assert!(!paste_is_unsafe(off, true, "safe\x1b[201~rm -rf /"));
    }

    #[test]
    fn scrollbar_key_parses_ghostty_values() {
        assert_eq!(Config::default().scrollbar, Scrollbar::System);
        assert_eq!(parsed("scrollbar = never").scrollbar, Scrollbar::Never);
        assert_eq!(parsed("scrollbar = system").scrollbar, Scrollbar::System);
        // Ghostty has no third value; anything else keeps the current one…
        assert_eq!(
            parsed("scrollbar = never\nscrollbar = always").scrollbar,
            Scrollbar::Never
        );
        // …and an empty value resets to the default.
        assert_eq!(parsed("scrollbar = never\nscrollbar =").scrollbar, Scrollbar::System);
    }

    #[test]
    fn resize_overlay_duration_parses_ghostty_grammar() {
        let ms = |s: &str| parsed(&format!("resize-overlay-duration = {s}")).resize_overlay_duration_ms;
        assert_eq!(ms("750ms"), 750);
        assert_eq!(ms("45s"), 45_000);
        // Components add…
        assert_eq!(ms("1h30m"), 5_400_000_u64.min(60_000));
        // …and repeat rather than overwrite (1h1h == 2h), though both clamp here.
        assert_eq!(ms("2s500ms"), 2_500);
        // A bare integer is milliseconds (a giest superset).
        assert_eq!(ms("300"), 300);
        // Clamped at both ends.
        assert_eq!(ms("10ms"), 250);
        assert_eq!(ms("999y"), 60_000);
        // Sub-millisecond parses but contributes nothing, then clamps up.
        assert_eq!(ms("500us"), 250);
        // Garbage keeps the current value.
        assert_eq!(
            parsed("resize-overlay-duration = 2s\nresize-overlay-duration = soon")
                .resize_overlay_duration_ms,
            2_000
        );
    }

    #[test]
    fn show_resize_overlay_gates_the_first_layout() {
        use super::show_resize_overlay as show;
        // The whole point of `after-first`: a session's initial sizing is not a
        // "resize" the user did, so it must not flash.
        assert!(!show(ResizeOverlay::AfterFirst, true));
        assert!(show(ResizeOverlay::AfterFirst, false));
        assert!(show(ResizeOverlay::Always, true));
        assert!(show(ResizeOverlay::Always, false));
        assert!(!show(ResizeOverlay::Never, true));
        assert!(!show(ResizeOverlay::Never, false));
    }

    #[test]
    fn osc_color_report_format_parses_each_value() {
        assert_eq!(Config::default().osc_color_report_format, OscColorReportFormat::Bits16);
        assert_eq!(
            parsed("osc-color-report-format = none").osc_color_report_format,
            OscColorReportFormat::None
        );
        assert_eq!(
            parsed("osc-color-report-format = 8-bit").osc_color_report_format,
            OscColorReportFormat::Bits8
        );
        // Garbage keeps the current value; empty resets.
        assert_eq!(
            parsed("osc-color-report-format = none\nosc-color-report-format = bogus")
                .osc_color_report_format,
            OscColorReportFormat::None
        );
        assert_eq!(
            parsed("osc-color-report-format = none\nosc-color-report-format =")
                .osc_color_report_format,
            OscColorReportFormat::Bits16
        );
    }

    #[test]
    fn bell_features_defaults_match_ghostty() {
        let b = Config::default().bell;
        // attention/title on, everything else off — note `border` is OFF, which
        // differs from giest's pre-parity default of always flashing the pane.
        assert!(b.attention && b.title);
        assert!(!b.system && !b.audio && !b.border);
    }

    #[test]
    fn bell_features_parses_feature_list() {
        // A feature list starts from the *defaults*, so attention/title survive.
        let b = parsed("bell-features = audio").bell;
        assert!(b.audio && b.attention && b.title);
        assert!(!b.system && !b.border);

        // `no-` disables one feature and leaves the rest at their defaults.
        let b = parsed("bell-features = no-title").bell;
        assert!(!b.title && b.attention);

        let b = parsed("bell-features = system,border,no-attention").bell;
        assert!(b.system && b.border && b.title);
        assert!(!b.attention && !b.audio);
    }

    #[test]
    fn bell_features_bool_shorthand_sets_all() {
        for v in ["true", "t", "1"] {
            let b = parsed(&format!("bell-features = {v}")).bell;
            assert!(b.system && b.audio && b.attention && b.title && b.border, "{v}");
        }
        for v in ["false", "f", "0"] {
            let b = parsed(&format!("bell-features = {v}")).bell;
            assert!(!b.system && !b.audio && !b.attention && !b.title && !b.border, "{v}");
        }
        // Ghostty's packed-struct parser takes a *stricter* bool set than its
        // ordinary one, so these are unknown feature names, not booleans — and an
        // unknown name keeps the current value (see the wholesale-reject test).
        let b = parsed("bell-features = border\nbell-features = on").bell;
        assert!(b.border, "'on' is not a packed-struct boolean");
    }

    #[test]
    fn bell_features_rejects_unknown_token_wholesale() {
        // The valid `audio` before the bad token must NOT be kept.
        let c = parsed("bell-features = audio,bogus");
        assert_eq!(c.bell, BellFeatures::default());
    }

    #[test]
    fn bell_features_replaces_rather_than_accumulates() {
        // The second line wins outright: `system` is dropped, not merged.
        let b = parsed("bell-features = system\nbell-features = border").bell;
        assert!(b.border && !b.system);
    }

    #[test]
    fn bell_features_empty_resets_to_default() {
        let b = parsed("bell-features = true\nbell-features =").bell;
        assert_eq!(b, BellFeatures::default());
    }

    #[test]
    fn bell_audio_keys_parse() {
        assert_eq!(Config::default().bell_audio_path, None);
        assert_eq!(Config::default().bell_audio_volume, 0.5);
        assert_eq!(
            parsed(r"bell-audio-path = C:\sounds\ding.wav").bell_audio_path.as_deref(),
            Some(r"C:\sounds\ding.wav")
        );
        // Parsed and clamped even though playback can't honor it yet.
        assert_eq!(parsed("bell-audio-volume = 0.25").bell_audio_volume, 0.25);
        assert_eq!(parsed("bell-audio-volume = 9").bell_audio_volume, 1.0);
    }

    #[test]
    fn background_opacity_parses_and_clamps() {
        assert_eq!(Config::default().background_opacity, 1.0);
        assert_eq!(parsed("background-opacity = 0.85").background_opacity, 0.85);
        // Ghostty clamps out-of-range opacities instead of rejecting them.
        assert_eq!(parsed("background-opacity = -1").background_opacity, 0.0);
        assert_eq!(parsed("background-opacity = 2").background_opacity, 1.0);
        // Empty resets; garbage keeps the current value.
        assert_eq!(
            parsed("background-opacity = 0.5\nbackground-opacity =").background_opacity,
            1.0
        );
        assert_eq!(
            parsed("background-opacity = 0.5\nbackground-opacity = nope").background_opacity,
            0.5
        );
    }

    #[test]
    fn background_opacity_cells_parses() {
        assert!(!Config::default().background_opacity_cells);
        assert!(parsed("background-opacity-cells = true").background_opacity_cells);
        assert!(!parsed("background-opacity-cells = false").background_opacity_cells);
        assert!(
            !parsed("background-opacity-cells = true\nbackground-opacity-cells =")
                .background_opacity_cells
        );
    }

    #[test]
    fn unfocused_split_opacity_clamps_to_ghostty_range() {
        assert_eq!(Config::default().unfocused_split_opacity, 0.7);
        assert_eq!(parsed("unfocused-split-opacity = 0.4").unfocused_split_opacity, 0.4);
        // The floor is 0.15, not 0 — Ghostty disallows a fully invisible split.
        assert_eq!(parsed("unfocused-split-opacity = 0.0").unfocused_split_opacity, 0.15);
        assert_eq!(parsed("unfocused-split-opacity = 2.0").unfocused_split_opacity, 1.0);
    }

    #[test]
    fn unfocused_split_fill_defaults_none_and_parses_named_color() {
        assert_eq!(Config::default().unfocused_split_fill, None);
        assert_eq!(
            parsed("unfocused-split-fill = #f80").unfocused_split_fill,
            Some(Rgb::new(0xff, 0x88, 0x00))
        );
        assert_eq!(
            parsed("unfocused-split-fill = rebeccapurple").unfocused_split_fill,
            Some(Rgb::new(0x66, 0x33, 0x99))
        );
    }

    #[test]
    fn cursor_and_faint_opacity_parse_and_clamp() {
        assert_eq!(Config::default().cursor_opacity, 1.0);
        // giest's faint used to be a hardcoded 0.55; Ghostty's default is 0.5.
        assert_eq!(Config::default().faint_opacity, 0.5);
        assert_eq!(parsed("cursor-opacity = 0.4").cursor_opacity, 0.4);
        assert_eq!(parsed("cursor-opacity = 5").cursor_opacity, 1.0);
        assert_eq!(parsed("faint-opacity = 0.1").faint_opacity, 0.1);
        assert_eq!(parsed("faint-opacity = -3").faint_opacity, 0.0);
    }

    #[test]
    fn background_blur_parses_ghostty_grammar() {
        use super::BackgroundBlur;
        assert_eq!(Config::default().background_blur, BackgroundBlur::Off);
        assert_eq!(parsed("background-blur = true").background_blur, BackgroundBlur::On);
        assert_eq!(parsed("background-blur = false").background_blur, BackgroundBlur::Off);
        // Ghostty's parseBool takes `0`/`1`, so those are bools — NOT radii.
        assert_eq!(parsed("background-blur = 0").background_blur, BackgroundBlur::Off);
        assert_eq!(parsed("background-blur = 1").background_blur, BackgroundBlur::On);
        // Only 2 and up reach the numeric branch.
        assert_eq!(
            parsed("background-blur = 20").background_blur,
            BackgroundBlur::Radius(20)
        );
        // Out of u8 range: keep the current value, like Ghostty's parseInt error.
        assert_eq!(
            parsed("background-blur = 20\nbackground-blur = 300").background_blur,
            BackgroundBlur::Radius(20)
        );
        // macOS glass implies plain `true` off macOS.
        assert_eq!(
            parsed("background-blur = macos-glass-clear").background_blur,
            BackgroundBlur::On
        );

        assert_eq!(BackgroundBlur::On.intensity(), 20);
        // `Radius(0)` is disabled, matching Ghostty.
        assert!(!BackgroundBlur::Radius(0).enabled());
        assert!(BackgroundBlur::Radius(1).enabled());
    }

    #[test]
    fn window_geometry_keys() {
        let d = Config::default();
        assert_eq!((d.window_width, d.window_height), (0, 0), "0 means unset");
        assert_eq!(d.window_position_x, None);

        assert_eq!(parsed("window-width = 120").window_width, 120);
        assert_eq!(parsed("window-height = 40").window_height, 40);
        // Ghostty's 10x4 floor: a window below it is unusable, not merely small.
        assert_eq!(parsed("window-width = 2").window_width, 10);
        assert_eq!(parsed("window-height = 1").window_height, 4);
        // …but zero still means "let the OS decide", not "the minimum".
        assert_eq!(parsed("window-width = 0").window_width, 0);
        // Garbage keeps the current value; empty resets.
        assert_eq!(parsed("window-width = 80\nwindow-width = wide").window_width, 80);
        assert_eq!(parsed("window-width = 80\nwindow-width =").window_width, 0);

        assert_eq!(parsed("window-position-x = 100").window_position_x, Some(100));
        // Negative is legal: a monitor left of the primary has negative x.
        assert_eq!(parsed("window-position-y = -40").window_position_y, Some(-40));
        assert_eq!(
            parsed("window-position-x = 10\nwindow-position-x =").window_position_x,
            None
        );
    }

    #[test]
    fn mouse_scroll_multiplier_grammar() {
        let d = Config::default().mouse_scroll_multiplier;
        // Ghostty's split defaults: notched wheels move further per event.
        assert_eq!(d.precision, 1.0);
        assert_eq!(d.discrete, 3.0);

        // A bare number sets both.
        let m = parsed("mouse-scroll-multiplier = 2").mouse_scroll_multiplier;
        assert_eq!((m.precision, m.discrete), (2.0, 2.0));

        // Prefixes set them independently — Ghostty's own example.
        let m = parsed("mouse-scroll-multiplier = precision:0.1,discrete:3").mouse_scroll_multiplier;
        assert_eq!((m.precision, m.discrete), (0.1, 3.0));

        // One prefix leaves the other alone.
        let m = parsed("mouse-scroll-multiplier = discrete:5").mouse_scroll_multiplier;
        assert_eq!((m.precision, m.discrete), (1.0, 5.0));

        // Clamped to Ghostty's range at both ends.
        let m = parsed("mouse-scroll-multiplier = 0").mouse_scroll_multiplier;
        assert_eq!(m.discrete, 0.01, "zero would make the wheel a no-op");
        let m = parsed("mouse-scroll-multiplier = 999999").mouse_scroll_multiplier;
        assert_eq!(m.discrete, 10_000.0);

        // Garbage keeps the current value rather than half-applying.
        let m = parsed("mouse-scroll-multiplier = 4\nmouse-scroll-multiplier = sideways:2")
            .mouse_scroll_multiplier;
        assert_eq!((m.precision, m.discrete), (4.0, 4.0));
        // Empty resets.
        let m = parsed("mouse-scroll-multiplier = 4\nmouse-scroll-multiplier =")
            .mouse_scroll_multiplier;
        assert_eq!((m.precision, m.discrete), (1.0, 3.0));
    }

    #[test]
    fn scroll_to_bottom_flags() {
        let d = Config::default().scroll_to_bottom;
        // Ghostty's default is `keystroke, no-output`.
        assert!(d.keystroke && !d.output);

        let s = parsed("scroll-to-bottom = output").scroll_to_bottom;
        assert!(s.keystroke && s.output, "a list starts from the defaults");
        let s = parsed("scroll-to-bottom = no-keystroke").scroll_to_bottom;
        assert!(!s.keystroke && !s.output);
        let s = parsed("scroll-to-bottom = no-keystroke,output").scroll_to_bottom;
        assert!(!s.keystroke && s.output);
        // An unknown name rejects the whole value.
        let s = parsed("scroll-to-bottom = no-keystroke\nscroll-to-bottom = output,bogus")
            .scroll_to_bottom;
        assert!(!s.keystroke, "a bad value must not half-apply");
    }

    #[test]
    fn notify_on_command_finish_keys_match_ghostty() {
        let d = Config::default();
        // Opt-in: nothing happens until you ask for it.
        assert_eq!(d.notify_on_command_finish, NotifyOnCommandFinish::Never);
        assert!(d.notify_on_command_finish_action.bell);
        assert!(!d.notify_on_command_finish_action.notify);
        assert_eq!(d.notify_on_command_finish_after_ms, 5_000);

        for (v, want) in [
            ("never", NotifyOnCommandFinish::Never),
            ("unfocused", NotifyOnCommandFinish::Unfocused),
            ("always", NotifyOnCommandFinish::Always),
        ] {
            assert_eq!(
                parsed(&format!("notify-on-command-finish = {v}")).notify_on_command_finish,
                want
            );
        }
        // Unknown keeps the current value; empty resets to the default.
        assert_eq!(
            parsed("notify-on-command-finish = always\nnotify-on-command-finish = bogus")
                .notify_on_command_finish,
            NotifyOnCommandFinish::Always
        );
        assert_eq!(
            parsed("notify-on-command-finish = always\nnotify-on-command-finish =")
                .notify_on_command_finish,
            NotifyOnCommandFinish::Never
        );

        // Ghostty's own documented example.
        let a = parsed("notify-on-command-finish-action = no-bell,notify")
            .notify_on_command_finish_action;
        assert!(!a.bell && a.notify);
        // A list starts from the defaults, so naming only `notify` leaves the
        // bell on — same rule as `bell-features`.
        let a = parsed("notify-on-command-finish-action = notify").notify_on_command_finish_action;
        assert!(a.bell && a.notify);
        // One unknown name rejects the whole value.
        let a = parsed("notify-on-command-finish-action = no-bell\nnotify-on-command-finish-action = bell,nope")
            .notify_on_command_finish_action;
        assert!(!a.bell, "a bad value must not half-apply");

        // Ghostty's additive duration grammar, shared with resize-overlay.
        assert_eq!(
            parsed("notify-on-command-finish-after = 45s").notify_on_command_finish_after_ms,
            45_000
        );
        assert_eq!(
            parsed("notify-on-command-finish-after = 1h30m").notify_on_command_finish_after_ms,
            5_400_000
        );
        // Zero means "every command" — this key is deliberately not clamped the
        // way `resize-overlay-duration` is.
        assert_eq!(
            parsed("notify-on-command-finish-after = 0").notify_on_command_finish_after_ms,
            0
        );
    }

    #[test]
    fn notify_focus_gate() {
        use super::should_notify_on_finish as go;
        // never: nothing, either way.
        assert!(!go(NotifyOnCommandFinish::Never, true));
        assert!(!go(NotifyOnCommandFinish::Never, false));
        // unfocused: only when you've looked away.
        assert!(!go(NotifyOnCommandFinish::Unfocused, true));
        assert!(go(NotifyOnCommandFinish::Unfocused, false));
        // always: both.
        assert!(go(NotifyOnCommandFinish::Always, true));
        assert!(go(NotifyOnCommandFinish::Always, false));
    }

    #[test]
    fn background_image_keys_match_ghostty() {
        let d = Config::default();
        assert_eq!(d.background_image, None);
        assert_eq!(d.background_image_opacity, 1.0);
        assert_eq!(d.background_image_position, BackgroundImagePosition::Center);
        assert_eq!(d.background_image_fit, BackgroundImageFit::Contain);
        assert!(!d.background_image_repeat);

        // The path is stored raw; resolution happens at the use site.
        assert_eq!(
            parsed(r"background-image = C:\pics\wall.png")
                .background_image
                .as_deref(),
            Some(r"C:\pics\wall.png")
        );
        assert_eq!(
            parsed("background-image = wall.png\nbackground-image =").background_image,
            None
        );

        for (v, want) in [
            ("contain", BackgroundImageFit::Contain),
            ("cover", BackgroundImageFit::Cover),
            ("stretch", BackgroundImageFit::Stretch),
            ("none", BackgroundImageFit::None),
        ] {
            assert_eq!(
                parsed(&format!("background-image-fit = {v}")).background_image_fit,
                want
            );
        }
        // An unknown value keeps the current setting rather than resetting.
        assert_eq!(
            parsed("background-image-fit = cover\nbackground-image-fit = bogus").background_image_fit,
            BackgroundImageFit::Cover
        );

        for (v, want) in [
            ("top-left", BackgroundImagePosition::TopLeft),
            ("top-center", BackgroundImagePosition::TopCenter),
            ("top-right", BackgroundImagePosition::TopRight),
            ("center-left", BackgroundImagePosition::CenterLeft),
            ("center", BackgroundImagePosition::Center),
            // Ghostty's enum spells the middle both ways.
            ("center-center", BackgroundImagePosition::Center),
            ("center-right", BackgroundImagePosition::CenterRight),
            ("bottom-left", BackgroundImagePosition::BottomLeft),
            ("bottom-center", BackgroundImagePosition::BottomCenter),
            ("bottom-right", BackgroundImagePosition::BottomRight),
        ] {
            assert_eq!(
                parsed(&format!("background-image-position = {v}")).background_image_position,
                want,
                "{v}"
            );
        }

        assert!(parsed("background-image-repeat = true").background_image_repeat);
        assert!(!parsed("background-image-repeat = true\nbackground-image-repeat =")
            .background_image_repeat);
    }

    #[test]
    fn background_image_opacity_allows_values_above_one() {
        // Ghostty documents >1 as meaningful (the image ends up more opaque than
        // the background color), so this must NOT clamp to 1.0 the way
        // `background-opacity` does.
        assert_eq!(
            parsed("background-image-opacity = 1.5").background_image_opacity,
            1.5
        );
        assert_eq!(
            parsed("background-image-opacity = 0.25").background_image_opacity,
            0.25
        );
        // Negative is nonsense and clamps to zero (fully hidden image).
        assert_eq!(
            parsed("background-image-opacity = -1").background_image_opacity,
            0.0
        );
        assert_eq!(
            parsed("background-image-opacity = 2\nbackground-image-opacity =")
                .background_image_opacity,
            1.0
        );
    }

    #[test]
    fn path_values_resolve_relative_to_the_config_dir() {
        let dir = Path::new(r"C:\Users\me\AppData\Roaming\giest");
        assert_eq!(
            resolve_path("wall.png", Some(dir)),
            Some(dir.join("wall.png"))
        );
        assert_eq!(
            resolve_path(r"C:\pics\wall.png", Some(dir)),
            Some(PathBuf::from(r"C:\pics\wall.png"))
        );
        // Blank means unset, not "the config directory".
        assert_eq!(resolve_path("   ", Some(dir)), None);
        // With no config file at all, a relative path stays relative.
        assert_eq!(resolve_path("wall.png", None), Some(PathBuf::from("wall.png")));
    }

    #[test]
    fn font_family_and_feature_keys_parse() {
        let c = parsed(
            "font-family = Cascadia Code\n\
             font-family-bold = Cascadia Code SemiBold\n\
             font-family-italic = Cascadia Code Italic\n\
             font-feature = -calt\n\
             font-feature = ss01, cv01",
        );
        assert_eq!(c.font_family.as_deref(), Some("Cascadia Code"));
        assert_eq!(c.font_family_bold.as_deref(), Some("Cascadia Code SemiBold"));
        assert_eq!(c.font_family_italic.as_deref(), Some("Cascadia Code Italic"));
        assert_eq!(c.font_family_bold_italic, None);
        // Repeatable; each value is a comma-separated list.
        assert_eq!(c.font_features, vec!["-calt", "ss01", "cv01"]);
        // Comma-only split (not whitespace): a feature's own value syntax keeps
        // its spaces, so `liga off` is one feature the shaper resolves to liga=0.
        assert_eq!(parsed("font-feature = liga off").font_features, vec!["liga off"]);

        // Defaults and resets.
        assert_eq!(Config::default().font_family, None);
        assert!(Config::default().font_features.is_empty());
        assert_eq!(parsed("font-family = Foo\nfont-family =").font_family, None);
        // Only an empty value resets; `clear` is not special (stored as a literal
        // token the shaper later drops — matching Ghostty).
        assert!(parsed("font-feature = -calt\nfont-feature =").font_features.is_empty());
        assert_eq!(
            parsed("font-feature = -calt\nfont-feature = clear").font_features,
            vec!["-calt", "clear"]
        );
    }

    #[test]
    fn cursor_style_and_blink_parse() {
        let d = Config::default();
        assert_eq!(d.cursor_style, CursorShape::Block);
        assert_eq!(d.cursor_style_blink, None);

        assert_eq!(parsed("cursor-style = bar").cursor_style, CursorShape::Bar);
        assert_eq!(
            parsed("cursor-style = underline").cursor_style,
            CursorShape::Underline
        );
        // Garbage keeps the current; empty resets to default.
        assert_eq!(parsed("cursor-style = wat").cursor_style, CursorShape::Block);
        assert_eq!(parsed("cursor-style =").cursor_style, CursorShape::Block);

        assert_eq!(
            parsed("cursor-style-blink = true").cursor_style_blink,
            Some(true)
        );
        assert_eq!(
            parsed("cursor-style-blink = false").cursor_style_blink,
            Some(false)
        );
        assert_eq!(parsed("cursor-style-blink =").cursor_style_blink, None);
    }

    #[test]
    fn bold_color_and_bold_is_bright_parse() {
        assert_eq!(Config::default().bold_color, BoldColor::None);
        assert_eq!(parsed("bold-color = bright").bold_color, BoldColor::Bright);
        assert_eq!(
            parsed("bold-color = #ff8800").bold_color,
            BoldColor::Color(Rgb::new(0xff, 0x88, 0x00))
        );
        // Deprecated alias.
        assert_eq!(parsed("bold-is-bright = true").bold_color, BoldColor::Bright);
        assert_eq!(parsed("bold-is-bright = false").bold_color, BoldColor::None);
        // Empty resets; garbage keeps the current (default here).
        assert_eq!(parsed("bold-color =").bold_color, BoldColor::None);
        assert_eq!(parsed("bold-color = nonsense").bold_color, BoldColor::None);
    }

    #[test]
    fn minimum_contrast_parses_and_clamps() {
        assert_eq!(Config::default().min_contrast, 1.0);
        assert_eq!(parsed("minimum-contrast = 4.5").min_contrast, 4.5);
        // Clamped to [1.0, 21.0].
        assert_eq!(parsed("minimum-contrast = 0.2").min_contrast, 1.0);
        assert_eq!(parsed("minimum-contrast = 99").min_contrast, 21.0);
        assert_eq!(parsed("minimum-contrast =").min_contrast, 1.0);
    }

    #[test]
    fn theme_spec_selects_dark_variant() {
        // A bare name passes through; the dual `light:/dark:` form picks dark.
        assert_eq!(select_theme_variant("dracula"), "dracula");
        assert_eq!(select_theme_variant("light:rosewater,dark:mocha"), "mocha");
        assert_eq!(select_theme_variant("dark:nord"), "nord");
        assert_eq!(select_theme_variant("light:onlylight"), "onlylight");
        assert_eq!(select_theme_variant(""), "");
    }

    #[test]
    fn unresolved_theme_is_ignored_but_other_keys_apply() {
        // `theme` resolves out-of-band; an unknown theme is ignored and must not
        // discard the supported keys around it.
        let c = parsed("theme = does-not-exist\nfont-size = 21\nbackground = #010203");
        assert_eq!(c.font_points, 21.0);
        assert_eq!(c.bg, Rgb::new(1, 2, 3));
    }
}
