# Ghostty parity plan (2026-09-26)

Built from the `ghostty-parity-audit` workflow run on **2026-09-26**. Every OPEN/PARTIAL row was
checked against `src/`. File:line references are from that date, so re-grep them before editing.
This plan does **not** edit GAP.md. Phase 0 lists the changes GAP.md needs.

Effort uses GAP.md's scale: **S** <1d · **M** 1–3d · **L** ~1wk · **XL** multi-week.
Status marks follow GAP.md: ✅ done · ◐ partial · ⬜ open.

## Context

| | |
|---|---|
| Upstream audited | `ghostty-org/ghostty` @ `b32f20f3e8d25bb925ec545c54498e93518e7ced` |
| geist's pinned engine | `b32f20f3e8d25bb925ec545c54498e93518e7ced`. This is the same commit, so a bump brings nothing new. The audit only re-checks correctness and coverage |
| Previous audit | GAP.md, re-audited 2026-09-19 |

**Scoreboard.** Strict definition: an item counts as done only when upstream's full value syntax and
params are accepted **and** the behaviour matches.

| Surface | Done | Total | Open/partial (planned below) | Blocked | Rest (N/A on Windows, or out by decision; derived) |
|---|---|---|---|---|---|
| config | **165** | 209 | 15 | 0 (1 soft) | 29 |
| actions | **72** | 88 | 13 | 0 | 3 |
| app | **40** | 77 | 27 | 0 | 10 |
| protocol | **26** | 55 | 16 | 3 | 10 |

GAP.md:29 reports **178/208 keys and 85/88 actions**. The gap comes from the definition: GAP.md counts
a key or action as done once its **name** is handled. Today these forms error or are silently ignored:
`new_split` (bare, whose default is `auto`), `increase_font_size:1`, `copy_to_clipboard:mixed`,
`close_tab:other`, `cursor-color = cell-foreground`, `window-padding-x = 2,4`, and
`command = wsl.exe -d Ubuntu`. Phase 0 has to pick one definition.

**Phase summary**

| Phase | Theme | Items | Audit rows | GAP.md still-open items folded in | Effort mix |
|---|---|---|---|---|---|
| 0 | GAP.md truth pass (docs only) | 31 stale + 22 over-claims | — | — | S |
| 1 | Wrong or unsafe today | 4 | 5 | 2365 (dup, adds OSC 5/13-19) | all S |
| 2 | Quit and multi-window scope | 6 | 4 | 2344, 1031, 2433 | S → M |
| 3 | Protocol: engine callbacks and host-side mode reports | 11 (incl. probe) | 12 | — | all S |
| 4 | Action parameter syntax | 6 | 11 | 566 (dup) | S, one M |
| 5 | Config value syntax and diagnostics | 6 | 8 | 122, 288; 2360 (dup) | S, one M |
| 6 | OS appearance (light/dark) and themes | 7 | 5 | 1465 (dup + rest) | S → M |
| 7 | Palette, tab strip and pane chrome | 11 | 11 | 126, 252 (dup) | S, one M |
| 8 | Persistence and quick terminal | 5 | 4 | 132; 1851, 1805 (dup) | all S |
| 9 | Windows shell surfaces and automation | 7 | 5 | 139, 45; 56 (dup) | S → M |
| 10 | Input and accessibility | 3 | 2 | 115; 119 (dup) | S → L |
| 11 | Fonts and text rendering | 4 | 1 | 1714, 750, 377 | M → L |
| 12 | Large items | 3 | 2 | 107; 38, 2329 (dup) | L |
| — | Blocked (3 hard + soft) | — | 3 + 1 soft | — | — |

Coverage: all **71** OPEN/PARTIAL rows are placed (70 in phases, plus `window-colorspace` as soft-blocked). All **25**
still-open GAP.md items are placed as well: 13 are new work and 12 duplicate an audit row, marked "dup" where they appear.

---

## Phase 0: GAP.md truth pass (docs only, S)

Do this first, in its own commit. Every later phase ends by flipping a GAP.md row, and that only
works if the rows are accurate. The audit found errors in both directions.

### 0a. Stale claims (31): GAP.md says open, the code says done

