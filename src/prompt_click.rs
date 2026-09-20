//! `cursor-click-to-move`: turn a click on the prompt into cursor movement.
//!
//! A port of `Surface.zig`'s `maybePromptClick` and `Screen.zig`'s
//! `promptClickMove`/`promptClickLine`. The shell opts in on its `OSC 133;A`
//! mark with one of two options (the engine applies the mark but exposes
//! neither, so `osc133.rs` side-scans them):
//!
//! - `click_events=1|2` (Kitty): send the click itself as an SGR left-press,
//!   with the row absolute (`1`) or relative to the prompt (`2`). Takes
//!   priority over `cl`.
//! - `cl=line|m|v|w`: synthesize left/right arrow presses. Every `cl` variant
//!   uses upstream's line-based walk today, so this does too.
//!
//! Everything here is pure (rows in, bytes/counts out) so the walk is testable
//! without a shell; the viewport rows come from
//! [`crate::engine::TerminalEngine::prompt_rows`].

use crate::engine::{PromptRowInfo, RowPrompt};

/// How the shell asked for prompt clicks to be handled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClickMode {
    /// No `cl`/`click_events` seen: prompt clicks are ignored.
    #[default]
    None,
    /// `click_events`: `relative` is `click_events=2`.
    ClickEvents { relative: bool },
    /// `cl=…`: arrow keys.
    Arrows,
}

/// The mode an `OSC 133;A` option string selects, or `None` when it names
/// neither option (which leaves the previous mode alone, as upstream only
/// assigns when an option is present). `opts` is the `;`-separated tail.
pub fn mode_from_options(opts: &str) -> Option<ClickMode> {
    let get = |key: &str| {
        opts.split(';')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    };
    match get("click_events") {
        Some("1") => return Some(ClickMode::ClickEvents { relative: false }),
        Some("2") => return Some(ClickMode::ClickEvents { relative: true }),
        _ => {}
    }
    match get("cl") {
        Some("line" | "m" | "v" | "w") => Some(ClickMode::Arrows),
        _ => None,
    }
}

/// The row the cursor's prompt starts on: the nearest [`RowPrompt::Prompt`]
/// at or above the cursor (upstream's `promptIterator(.left_up)`).
pub fn prompt_row(rows: &[PromptRowInfo], cursor_y: u16) -> Option<u16> {
    (0..=cursor_y).rev().find(|&y| {
        rows.get(y as usize)
            .is_some_and(|r| r.prompt == RowPrompt::Prompt)
    })
}

/// Arrow presses (`left`, `right`) that move the cursor to `click`.
/// `promptClickLine`, row for row: only input cells count, soft-wrapped rows
/// are one line, and a click past the end moves one beyond the last input
/// cell when the cursor already sits on input.
pub fn line_move(rows: &[PromptRowInfo], cursor: (u16, u16), click: (u16, u16)) -> (usize, usize) {
    let (cx, cy) = (cursor.0 as usize, cursor.1 as usize);
    let (kx, ky) = (click.0 as usize, click.1 as usize);
    if (cx, cy) == (kx, ky) {
        return (0, 0);
    }
    let cursor_on_input = rows
        .get(cy)
        .and_then(|r| r.input.get(cx))
        .copied()
        .unwrap_or(false);
    if (cy, cx) < (ky, kx) {
        // Right: walk forward from just after the cursor.
        let mut count = 0;
        for y in cy..=ky {
            let Some(row) = rows.get(y) else { break };
            if y != cy && row.prompt != RowPrompt::Continuation && !row.wrap_continuation {
                // Upstream checks `semantic_prompt == prompt_continuation`; a
                // soft-wrapped input row carries its prompt's marking, which the
                // row API reports as a wrap continuation, so accept either.
                break;
            }
            let start = if y == cy {
                cx + 1
            } else {
                row.input.iter().position(|&i| i).unwrap_or(row.input.len())
            };
            for x in start..row.input.len() {
                if !row.input[x] {
                    continue;
                }
                count += 1;
                if (y, x) == (ky, kx) {
                    return (0, count);
                }
            }
            if !row.wrap {
                if cursor_on_input {
                    count += 1;
                }
                break;
            }
        }
        return (0, count);
    }
    // Left: walk backward from just before the cursor.
    let mut count = 0;
    for y in (ky..=cy).rev() {
        let Some(row) = rows.get(y) else { break };
        let end = if y == cy {
            cx.min(row.input.len())
        } else {
            row.input.len()
        };
        for x in (0..end).rev() {
            if !row.input[x] {
                continue;
            }
            count += 1;
            if (y, x) == (ky, kx) {
                return (count, 0);
            }
        }
        if !row.wrap_continuation {
            break;
        }
    }
    (count, 0)
}

