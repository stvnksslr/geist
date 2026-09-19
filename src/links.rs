//! Link matching beyond the built-in URL detector: Ghostty's `link` table
//! (user regexes) and the `link-previews` policy for the hover banner.
//!
//! Priority is upstream's: an OSC 8 hyperlink first (when `link-osc8`), then
//! the configured `link` regexes in order, then the `link-url` matcher, which
//! Config.zig documents as "always lowest priority of any configured links".

use regex::Regex;

use crate::config::LinkPreviews;
use crate::engine::GridSnapshot;

/// Where a link under the pointer came from. Only [`LinkSource::Osc8`] matters
/// to `link-previews = osc8`, whose point is that an OSC 8 link's target can
/// differ from its visible text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkSource {
    Osc8,
    Rule,
    Url,
}

/// Whether the hover banner shows a link from `source`.
pub fn preview_allowed(policy: LinkPreviews, source: LinkSource) -> bool {
    match policy {
        LinkPreviews::All => true,
        LinkPreviews::Never => false,
        LinkPreviews::Osc8 => source == LinkSource::Osc8,
    }
}

/// The text of row `y`, with each column's starting byte offset. A cell with
/// no text (blank, or the tail of a wide character) contributes one space so
/// that a regex never matches *across* a gap on screen.
fn row_text(snap: &GridSnapshot, y: u16) -> (String, Vec<usize>) {
    let mut s = String::new();
    let mut starts = Vec::with_capacity(snap.cols as usize);
    for x in 0..snap.cols {
        starts.push(s.len());
        match snap.cell(x, y).map(|c| c.text.as_str()).filter(|t| !t.is_empty()) {
            Some(t) => s.push_str(t),
            None => s.push(' '),
        }
    }
    (s, starts)
}

/// The first `rules` match (in rule order) that covers column `x` of row `y`.
pub fn rule_match_at(snap: &GridSnapshot, x: u16, y: u16, rules: &[Regex]) -> Option<String> {
    if rules.is_empty() || x >= snap.cols || y >= snap.rows {
        return None;
    }
    let (text, starts) = row_text(snap, y);
    let at = starts[x as usize];
    rules.iter().find_map(|re| {
        re.find_iter(&text)
            .find(|m| m.start() <= at && at < m.end())
            .map(|m| m.as_str().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Cell;

    fn snap(rows: &[&str]) -> GridSnapshot {
        let cols = rows.iter().map(|r| r.chars().count()).max().unwrap_or(0) as u16;
        let mut s = GridSnapshot {
            cols,
            rows: rows.len() as u16,
            ..Default::default()
        };
        for r in rows {
            let mut n = 0;
            for ch in r.chars() {
                s.cells.push(Cell {
                    text: if ch == ' ' { "".into() } else { ch.to_string().into() },
                    ..Default::default()
                });
                n += 1;
            }
            for _ in n..cols {
                s.cells.push(Cell::default());
            }
        }
        s
    }

    #[test]
    fn a_rule_matches_only_under_the_pointer() {
        let s = snap(&["see JIRA-123 and JIRA-7 now"]);
        let rules = [Regex::new(r"JIRA-\d+").unwrap()];
        assert_eq!(rule_match_at(&s, 4, 0, &rules).as_deref(), Some("JIRA-123"));
        assert_eq!(rule_match_at(&s, 11, 0, &rules).as_deref(), Some("JIRA-123"));
        assert_eq!(rule_match_at(&s, 12, 0, &rules), None, "the space after it");
        assert_eq!(rule_match_at(&s, 17, 0, &rules).as_deref(), Some("JIRA-7"));
        assert_eq!(rule_match_at(&s, 0, 0, &rules), None);
    }

    #[test]
    fn earlier_rules_win() {
        let s = snap(&["path /tmp/abc.log"]);
        let rules = [Regex::new(r"/tmp/\S+").unwrap(), Regex::new(r"\S+\.log").unwrap()];
        assert_eq!(rule_match_at(&s, 8, 0, &rules).as_deref(), Some("/tmp/abc.log"));
        let rules = [Regex::new(r"abc\.log").unwrap(), Regex::new(r"/tmp/\S+").unwrap()];
        assert_eq!(rule_match_at(&s, 12, 0, &rules).as_deref(), Some("abc.log"));
        // Outside the first rule's match, the second still applies.
        assert_eq!(rule_match_at(&s, 6, 0, &rules).as_deref(), Some("/tmp/abc.log"));
    }

    #[test]
    fn blanks_break_a_match() {
        let s = snap(&["ab  cd"]);
        let rules = [Regex::new(r"[a-d]+").unwrap()];
        assert_eq!(rule_match_at(&s, 0, 0, &rules).as_deref(), Some("ab"));
        assert_eq!(rule_match_at(&s, 5, 0, &rules).as_deref(), Some("cd"));
        assert_eq!(rule_match_at(&s, 2, 0, &rules), None);
    }

    #[test]
    fn preview_policy() {
        use LinkSource::*;
        assert!(preview_allowed(LinkPreviews::All, Url));
        assert!(!preview_allowed(LinkPreviews::Never, Osc8));
        assert!(preview_allowed(LinkPreviews::Osc8, Osc8));
        assert!(!preview_allowed(LinkPreviews::Osc8, Rule));
        assert!(!preview_allowed(LinkPreviews::Osc8, Url));
    }
}