| GAP.md line(s) | Flip to | Evidence |
|---|---|---|
| 53, 54, 59, 60 (+424) | ✅ `grapheme-width-method`, `cursor-text`, `title-report`/`vt-kam-allowed`, `palette-generate`/`-harmonious` | config.rs:3429, 2259-2263, 3420/3423, 3183/3186, 1998-2020; ghostty_vt.rs:1287, 1290 |
| 93 (heading "17"), 101-104, 1980 | ✅ `reset_window_size`, `copy_url_to_clipboard`, `scroll_to_selection`, `end_key_sequence` | command.rs:789-792, 982-985; app.rs:4496-4515 |
| 253 | ✅ audible and attention bells. `bell-audio-volume` stays ◐ (→ 5.5) | bell.rs:66, 91, 95, 129 |
| 271 | ✅ `cursor-opacity` (line 314 already agrees) | config.rs:2424 |
| 309, 417 | ✅ `window-inherit-working-directory` | app.rs:1818, 4597 `should_inherit_cwd` |
| 324 | ✅ `isCovering` (U+2588 only) and custom shaders | render/mod.rs:419-424, 1503; shader.rs; tests/shader_gpu.rs |
| 373 | ✅ kitty graphics, OSC 9/777/99, OSC 9;4. Only the OSC 4/5/13-19 queries are left (→ 1.1) | osc_notify.rs, notify.rs, taskbar.rs |
| 377, 750, 1513 | ◐ Legacy computing: sextants, octants, eighths, smooth mosaics, triangles, corner diagonals and U+1FB98/99 fills are all drawn. Separated blocks and segmented digits (→ 11.2) and COLRv1 (→ 11.4) are left | sprite.rs:380-418, 563 |
| 389 | ✅ titlebar/decoration styles, About, runtime icons. Only the settings UI is left (upstream has none either) | winchrome.rs, about.rs, icon.rs, iconart.rs |
| 415, 592, 2079-2083 | ✅ OSC 133 C/D and `notify-on-command-finish` (+`-action`/`-after`). Line 2108 already agrees | osc133.rs:134-135; config.rs:3099 |
| 428 | ✅ notifications and taskbar progress. Only `bell-audio-volume` is left | osc_notify.rs, notify.rs, taskbar.rs |
| 432 | ✅ IPC and auto-update | ipc.rs, update.rs, cli.rs |
| 490, 1930 | ✅ keybind action coverage. Only `show_gtk_inspector` and the two debug/internal actions are left | command.rs `from_name`; app.rs:4448 |
| 491, 575 | ✅ fallback chains and synthetic bold/italic | config.rs:2234, 2311; render/atlas.rs:859-956 |
| 510, 1170 | ✅ named key tables, `catch_all`, `chain=`, `all:`, `performable:`, `unconsumed:`, `global:`. `global:` inside a table stays rejected by design | command.rs:204-205; keybind.rs:33, 65, 72-75; app.rs:4523 |
| 513 | ✅ cross-wrap matches, tracked highlights, regex | search.rs:15, 50, 162 |
| 524 | Tier 3: only COLRv1 (◐) and the settings UI are open. colorspace is ◐ | update.rs, about.rs, icon.rs, ipc.rs, winchrome.rs, colorspace.rs |
| 1132, 1974 | ✅ `all:` and `chain=` | keybind.rs:33, 65, 72-75; app.rs:4315 |
| 1425 | ✅ rectangle selection, the 60% threshold, autoscroll, `adjust_selection`, cross-wrap search | session.rs:1010; command.rs:55, 193; search.rs:50 |
| 1491 | ✅ box thickness now comes from the font's underline thickness. **Also** fix the stale `h/12` doc comment at sprite.rs:65-74 | render/atlas.rs:520, 1418-1424 |
| 1668 | ✅ double/triple-click drag snaps through the engine SelectionGesture | session.rs:1010; app.rs:7180-7186 |
| 1757 | ✅ `adjust-icon-height`, `font-variation*` | config.rs:2896, 2347; render/atlas.rs:525 |
| 1810 | ✅ quick-terminal slide animation and the `all:`/`unconsumed:`/`performable:` flags | config.rs:3189; keybind.rs |
| 1851 ("Not saved: window size/position") | ✅ state files carry `F x y w h max`. Cross-launch memory without state restore is still open (→ 8.1) | state.rs:17, 68, 218 |
| 2132 | ◐ WSL gets OSC 133 C/D from the vendored scripts. Custom shells, and native bash/zsh (→ 9.7), still get none | assets/shell-integration/*; profiles.rs:92-100 |

### 0b. Over-claims (22): GAP.md marks done or out of scope, the audit found partial or doable. Downgrade each and link the plan item

| GAP.md line | Says | Reality | → |
|---|---|---|---|
| 29 | `theme` counted as supported | No bundled theme pack. `light:`/`dark:` always picks dark | 6.3, 6.6 |
| 38 | `window-inherit-font-size` ✅ (with a divergence note) | `false` resets **every** window | 12.1 |
| 49 | `initial-command` resolves like `command` | No arguments, no `direct:`/`shell:` | 5.4 |
| 56 | "Only a `global:` keybind brings a window back" | An IPC relaunch also wakes it (app.rs:9827). The tray is still ⬜ | 9.3 |
| 98 | `new_split` covers left/up | `auto` (upstream's default) and a bare `new_split` are rejected | 4.2 |
| 109 | `crash` is "Debug-only, N/A" | It is a cross-platform debug action, and `main`/`io` are doable | 4.5 |
| 123 | "Reset Terminal" menu ✅ | Writes `ESC c` into the PTY **input**, so the terminal is never reset | 1.2 |
| 133 | IPC ✅ | No split, close by id, send key/mouse or get-details | 9.6 |
| 140 | Update: only "no release-notes popover" | Also no Skip/Later/Cancel/Retry, and the pill is invisible without a tab bar | 7.5 |
| 219 | "RIS clears progress", "2048 size-on-enable" and "XTGETTCAP" all free with the bump | Not in geist: progress comes from a side-scanner, `on_size` is not installed, `TN` is unanswered | 1.2, 3.3, 3.8 |
| 243, 486 | `theme` `light:`/`dark:` ✅ | Always picks dark | 6.3 |
| 342-347 | OSC 10/11/12 ✅, "only `?` needed building" | Answered **twice** once a theme is applied, and the report format is ignored | 1.1 |
| 392 | `window-theme` done | `system` follows the background colour, not the OS | 6.2 |
| 1306 | `write_*_file` done | The `,vt`/`,html` forms are rejected | 4.6 |
| 1805 | "Per-monitor enumeration unavailable" | `MonitorFromPoint` + `GetMonitorInfoW` provide it | 8.3 |
| 1818 | State restore ✅ | Per-pane profile, tab colour, title override and fullscreen are not saved | 8.2 |
| 1851 | Position/size covered by `window-*` keys | Nothing remembers the last frame across launches | 8.1 |
| 1920-1922 | "No remember: the deferred reply is too late" | The read replay (clipboard.rs:188) could carry `remember` | 9.5 |
| 1934 | `set_font_size` wired | Rounds to whole points | 4.1 |
| 1935-1936 | `quit` / `close_all_windows` wired | Closes the invoking window only, with no confirm | 2.1 |
| 2245-2246 | DA1 answered | DA1 omits `52` and DA2 is lib-vt's default | 3.4 |
| 2329-2333 | Kitty `U=1` placeholders "out of scope". `t=f`/`t=t` "hardcoded /tmp" | Both rationales are stale: the data is exported and upstream validates Windows paths | 12.2, 3.10 |

### 0c. Also in the truth pass

- **Scoreboard:** reconcile GAP.md:29 with the strict table above, or record both definitions side by side.
- **Add rows that GAP.md lacks** (the audit found these "absent"): `alpha-blending`; the `cell-*` forms of
  `selection-*`/`cursor-color`; `command` args; padding pairs; the params on `copy_to_clipboard`,
  `increase_`/`decrease_font_size`, `open_config` and `close_tab`; palette close; text drag-and-drop;
  palette focus/update rows; inspector export; the untrusted-URL policy; the quick-terminal frame cache;
  chrome following OSC 11; OS appearance; pointer shape; shortcut-label sync; inline rename; the
  reset-zoom badge; tab hints; merge-all; the debug banner; OSC 22, 1004, 1007, 2026, 996/2031, 2033,
  XTWINOPS, XTVERSION.
- **CLAUDE.md:** the `osc_color.rs` bullet ("only `?` is dropped upstream") is stale. Upstream now
  answers queries itself. Fix it with 1.1.

---

## Phase 1: wrong or unsafe today (all S)

These bugs hit users today. Each fits in a file or two and needs no new infrastructure.

| # | Item (audit rows) | Fix | Files / reuse |
|---|---|---|---|
| 1.1 | **OSC 10/11/12 `?` is answered twice**, and `osc-color-report-format` is not honoured. OSC 4 ignores the format too. OSC 5/13-19 go unanswered (protocol: OSC 10/11/12, OSC 4; GAP 2365 dup) | **Verify first.** This was found by reading the source, not by running it. Make the tripwire `engine_osc_color_query_replies` (ghostty_vt.rs:3080) call `apply_theme` (ghostty_vt.rs:1158) first, as session.rs:369 does, and assert **exactly one** reply. Then make geist the single answerer. Strip the engine's OSC 4/10/11/12 replies in `write` using the `strip_osc72` pattern (ghostty_vt.rs:47, called at :1057). Extend `parse_color_queries` (osc_color.rs:157) to `4;n;?` from the engine palette and to the 5/13-19 slots geist has values for, keeping `color_report` (osc_color.rs:200) as the one formatter. Update CLAUDE.md's `osc_color.rs` bullet | engine/ghostty_vt.rs, osc_color.rs, session.rs:559-576 |
| 1.2 | **`reset_terminal` sends Alt+C to the shell.** RIS doesn't clear the scanner-driven taskbar progress (protocol: RIS) | Add `TerminalEngine::full_reset()` (engine/mod.rs) → vendored `Terminal::reset()` (terminal.rs:339). `Session::reset` (session.rs:1284) calls it instead of `pty_write(b"\x1bc")`, and clears selection, search and progress. Re-apply the theme and VT policy after the reset, and pin that with a test. Teach `OscNotifyScanner` (osc_notify.rs:106) that `ESC c` means progress-remove (it also resets the pointer shape once 3.6 lands) | session.rs, engine/mod.rs, engine/ghostty_vt.rs, osc_notify.rs, taskbar.rs; menu entry at app.rs:4854 |
| 1.3 | **OSC 8 targets are launched unchecked.** A printed `file:///…exe`, `.lnk`, `.url` or `ms-*` link runs on Ctrl+click (app: untrusted URL) | New pure `src/untrusted_url.rs`: `decide(url, LinkSource) -> Allow / Confirm / Deny`, plus a display string with bidi and zero-width characters stripped. Port UntrustedURL.swift: allow http(s)/mailto, confirm custom schemes, deny executables, unsafe local files and UNC paths. Gate the OSC 8 source from Ctrl+click (app.rs:7257) before `open_url` (app.rs:10498). geist's own `write_*_file` temp paths stay trusted. The confirm dialog goes into **both** gates: `App::modal_open` (app.rs:3775) and the `palette_open` local (app.rs:6595) | new untrusted_url.rs, links.rs:17 `LinkSource`, app.rs |
| 1.4 | **Palette close is hardcoded to Ctrl+Shift+P.** A rebound toggle can't close it, and an unbound one still does (actions: `toggle_command_palette`) | Resolve the close against the keymap, the way `search_overlay_actions` (app.rs:5206) and the tab overview (app.rs:2853) do. Delete the `consume_key` block in `render_palette` (app.rs:4997-5008) | app.rs |
| 1.5 | **No `COLORTERM`/`TERM_PROGRAM`/`TERM_PROGRAM_VERSION`** in the child env, so many TUIs fail truecolor detection. Found by the first audit run, missed by the second, and checked by hand: nothing in `src/` sets either variable (app: child env) | Layer them in `Session::new` beside `term` (session.rs:272-276). An explicit `env =` still wins. `COLORTERM=truecolor`, `TERM_PROGRAM=geist`, version from about.rs. Add `GHOSTTY_QUICK_TERMINAL=1` for the quick terminal. Forward them through `WSLENV` in the WSL launch (profiles.rs:92-101) | session.rs, profiles.rs, about.rs |

## Phase 2: quit and multi-window scope

2.1 is the value item. 2.5 is the enabler for the per-window Win32 features (blur, attention, taskbar).

| # | Item (audit rows) | Fix | Files / reuse | Effort |
|---|---|---|---|---|
| 2.1 | **`quit` closes only the invoking window, with no confirm.** `close_all_windows` maps to the same action (actions: `quit`; app: quit flow) | Split `quit` and `close_all_windows` in `from_name` (command.rs:1017). Add `AppRequest::Quit`, handled at App level next to `CloseWindow` (app.rs:9022) instead of `retire` (app.rs:10000). Aggregate needs-confirm across windows behind `PendingClose` (app.rs:1827): one window uses the existing dialog, N windows get "N windows have running processes", with an option to review them (focus each in turn). Then retire every window, bypassing dormant mode and `quit-after-last-window-closed`. Keep the `closing` latch, and remember that `CancelClose` is honoured for ROOT only | app.rs, command.rs | M |
| 2.2 | `window-step-resize` applies to the root window only (config) | Drop the `self.is_root` filter (app.rs:7677-7697). Install `winchrome::set_step_resize` (winchrome.rs:402) on every HWND from winchrome's existing `EnumThreadWindows` walk. The cell size is app-global, so one geometry fits all windows | app.rs, winchrome.rs | S |
| 2.3 | Merge All Windows (app) | A palette entry and action that moves every tab into the focused window, reusing tear-out's cross-window tab move (app.rs:6430) and addressing windows **by id** | app.rs, command.rs | S |
| 2.4 | No system menu on right-click in the client-drawn caption (GAP 2433) | In the winchrome subclass, right-click on empty `HTCAPTION` → `GetSystemMenu` + `TrackPopupMenu` → post `WM_SYSCOMMAND`. Keep the caption buttons non-client so snap layouts keep working | winchrome.rs, app.rs | S |
| 2.5 | **Enabler: per-window HWND.** Secondary windows get no blur, `FlashWindowEx`, taskbar overlay or per-button progress, and pane drag-drop resolves windows source-first instead of by z-order (GAP 2344) | Map `Window::id` → HWND. Correlate through winchrome's `EnumThreadWindows` walk: children have no raw handle, see CLAUDE.md. Then feed `blur::hwnd_of` (blur.rs:63; root only at app.rs:2107), attention (app.rs:2252), the overlay (app.rs:8910) and progress (app.rs:8936, which always targets `windows[0]`) per window | app.rs, winchrome.rs, blur.rs, bell.rs, taskbar.rs | M |
| 2.6 | `all:` stops at the current window. Structural actions run once. `global:` doesn't imply `all:` (GAP 1031) | Thread a target `(window, tab, pane)` **id** through `execute_action`, so `execute_action_all` (app.rs:4315) can loop every window and `broadcasts_to_panes` (command.rs:171) can widen. `run_app_action` (app.rs:8739) broadcasts global surface actions | app.rs, command.rs | M |

## Phase 3: protocol, engine callbacks and host-side mode reports (all S)

Most rows are one callback in `GhosttyVtEngine::new` (next to `on_enquiry`, ghostty_vt.rs:268) or one
`term.mode()` read in the session. **3.0 comes first**: CLAUDE.md says to confirm the bytes arrive.

| # | Item (audit rows) | Fix | Files / reuse |
|---|---|---|---|
| 3.0 | **Probe** (enables this phase and 6.4) | Add cases to `tests/conpty_passthrough.rs` for `CSI c`, `CSI >c`, `CSI >q`, `DCS +q`, `CSI 14/16/18 t`, `?1004h`, `?1007h`, `?2026h`, `?2031h` + `CSI ?996n`, `?2033h`, `?2048h` and OSC 22. Run it on the inbox ConPTY and with `geist_TEST_PASSTHROUGH=1`. A stripped sequence moves to Blocked; don't debug it as a geist bug | tests/conpty_passthrough.rs |
| 3.1 | **Mode 1007 alternate scroll**: the wheel does nothing in `less`/`man` without mouse mode | Add `TerminalEngine::mode(Mode)` and `is_alt_screen()` (vendored `Terminal::mode` terminal.rs:433, `active_screen` :860; the `term.mode()` pattern at ghostty_vt.rs:1060). In the wheel path (session.rs:2038-2051): no tracking + alt screen + `ALT_SCROLL` (default on) → N× `encode_key(Up/Down)`, which respects DECCKM | session.rs, engine/mod.rs, engine/ghostty_vt.rs |
| 3.2 | **Mode 1004 focus reports.** ConPTY turns 1004 on in its own handshake (GAP.md:2243), so console apps never see focus changes | Add `TerminalEngine::encode_focus(bool)` via vendored `focus::Event::encode` (focus.rs:43). The session tracks the last focus it sent. The app feeds `window focused && pane focused` each pass. Send only while `Mode::FOCUS_EVENT` is set | engine/*, session.rs, app.rs |
| 3.3 | 2048 sends no report on enable. XTWINOPS `CSI 14/16/18 t` go unanswered (2 rows) | Install `on_size` (terminal.rs:2163), returning the cols, rows and cell px already passed at resize (session.rs:930) | engine/ghostty_vt.rs |
| 3.4 | DA1 omits `52`, so neovim doesn't see OSC 52. DA2 is lib-vt's default | `on_device_attributes` (terminal.rs:2203): DA1 `?62;22;52c` when `clipboard-write != deny`, DA2 `>1;10;0c`. Keep the policy in a shared cell like `enquiry_src` (ghostty_vt.rs:267) so a reload updates it | engine/ghostty_vt.rs, clipboard.rs |
| 3.5 | XTVERSION answers `libghostty` | `on_xtversion` (terminal.rs:2124) → `geist <version>` (about.rs) | engine/ghostty_vt.rs, about.rs |
| 3.6 | **Pointer shape**: no I-beam over the grid, no hand over links, OSC 22 ignored (protocol: OSC 22; app: pointer) | I-beam and hand need no probe: set `CursorIcon::Text` over the grid and `PointingHand` when `link_at` hits (session.rs `hover_cell` :140), leaving chrome cursors alone (app.rs:5588, 6844, 7722, 7845). OSC 22 needs a new `src/osc22.rs` side-scanner in `OscColorScanner`'s shape (osc_color.rs:66), with its `scan` test helper, mapping W3C names → `egui::CursorIcon`. It is stored per Session and reset by RIS (1.2) | new osc22.rs, session.rs, app.rs |
| 3.7 | Mode 2026 synchronized output ignored | Hold the previous snapshot while `Mode::SYNC_OUTPUT` is set, with a 1 s safety reset (upstream Thread.zig:38). The probe matters here: OpenConsole may already buffer it | session.rs, engine/ghostty_vt.rs, app.rs (snapshot site) |
| 3.8 | XTGETTCAP `TN` unanswered | Add `Terminal::set_terminfo_name` to the vendored safe binding (`GHOSTTY_TERMINAL_OPT_TERMINFO_NAME` = 37). This is a **new geist-local vendored delta**, so list it in CLAUDE.md. The value comes from config `term` (config.rs:3335) | vendor/libghostty-rs/…/terminal.rs, engine/ghostty_vt.rs, session.rs |
| 3.9 | Mode 2033: no unsolicited visibility reports | While `Mode::VISIBILITY_REPORT` is set (terminal.rs:1243), write `CSI ?999;1n` or `;2n` on minimize, occlusion, or the tab going inactive. The `?998n` query stays "visible" (soft-blocked, below) | session.rs, app.rs |
| 3.10 | Kitty `t=f`/`t=t` refused on a stale `/tmp` rationale | At ghostty_vt.rs:1269-1270, set `set_kitty_image_from_file_allowed(true)` and `set_kitty_image_temp_file_dir(Some(GetTempPathW()))`. Keep `t=s` off and fix the comment. WSL clients send Linux paths that won't resolve, so document that. Enabling it is a security decision (Human-only) | engine/ghostty_vt.rs |

## Phase 4: action parameter syntax

| # | Item (audit rows) | Fix | Files / reuse | Effort |
|---|---|---|---|---|
| 4.1 | `increase_font_size:N` / `decrease_font_size:N` are rejected (upstream has no bare form). `set_font_size` rounds to whole points (3 rows) | Parse an f32 param in `from_name` (command.rs:994-995, 895-903). Keep bare = 1 as a geist compatibility alias. Carry it through `font_zoom_by` (app.rs:4364) and `set_font_points` (app.rs:3072) instead of the hardcoded ±1 (app.rs:4811-4812) | command.rs, app.rs | S |
| 4.2 | `new_split:auto` and bare `new_split` are rejected | Add an `auto` split resolved at split time by a pure `auto_dir(rect)` (split along the larger dimension). Make it the default. It sits next to `Action::SplitRight/Down/…` (command.rs:777, 930-939; executed at app.rs:4793-4796) | command.rs, app.rs | S |
| 4.3 | `close_tab:this|other|right` rejected | Map the forms to the existing `CloseTab` / `close_other_tabs` / `CloseTabsToRight` (command.rs:973-975). `Action::name` (command.rs:770) round-trips the upstream form | command.rs | S |
| 4.4 | `open_config:os_open` rejected | Parse the param in `from_name` (command.rs:1007). The `new_window` variant is GTK-only | command.rs | S |
| 4.5 | `crash:main|io|render` missing | `main` panics on the UI thread. `io` panics in the PTY reader thread (pty.rs). `render` panics in `prepare`. Shipping it in release builds is a decision (Human-only) | command.rs, app.rs, pty.rs | S |
| 4.6 | `copy_to_clipboard:plain|vt|html|mixed` and `write_{scrollback,screen,selection}_file:…,plain|vt|html` rejected (4 rows; GAP 566 dup) | Selection: add a format to `selected_text` (ghostty_vt.rs:~1740) through the vendored `FormatOptions::with_emit_format` (selection.rs:577). Screen/scrollback: `ghostty_formatter_terminal_new` with `fmt::Format::{Vt,Html}` (fmt.rs:267-269). `mixed` = arboard HTML plus plain alternative. Parse `,fmt` in `WriteAction` (writefile.rs:28) and pick the extension in `file_name` (writefile.rs:92). Whether bare copy defaults to `mixed` as upstream does is a decision | engine/mod.rs, engine/ghostty_vt.rs, session.rs:1898, command.rs:997, 1039-1048, writefile.rs, app.rs:4614, 4814 | M |

## Phase 5: config value syntax and diagnostics

5.0 comes first because every setter touched after it should report bad values.

| # | Item (audit rows) | Fix | Files / reuse | Effort |
|---|---|---|---|---|
| 5.0 | **Enabler: setters that swallow bad values** (GAP 122). About 19 `diag!` sites cover 185 setters | Make `parse_bool` (config.rs:3457) and the `font-size` (2222-2229) and `grapheme-width-method` (3429-3434) setters push through `diag!` (config.rs:42). Validate `font-feature` in its setter so errors reach `Config::diagnostics` (config.rs:1769) rather than the `eprintln` at render/atlas.rs:600-605. Extend `diagnostics_collect_unknown_keys_bad_values_and_malformed_lines` (config.rs:6345) | config.rs, render/atlas.rs | M (incremental) |
| 5.1 | `font-feature = cv01 = 2` (spaces around `=`) rejected (GAP 288) | Normalise the spaces before `Feature::from_str` in `parse_features` (render/atlas.rs:597). Extend `parse_features_keeps_valid_drops_invalid` (atlas.rs:2159) | render/atlas.rs | S |
| 5.2 | `selection-foreground`/`-background` and `cursor-color` reject `cell-foreground`/`cell-background`. `selection-background` has a fixed default instead of inversion (3 rows) | Switch the setters (config.rs:2613, 2610, 2359) from `opt_color`/`color` to the existing `terminal_color`/`TerminalColor` (config.rs:451, 464). Unset `selection-background` → invert each cell's fg/bg (render/mod.rs:1517, 1669). Resolve cursor `cell-*` per cell the way `cursor-text` already does (render/mod.rs:1497), and pass `None` to `set_cursor_color` (session.rs:370) for it | config.rs, app.rs (TermFrame), render/mod.rs, session.rs | S |
| 5.3 | `window-padding-x/-y = a,b` fails to parse (2 rows) | `padding()` (config.rs:3702) returns a pair, stored as left/right and top/bottom. Update `padding_rects` (padding.rs:87) and every `2.0 * padding_x` consumer (app.rs:5430, 6549, 7651, 7656). Check the interplay with `window-padding-balance` (test config.rs:4966) | config.rs, padding.rs, app.rs | S |
| 5.4 | `command`/`initial-command` take no args and no `direct:`/`shell:` prefix (2 rows) | A pure, table-tested `parse_command(s) -> (prefix, program, args)` using Windows argv quoting. `shell:` runs through the default profile's shell. Wire it into `Profile::new` (profiles.rs:576-584) and `for_command` (profiles.rs:603). The `-e argv` path (cli.rs:216) already carries args. Update CLAUDE.md's "`command` names a program, not a command line" and the `.cmd` workaround in the capture guide | profiles.rs, config.rs:2731, 2803, pty.rs:119, app.rs:2076-2081 | S |
| 5.5 | `bell-audio-volume` parsed but never read (config; GAP 2360 dup) | A pure `scale_wav(bytes, vol)` (16-bit PCM, table-tested), played with `PlaySoundW(SND_MEMORY|SND_ASYNC)` from a buffer kept alive in a static. Hook it into `bell::play_audio` (bell.rs:95), called from `ring_bell` (app.rs:2234). `waveOutSetVolume` is the fallback | bell.rs, app.rs | S |

## Phase 6: OS appearance (light/dark) and themes

6.1 is the enabler. It unlocks 6.2-6.4 and 6.7.

| # | Item (audit rows) | Fix | Files / reuse | Effort |
|---|---|---|---|---|
| 6.1 | **Enabler: OS appearance source** | New `src/appearance.rs`: read `HKCU\…\Themes\Personalize\AppsUseLightTheme`, and watch `WM_SETTINGCHANGE("ImmersiveColorSet")` in the existing root-window subclass (restart.rs), then wake ROOT. The parse function is pure and table-tested | new appearance.rs, restart.rs | S |
| 6.2 | `window-theme = system` follows the background colour, not the OS. `auto` with a light/dark `theme` should act as `system` (config) | `system` → appearance. `auto` → appearance when `theme` has `light:`/`dark:`, otherwise the current `prefers_dark` path (config.rs:2503; theme.rs:79, 170) | config.rs, theme.rs | S |
| 6.3 | `theme = light:X,dark:Y` always picks dark (config: `theme`, part; app: follow OS appearance; GAP 1465 dup) | `select_theme_variant` (config.rs:3590) takes the appearance. On a change, re-run the reload path so every session's engine gets `apply_theme` (ghostty_vt.rs:1158). Update the test at config.rs:6327-6331 | config.rs, app.rs, engine/ghostty_vt.rs | S |
| 6.4 | `CSI ?996n` unanswered. No DEC 2031 unsolicited reports (protocol) | `on_color_scheme` (terminal.rs:2183) answers from 6.1, falling back to `theme::prefers_dark` (theme.rs:79). On a change and on RIS, write `ghostty_color_scheme_report_encode` output (bindings.rs:2905) to sessions that have 2031 set (uses 3.1's `mode()`). Run 3.0's probe first | engine/ghostty_vt.rs, session.rs, theme.rs | S |
| 6.5 | Window chrome ignores live OSC 11 (app) | Feed `theme::chrome` (theme.rs:168) the focused top pane's live default background instead of the config's. `sync_titlebar_colors` (app.rs:9628) then pushes it through `winchrome::apply_titlebar_colors` (winchrome.rs:336). Remove the "not live OSC 11" note at app.rs:9632-9636 | theme.rs, app.rs | S |
| 6.6 | No bundled theme pack, so a bare `theme = Name` fails (config: `theme`, rest) | Ship the iTerm2-Color-Schemes `ghostty/` set as `assets/themes`, and install it beside the exe in `scripts/package.ps1` and the MSIX. `resolve_theme_path` (config.rs:3625) searches `<config>/themes` first, then `<exe>/themes`. Licence and size are a decision | config.rs, assets/themes (new), scripts/package.ps1, packaging/AppxManifest.xml.in | M |
| 6.7 | Conditional configuration beyond `theme` (GAP 1465, rest) | Generalise 6.3 into upstream's condition set, re-evaluated on appearance change, reusing the `apply_theme_spec` (config.rs:2155) reload path | config.rs, app.rs | M |

## Phase 7: palette, tab strip and pane chrome

7.1 comes first because 7.2 reuses its reverse lookup.

| # | Item (audit rows) | Fix | Files / reuse | Effort |
|---|---|---|---|---|
| 7.1 | **Enabler: shortcut labels are a hardcoded default table.** Palette rows, tooltips and menus are wrong after a rebind or `keybind = clear` (app: menu shortcut sync) | Add `Keymap::first_chord_for(&Action) -> Option<String>` in keybind.rs (next to `lookup`, :115; format with `key_name`, :788). It replaces the `&'static str` `Action::keybind()` (command.rs:639) in palette rows (command.rs:1151; drawn at app.rs:5116) and the tooltips at app.rs:6209, 6285, 5311 | keybind.rs, command.rs, app.rs, menu.rs | S |
| 7.2 | Tab shortcut hints (`goto_tab:N`) | Draw 7.1's chord on each tab label (app.rs:5942-5951) | app.rs | S |
| 7.3 | Palette "Focus: <terminal>" entries | App-level `build_catalog` (app.rs:4267) walks every window, tab and pane (with tab colour and a pwd subtitle), producing a focus action addressed **by id** that reuses `answer_ipc`'s focus path (app.rs:9785) | app.rs, command.rs:1197 | S |
| 7.4 | Palette update entries | `build_catalog` adds "Update and Restart" / "Cancel or Skip" from `update::global()` state (update.rs:781, `State` :716) | app.rs, command.rs, update.rs | S |
| 7.5 | Update popover: Skip / Later / Cancel / Retry; an overlay when there is no tab bar | Add skip, cancel and retry states to `update::State` (update.rs:716). Cancel goes through the `Http` trait and is mock-tested. Open the popover from the pill (app.rs:5877), and add a corner overlay when `window-show-tab-bar = never` or in the quick terminal. Persist the skipped version in state.rs's line format | update.rs, app.rs, state.rs | S |
| 7.6 | Double-click a tab to rename it | `.double_clicked()` on the tab (app.rs:6154) → `self.renaming` (inline box at app.rs:5955). Clear it at both mutation points (the stale-index rule) | app.rs | S |
| 7.7 | Reset-zoom button on a zoomed tab | A badge on the tab while a split is zoomed, which runs the existing toggle-split-zoom action | app.rs | S |
| 7.8 | Per-pane progress bar (app; GAP 126 dup) | Draw a bar at the top of each pane in `render_active` from `Session::progress` (session.rs:1380) with `indicators::progress_span` (indicators.rs:101). Keep the tab-strip bar | app.rs, indicators.rs | S |
| 7.9 | Debug-build warning banner | A `cfg!(debug_assertions)` strip above the panes | app.rs | S |
| 7.10 | Inspector: export the Terminal IO log | An Export button on `inspector::Log` (inspector.rs:140), rendering with `render_bytes` (inspector.rs:237). Write with `writefile::write` (writefile.rs:98) to the temp dir, then open or copy the path | inspector.rs, app.rs, writefile.rs | S |
| 7.11 | Link hover highlight: underline OSC 8 links on hover, and regex links on hover+mods (app; GAP 252 dup) | Resolve the hovered link's cell span from `hover_cell` (session.rs:140) and `link_at`, carry it in `TermFrame`, and have the renderer underline it with the cell's fg | session.rs, links.rs, app.rs, render/mod.rs | M |

## Phase 8: persistence and quick terminal (all S)

| # | Item (audit rows) | Fix | Files / reuse |
|---|---|---|---|
| 8.1 | Last window position/size across launches (app) | Save the frame on move/resize (debounced) in state.rs's line format, reusing `WindowFrame` (state.rs:68) and `parse_frame` (:218). Apply it to the first window (main.rs:350-352, app.rs:3371) unless `window-position-*`/`-width`/`-height` are set. Centre otherwise | state.rs, main.rs, app.rs |
| 8.2 | State restore completeness: per-pane profile, tab colour, title override, fullscreen (app; GAP 1851 dup) | Add backward-compatible fields to `SavedNode::Leaf` (state.rs:42) and `SavedTab` (:60), and check that `parse` (:240) skips unknown records. Restore the pane through the profile index. A WSL pane must not come back as pwsh | state.rs, app.rs, session.rs |
| 8.3 | `quick-terminal-screen = mouse` (config; GAP 1805 dup) | Store the value (the setter at config.rs:2926 only warns today). `quickterm::work_area` (quickterm.rs:188) takes a point: `MonitorFromPoint(GetCursorPos)` + `GetMonitorInfoW().rcWork`. `macos-menu-bar` stays mapped to `main` | config.rs, quickterm.rs, app.rs:3404, 3455 |
| 8.4 | Quick terminal: per-screen frame cache and split-tree restore across relaunch (app) | A pure cache in quickterm.rs keyed by monitor, recorded on hide. `quick_builder` (app.rs:3391) uses it before `quickterm::frame` (quickterm.rs:159). Add a quick-terminal record to state.rs. Depends on 8.3 | quickterm.rs, app.rs, state.rs |
| 8.5 | `+new-window --title` not forwarded to the running instance (GAP 132) | Add `title` to `Request::NewWindow` (ipc.rs:66, 699) and to the plan built at cli.rs:340, 389. Apply it in `answer_ipc` | cli.rs, ipc.rs, app.rs |

## Phase 9: Windows shell surfaces and automation

Anything COM/OLE follows the `jumplist.rs`/`taskbar.rs` pattern of hand-declared vtables, including
the ignored host test.

| # | Item (audit rows) | Fix | Files / reuse | Effort |
|---|---|---|---|---|
| 9.1 | Open a script with geist: confirm, run, then wait. No "Open with geist" for .ps1/.bat/.cmd (app) | A script opened via cli.rs:294-295 shows a confirm modal (both gates), then a new tab runs it through 5.4's argv with `wait-after-command`. Register the association in `shellreg.rs` (HKCU, explicit CLI only). Depends on 5.4 | cli.rs, app.rs, shellreg.rs | S |
| 9.2 | Packaging leftovers: no LICENSE, no Cargo.toml `license`, no winget manifest (GAP 139) | Add an MIT LICENSE (decided 2026-09-26: match Ghostty) and `license = "MIT"` to Cargo.toml. `scripts/package.ps1` already copies LICENSE when present. Emit a winget manifest from `geist-manifest.json` data | LICENSE, Cargo.toml, scripts/package.ps1, packaging/ | S |
| 9.3 | Resident mode has no tray icon (app; GAP 56 dup) | Make notify.rs's notification-area icon persistent while dormant, with New Window and Quit. Reuse `go_dormant` (app.rs:9483) and `wake_with` (app.rs:9511). Quit goes through 2.1 | notify.rs, app.rs | M |
| 9.4 | Text/URL drag-and-drop (CF_UNICODETEXT) refused (app) | Revoke winit's drop target and `RegisterDragDrop` geist's own `IDropTarget` on each top-level HWND (CF_HDROP + CF_UNICODETEXT). Text goes through **`Session::paste_str`** (session.rs:1220, the paste gate). Files go to the existing `dropfiles::quote_paths` path (dropfiles.rs:113; app.rs:7613) | new dragdrop.rs, dropfiles.rs, app.rs, main.rs | M |
| 9.5 | Clipboard prompt: show the requesting program's name and "Remember for this session" (app) | Expose `name` and `can_remember` from the vendored callbacks (terminal.rs:2245, 2293 hardcode `remember: false`), which is a **new vendored delta**. Carry `remember` through `clipboard::replay_request` (clipboard.rs:188) for reads, and add the same replay for writes | vendor/libghostty-rs/…/terminal.rs, engine/ghostty_vt.rs, clipboard.rs, app.rs | M |
| 9.6 | IPC parity with App Intents/AppleScript (app) | New `Request`s (ipc.rs:65): split{dir}, close{window/tab/pane id}, send_key (`encode_key`), send_mouse and scroll, pane-targeted run_action, get-details (text, pid, tty), and env / initial input for new terminals. `input_text` stays a paste through `paste_str` | ipc.rs, app.rs `answer_ipc`, session.rs, cli.rs | M |
| 9.7 | Native Windows bash/zsh get no shell-integration injection (GAP 45) | Extend `Profile::launch_args` (profiles.rs:87-100) to Git Bash/MSYS bash and zsh, using the vendored scripts (`extract_shell_integration`, profiles.rs:527). Ghostty's bash script is already tested via Git Bash | profiles.rs, assets/shell-integration/ | M |

## Phase 10: input and accessibility

| # | Item (audit rows) | Fix | Files / reuse | Effort |
|---|---|---|---|---|
| 10.1 | A long IME preedit runs past the pane edge (GAP 115) | Replace `layout_no_wrap` (app.rs:8101) with a layout wrapped at the pane's right edge | app.rs, ime.rs | S |
| 10.2 | IME preedit caret and segment styling (app: IME) | Carry winit's preedit cursor range through the vendored egui-winit. This is a **second egui-winit delta**: record it in CLAUDE.md and re-apply it on every egui bump. Draw it in `ime::Preedit` (ime.rs:20) | vendor/egui-winit, ime.rs, app.rs:8092-8097 | M |
| 10.3 | Accessibility depth: soft-wrap-aware rows, rectangle-selection fidelity, palette active descendant (app: a11y; GAP 119 dup) | `a11y::extract` (a11y.rs:129) joins soft-wrapped rows using the wrap flag `screen_text` already reports (test ghostty_vt.rs:3686). `build_nodes` (a11y.rs:375) keeps rectangle selections, and the palette text box gets an active descendant | a11y.rs, app.rs | L |

## Phase 11: fonts and text rendering (human visual confirmation required)

| # | Item (audit rows) | Fix | Files / reuse | Effort |
|---|---|---|---|---|
| 11.1 | Synthetic bold/italic never reaches fallback faces, so CJK bold renders regular (GAP 1714) | Key the monochrome fallback cache by `(char, style)` (render/atlas.rs:1110-1111, 1480-1501), and apply `Synth` from `resolve_slots` (atlas.rs:859) as the primary slots do (atlas.rs:1670) | render/atlas.rs | M |
| 11.2 | Legacy-computing separated blocks and segmented digits (U+1FBF0–1FBF9) come from the font (GAP 750) | Add the ranges to `sprite::covers` (sprite.rs:371) and the draw dispatch (sprite.rs:399-418), porting upstream's sprite code. Extend the sprite tests (sprite.rs:~2014) | sprite.rs | M |
| 11.3 | `alpha-blending` (native / linear / linear-corrected) (config) | New key. Branch the glyph shader's coverage path and blend state (render/mod.rs:2222-2225). Define its precedence over geist-only `text-gamma` (config.rs:2516), and set the custom-shader input colour space (shader.rs). The default is a decision, since it changes text weight for everyone | config.rs, render/mod.rs, shader.rs | M |
| 11.4 | COLRv1: gradients flattened to their mean stop colour; transforms, clips and composite modes ignored (GAP 377; Segoe UI Emoji is COLRv1) | Replace `average_stops` (render/atlas.rs:216) with real linear/radial/sweep gradient fills, and implement `push_transform`, `push_clip` and `push_layer` (atlas.rs:202-259) | render/atlas.rs | L |

## Phase 12: large items (L)

| # | Item (audit rows) | Fix | Files / reuse |
|---|---|---|---|
| 12.1 | `window-inherit-font-size = false` resets every window. Mixed-DPI monitors can't both be right (config; GAP 38 dup) | Key the atlases by size and DPI inside the single `GpuResources`, with `TermFrame` carrying the window's key. This **overturns** CLAUDE.md's "font size is app-global" rule, so update it | render/mod.rs, render/atlas.rs, app.rs:9681 |
| 12.2 | Kitty `U=1` unicode placeholders (protocol; GAP 2329 dup) | Port `graphics_unicode.zig`: read U+10EEEE cells (row flag `KITTY_VIRTUAL_PLACEHOLDER`, diacritics → row/col, fg → image id, underline colour → placement id), and stop skipping virtual placements (ghostty_vt.rs:618-620). Keep `walk_placements` a free function taking `&Terminal` (CLAUDE.md). Needs the sideloaded ConPTY | engine/ghostty_vt.rs, engine/mod.rs, render/mod.rs |
| 12.3 | Tab overview shows text thumbnails, not rendered or live previews (GAP 107) | Render each tab offscreen in `prepare`, using the same offscreen-pass pattern as custom shaders, which must live in `prepare` | app.rs, render/mod.rs |

---

## Blocked (and what would unblock each)

| Item | Blocked on | Unblocks when |
|---|---|---|
| OSC 72 kitty drag-and-drop (protocol) | No C export for drop/move/leave (upstream `c/terminal.zig:669` sets `.drag_and_drop = null`). The macOS app ignores `.kitty_dnd` at this ref too, so geist's `strip_osc72` (ghostty_vt.rs:47) matches it | lib-vt exports drag-and-drop event entry points **and** the upstream app implements them. Until then, keep stripping the replies |
| Kitty graphics autoplay (`a=a`, `s=2|3`) (protocol) | `animationTick` is called only from upstream's renderer (generic.zig:1440). `kitty_graphics.h` exports no tick, frame count or per-frame gap (IMAGE_DATA stops at GENERATION = 9) | lib-vt exports an animation-tick call plus frame metadata, either upstream or as a Zig patch in the build `libghostty-vt-sys/build.rs` fetches |
| Glyph protocol, APC 25a1 (protocol) | Only the on/off option (`OPT_GLYPH_PROTOCOL`, terminal.h:1327-1335). No glossary or outline getter, and the renderer half isn't in the pin | Upstream ships both a renderer consumer and a C read-back API. Until then keep it disabled (test `glyph_protocol_is_not_advertised`, ghostty_vt.rs:2433) |
| *Soft:* `window-colorspace` real wide gamut (config, counted ◐; the sRGB-clipped P3 conversion already ships) | Needs an FP16/scRGB swapchain + `IDXGISwapChain3::SetColorSpace1`. wgpu-hal 29 DX12 never calls it, and egui-wgpu picks the surface format | wgpu exposes a surface colour space on DX12, or geist accepts vendoring wgpu-hal + egui-wgpu (a new render-stack delta, **L**) |
| *Soft:* an exact `?998n` answer (part of 3.9) | No C setter for `Terminal.flags.visible`, so the engine always answers "visible" | lib-vt exports a visibility option. Unsolicited reports (3.9) don't need it |
| *Conditional:* anything 3.0 shows ConPTY strips (e.g. 2026, OSC 22) | ConPTY re-renders its own stream | Only under the sideloaded OpenConsole, or not at all. Record the probe result in GAP.md |

## Human-only

- **Visual confirmation.** Per CLAUDE.md, never declare these done from a screenshot: 3.6 pointer
  shapes, 3.7 no tearing under 2026, 5.2 inverted selection and `cell-*` cursor, 5.3 asymmetric
  padding, 6.5 live chrome colour, 7.2/7.7/7.8/7.9 tab and pane chrome, 7.11 hover underline, all
  of Phase 11, 10.1/10.2 preedit, 12.1-12.3.
- **Decisions:**
  - the untrusted-URL allow/confirm/deny lists (1.3)
  - `quit` vs `close_all_windows` semantics and undo (2.1)
  - whether `crash` ships in release builds (4.5)
  - bare `copy_to_clipboard` → `mixed`, which changes what Ctrl+C puts on the clipboard (4.6)
  - what `shell:` means on Windows (5.4)
  - the `TN` value when `term` is empty (3.8)
  - enabling kitty file media, which adds a local-file-read surface (3.10)
  - theme-pack licence, size and packaging (6.6)
  - the `alpha-blending` default (11.3)
  - the geist LICENSE, and whether to submit to winget (9.2)
- **Hardware or environment a harness can't fake:**
  - two monitors, including mixed DPI (8.3, 8.4, 2.5, 12.1)
  - toggling the OS light/dark setting live (Phase 6)
  - a real CJK IME (10.x)
  - Narrator/NVDA (10.3)
  - hearing the bell volume (5.5)
  - dragging text from a browser (9.4)
  - snap layouts and the system menu on the drawn caption (2.2, 2.4)
  - Explorer "Open with" (9.1)
  - a signed release manifest for the update popover (7.5)
  - the tray while resident (9.3)

## Verification (per item, following CLAUDE.md)

1. **Run cargo from the project root through mise**, never inside `vendor/`:
   `mise exec -- cargo test`, `mise exec -- cargo clippy --all-targets -- -D warnings` and
   `mise exec -- cargo fmt --all -- --check`. These are the same gates as `.pre-commit-config.yaml` and CI.
   Single test: `cargo test <name>`.
2. **Every item lands with a tripwire test** named for the behaviour. Examples:
   `osc11_query_gets_exactly_one_reply` (1.1, with a theme applied, replacing
   `engine_osc_color_query_replies`), `reset_action_resets_the_engine_not_the_shell`,
   `ris_clears_progress`, `wheel_on_alt_screen_sends_arrows`, `new_split_bare_is_auto`,
   `selection_background_unset_inverts`, `padding_pair_parses`. Keep the logic pure and table-tested
   where you can: `untrusted_url::decide`, `scale_wav`, `parse_command`, `auto_dir`, the
   appearance parse, the quick-terminal cache.
3. **Run `cargo build` before any manual or screenshot check.** `cargo test` leaves
   `target\debug\geist.exe` stale.
4. **Protocol work (Phase 3, 6.4, 12.2): run the probe first.**
   `cargo test --test conpty_passthrough -- --ignored`, then again with `geist_TEST_PASSTHROUGH=1`.
   Add the new sequence's case *before* implementing. A stripped sequence goes to Blocked, not into a bug hunt.
5. **render/\*, padding, font or compositing changes** (5.2, 5.3, 7.8, 7.11, Phase 11, 12.x): say
   plainly that they need human visual confirmation, and describe what should look different. If you
   capture anyway: make the probe per-monitor DPI aware, use `PrintWindow(…, 3)`, and solve for the
   expected pixel values rather than eyeballing them.
6. **UI invariants.** Every new modal (1.3, 2.1, 9.1) goes into **both** `App::modal_open` and the
   `palette_open` local, and resolves its own keybinds against the keymap. Every new text-input path
   (9.4, 9.6) goes through `Session::paste_str`. Anything held across frames uses ids, not indices.
7. **Live tests stay isolated.** Set `$geist_IPC_PIPE` and `jump-list = false`, use a temp
   `LOCALAPPDATA` for update work (7.5), keep restore guards around registry writes (9.1), and clean
   up with `+unregister-shell-integration`.
8. **Vendored deltas** (3.8 terminfo setter, 9.5 clipboard remember, 10.2 egui-winit preedit range):
   add each one to CLAUDE.md's vendored list, then build **and launch**. A link-mode or ABI slip
   only shows at runtime.
9. **After each phase**, flip the GAP.md rows and the scoreboard in their own commit, then re-run
   `.claude/workflows/ghostty-parity-audit.js` to confirm the counts moved.

## Next five (after Phase 0)

1. **1.1**: verify, then fix, the OSC 10/11/12 double reply. The second reply leaks into shell input in every themed session.
2. **1.2**: `reset_terminal` sends Alt+C to the shell instead of resetting. RIS leaves taskbar progress stuck.
3. **1.3**: untrusted OSC 8 link policy. Today any program can make a Ctrl+click launch an arbitrary target.
4. **3.0 → 3.1**: run the ConPTY probe, then add mode 1007 alternate scroll so the wheel works in `less`, `man` and `git log`.
5. **2.1**: make `quit` quit the app, with one confirm across all windows.
