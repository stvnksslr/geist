//! Regressions for the libghostty-vt 1.0 C ABI port (geist-local).
//!
//! Each test drives a call whose shape changed in 1.0 behind a `void*` (so
//! the compiler could not catch a stale binding) or whose defaults changed.
//! They run against the real library, not a mock.

use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::render::{CellIterator, RowIterator};
use libghostty_vt::selection::FormatOptions;
use libghostty_vt::style::{Style, Underline};
use libghostty_vt::{RenderState, Terminal};

/// Every cell of the first row, as (graphemes, style), via the render state.
fn first_row(terminal: &Terminal<'_, '_>) -> Vec<(Vec<char>, Style)> {
    let mut state = RenderState::new().unwrap();
    let mut rows = RowIterator::new().unwrap();
    let mut cells = CellIterator::new().unwrap();
    let snapshot = state.update(terminal).unwrap();
    let mut row_iter = rows.update(&snapshot).unwrap();
    let row = row_iter.next().expect("a first row");
    let mut cell_iter = cells.update(row).unwrap();
    let mut out = Vec::new();
    while let Some(cell) = cell_iter.next() {
        out.push((cell.graphemes().unwrap(), cell.style().unwrap()));
    }
    out
}

/// `GRAPHEMES_BUF` takes a `GhosttyCodepointBuffer` in 1.0. Passing the old
/// bare `uint32_t*` made the library panic on alignment for every cell.
#[test]
fn graphemes_buf_reads_a_combining_cluster() {
    let mut terminal = Terminal::new(10, 2).unwrap();
    // "e" + COMBINING ACUTE ACCENT, then a plain "x".
    terminal.vt_write("e\u{301}x".as_bytes());
    let row = first_row(&terminal);
    assert_eq!(row[0].0, vec!['e', '\u{301}']);
    assert_eq!(row[1].0, vec!['x']);
    assert!(row[2].0.is_empty());
}

/// `GhosttyStyle` packs its attributes into one flags word in 1.0.
#[test]
fn style_flags_decode_from_the_engine() {
    let mut terminal = Terminal::new(10, 2).unwrap();
    // bold, italic, curly underline (4:3), strikethrough, then plain.
    terminal.vt_write(b"\x1b[1;3;4:3;9mA\x1b[0mB");
    let row = first_row(&terminal);
    let styled = row[0].1;
    assert!(styled.bold && styled.italic && styled.strikethrough);
    assert!(!styled.faint && !styled.inverse && !styled.overline);
    assert_eq!(styled.underline, Underline::Curly);
    let plain = row[1].1;
    assert!(!plain.bold && !plain.italic && !plain.strikethrough);
    assert_eq!(plain.underline, Underline::None);
}

/// Rust -> C -> Rust keeps every attribute and the underline style.
#[test]
fn style_round_trips_through_the_ffi_layout() {
    for underline in [
        Underline::None,
        Underline::Single,
        Underline::Double,
        Underline::Curly,
        Underline::Dotted,
        Underline::Dashed,
    ] {
        let mut style = Style::default();
        style.bold = true;
        style.blink = true;
        style.invisible = true;
        style.overline = true;
        style.underline = underline;
        let raw: libghostty_vt::ffi::Style = style.into();
        assert_eq!(raw.reserved, 0);
        assert_eq!(Style::try_from(raw).unwrap(), style);
    }
}

/// The selection one-shot is rebuilt on the formatter (1.0 removed
/// `ghostty_terminal_selection_format_*`). geist's copy path calls it with
/// unwrap + trim on the terminal's active selection.
#[test]
fn format_selection_alloc_copies_the_active_selection() {
    let mut terminal = Terminal::new(20, 3).unwrap();
    terminal.vt_write(b"hello   \r\nworld");

    assert!(
        terminal
            .format_selection_alloc(None, FormatOptions::new())
            .unwrap()
            .is_none(),
        "no active selection formats to None"
    );

    let all = terminal.select_all().unwrap().expect("a selection");
    terminal.set_selection(Some(&all)).unwrap();
    let trimmed = terminal
        .format_selection_alloc(None, FormatOptions::new().with_unwrap(true).with_trim(true))
        .unwrap()
        .expect("text");
    assert_eq!(&*trimmed, b"hello\nworld");
}

/// 1.0 reports empty `*_alloc` output as SUCCESS with a NULL pointer; that
/// used to surface as `Error::OutOfMemory`.
#[test]
fn empty_formatter_output_is_ok() {
    let terminal = Terminal::new(10, 2).unwrap();
    let mut formatter = Formatter::new(&terminal, FormatterOptions::new().with_trim(true)).unwrap();
    assert!(formatter.format_alloc(None).unwrap().is_empty());
    assert_eq!(formatter.format_len().unwrap(), 0);
}

/// A 1.0 formatter defaults to trim on and palette/style/hyperlink extras;
/// the binding sets every option explicitly so its builder defaults (all
/// off) still hold.
#[test]
fn formatter_options_default_to_off() {
    let mut terminal = Terminal::new(10, 2).unwrap();
    terminal.vt_write(b"ab  ");
    let mut plain = Formatter::new(&terminal, FormatterOptions::new()).unwrap();
    let untrimmed = plain.format_alloc(None).unwrap();
    assert!(untrimmed.starts_with(b"ab  "), "{:?}", &*untrimmed);

    let mut vt =
        Formatter::new(&terminal, FormatterOptions::new().with_format(Format::Vt)).unwrap();
    let out = vt.format_alloc(None).unwrap();
    assert!(
        !out.windows(4).any(|w| w == b"\x1b]4;"),
        "no palette extra unless asked: {:?}",
        String::from_utf8_lossy(&out)
    );
}

/// OSC 0/2 titles are a `GhosttyString` in 1.0 (were `const char**`).
#[test]
fn osc_window_title_is_read_as_a_string() {
    use libghostty_vt::osc::{CommandType, Parser};
    let mut parser = Parser::new().unwrap();
    for &byte in b"2;geist \xe2\x9c\x93" {
        parser.next_byte(byte);
    }
    match parser.end(0x07).command_type() {
        CommandType::ChangeWindowTitle { title } => assert_eq!(title, "geist \u{2713}"),
        other => panic!("unexpected {other:?}"),
    }
}