/// The SGR left-press `click_events` sends: 1-based column, and the row either
/// absolute or relative to the prompt's first row.
pub fn click_event(click: (u16, u16), prompt_y: u16, relative: bool) -> Vec<u8> {
    let y = if relative {
        click.1.saturating_sub(prompt_y) as u32 + 1
    } else {
        click.1 as u32 + 1
    };
    format!("\x1b[<0;{};{}M", click.0 as u32 + 1, y).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row from a picture: `p` prompt text, `i` input, `.` blank.
    fn row(s: &str, prompt: RowPrompt, wrap: bool, cont: bool) -> PromptRowInfo {
        PromptRowInfo {
            input: s.chars().map(|c| c == 'i').collect(),
            wrap,
            wrap_continuation: cont,
            prompt,
        }
    }

    #[test]
    fn options_select_the_mode_with_click_events_first() {
        assert_eq!(mode_from_options("cl=line"), Some(ClickMode::Arrows));
        assert_eq!(mode_from_options("aid=1;cl=m"), Some(ClickMode::Arrows));
        assert_eq!(
            mode_from_options("cl=line;click_events=2"),
            Some(ClickMode::ClickEvents { relative: true })
        );
        assert_eq!(
            mode_from_options("click_events=1"),
            Some(ClickMode::ClickEvents { relative: false })
        );
        // click_events=0 is not an opt-in; fall back to cl.
        assert_eq!(
            mode_from_options("click_events=0;cl=line"),
            Some(ClickMode::Arrows)
        );
        assert_eq!(mode_from_options("aid=3"), None);
        assert_eq!(mode_from_options("cl=bogus"), None);
    }

    #[test]
    fn same_line_moves_count_input_cells_only() {
        // Prompt `ppp`, input at cols 4..=8, blanks after.
        let rows = vec![row("ppp.iiiii.....", RowPrompt::Prompt, false, false)];
        assert_eq!(line_move(&rows, (9, 0), (5, 0)), (4, 0));
        assert_eq!(line_move(&rows, (4, 0), (7, 0)), (0, 3));
        assert_eq!(line_move(&rows, (6, 0), (6, 0)), (0, 0));
        // Clicking past the end from on-input moves one beyond the last cell.
        assert_eq!(line_move(&rows, (4, 0), (12, 0)), (0, 5));
        // Clicking on the prompt text itself moves to the start of input.
        assert_eq!(line_move(&rows, (8, 0), (1, 0)), (4, 0));
    }

    #[test]
    fn soft_wrapped_input_is_one_line() {
        let rows = vec![
            row("pp.iiiii", RowPrompt::Prompt, true, false),
            row("iiii....", RowPrompt::None, false, true),
        ];
        // Cursor at row 1 col 2, click row 0 col 4: 2 cells on row 1, then 4 on row 0.
        assert_eq!(line_move(&rows, (2, 1), (4, 0)), (6, 0));
        assert_eq!(line_move(&rows, (4, 0), (1, 1)), (0, 5));
    }

    #[test]
    fn a_hard_line_break_stops_the_walk() {
        let rows = vec![
            row("pp.iii..", RowPrompt::Prompt, false, false),
            row("iiii....", RowPrompt::None, false, false),
        ];
        // The second row isn't a continuation of the prompt: stop after row 0,
        // plus the step past the end (cursor was on input).
        assert_eq!(line_move(&rows, (3, 0), (2, 1)), (0, 3));
    }

    #[test]
    fn prompt_row_and_click_event_encoding() {
        let rows = vec![
            row("out.....", RowPrompt::None, false, false),
            row("pp.iii..", RowPrompt::Prompt, false, false),
            row("pp.i....", RowPrompt::Continuation, false, false),
        ];
        assert_eq!(prompt_row(&rows, 2), Some(1));
        assert_eq!(prompt_row(&rows, 0), None);
        assert_eq!(click_event((4, 2), 1, false), b"\x1b[<0;5;3M");
        assert_eq!(click_event((4, 2), 1, true), b"\x1b[<0;5;2M");
    }
}
