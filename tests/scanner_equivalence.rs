//! The side scanners `Session::pump_pty` runs over every PTY chunk skip ahead
//! with SIMD searches (to the next ESC, `ESC ]`, `ESC [ >`, or a CSI's final
//! byte) instead of stepping a state machine through each byte. Those skips are
//! only sound if they land in exactly the state the byte-by-byte walk would
//! have — including when a sequence is split across reads, which is where a
//! skip that peeks ahead goes wrong.
//!
//! So: build seeded random streams from a vocabulary of sequence fragments
//! (complete, partial, malformed and nested), feed each one whole, one byte at
//! a time, and in random splits, and require every scanner to report the same
//! outputs and end in the same observable state. Feeding one byte at a time
//! never gives a skip anything to skip, so it is the reference walk.

use geist::decscusr::DecscusrScanner;
use geist::osc_color::OscColorScanner;
use geist::osc_notify::OscNotifyScanner;
use geist::osc133::Osc133Scanner;
use geist::synthetic::Lcg;
use geist::xtshiftescape::ShiftEscapeScanner;

const FRAGMENTS: &[&[u8]] = &[
    b"hello ",
    b"q h l s c ] [ > ? 12 ; ",
    b"\x1b",
    b"\x1b[",
    b"\x1b]",
    b"\x1b\\",
    b"\x07",
    b"\x1b\x1b",
    b"\x1bc",
    b"\x1b[c",
    b"\x1b[>",
    b"\x1b[>1s",
    b"\x1b[>0s",
    b"\x1b[>s",
    b"\x1b[1>s",
    b"\x1b[>>1s",
    b"\x1b[?12h",
    b"\x1b[?12l",
    b"\x1b[?25;12h",
    b"\x1b[12;?l",
    b"\x1b[5 q",
    b"\x1b[0 q",
    b"\x1b[ q",
    b"\x1b[2  q",
    b"\x1b[38;5;123m",
    b"\x1b[0m",
    b"\x1b[12;1H",
    b"\x1b]11;?\x07",
    b"\x1b]10;?\x1b\\",
    b"\x1b]12;?\x1bx",
    b"\x1b]133;A\x07",
    b"\x1b]133;C\x07",
    b"\x1b]133;D;1\x1b\\",
    b"\x1b]9;hello\x07",
    b"\x1b]9;4;1;50\x07",
    b"\x1b]777;notify;title;body\x07",
    b"\x1b]0;title",
    b"]11;?\x07",
    b"133;D;0\x07",
    b">1s",
    b" q",
    b"?12h",
    b"\xe4\xb8\xad\xe6\x96\x87",
];

#[derive(Debug, PartialEq)]
struct Seen {
    colors: Vec<String>,
    marks: Vec<String>,
    notes: Vec<String>,
    // (is_default, default_blink) after every feed of the *whole* stream, and
    // the final shift-escape capture.
    cursor: (bool, bool),
    shift: Option<bool>,
}

fn run(stream: &[u8], splits: &[usize]) -> Seen {
    let mut color = OscColorScanner::new();
    let mut marks = Osc133Scanner::new();
    let mut notify = OscNotifyScanner::new();
    let mut cursor = DecscusrScanner::new();
    let mut shift = ShiftEscapeScanner::new();
    let (mut q, mut m, mut n) = (Vec::new(), Vec::new(), Vec::new());
    let mut at = 0;
    for &end in splits.iter().chain(std::iter::once(&stream.len())) {
        let chunk = &stream[at..end];
        color.feed(chunk, &mut q);
        marks.feed(chunk, &mut m);
        notify.feed(chunk, &mut n);
        cursor.feed(chunk);
        shift.feed(chunk);
        at = end;
    }
    Seen {
        colors: q.iter().map(|x| format!("{x:?}")).collect(),
        marks: m.iter().map(|x| format!("{x:?}")).collect(),
        notes: n.iter().map(|x| format!("{x:?}")).collect(),
        cursor: (cursor.is_default(), cursor.default_blink()),
        shift: shift.capture(),
    }
}

fn below(rng: &mut Lcg, n: usize) -> usize {
    rng.below(n as u32) as usize
}

#[test]
fn skipping_scanners_match_the_byte_by_byte_walk() {
    for seed in 0..400u64 {
        let mut rng = Lcg::new(seed);
        let mut stream = Vec::new();
        for _ in 0..(5 + below(&mut rng, 60)) {
            stream.extend_from_slice(FRAGMENTS[below(&mut rng, FRAGMENTS.len())]);
        }

        let whole = run(&stream, &[]);
        let bytewise: Vec<usize> = (1..stream.len()).collect();
        assert_eq!(
            run(&stream, &bytewise),
            whole,
            "seed {seed}: byte-wise vs whole, {stream:?}"
        );

        for _ in 0..8 {
            let mut cuts: Vec<usize> = (0..below(&mut rng, 6))
                .map(|_| below(&mut rng, stream.len() + 1))
                .collect();
            cuts.sort_unstable();
            assert_eq!(
                run(&stream, &cuts),
                whole,
                "seed {seed}: cuts {cuts:?}, {stream:?}"
            );
        }
    }
}
