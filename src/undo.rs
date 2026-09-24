//! Undo / redo — Ghostty's `undo` and `redo` actions and its `undo-timeout`.
//!
//! Ghostty's macOS app leans on `NSUndoManager` (subclassed as
//! `ExpiringUndoManager`) to make structural terminal operations reversible:
//! close a split, a tab, a window — or *create* one — and the action can be
//! taken back for a few seconds. This module is the platform-independent half
//! of that: a two-stack manager whose entries **expire**, generic over the
//! operation payload so it can be tested without spawning a shell. `app.rs`
//! supplies the payload (`app::UndoOp`), which owns the live [`crate::session::Session`]s
//! a restore puts back.
//!
//! Two properties are worth stating outright, because they are what make undo
//! useful rather than merely present:
//!
//! - **A restore is lossless.** Undoing a close puts the *same running shell*
//!   back, not a fresh one — the removed subtree is moved into the undo entry
//!   and moved back out. That is also what Ghostty does (its `UndoState` holds
//!   the live `SurfaceView` tree), and it is the reason the entries have to
//!   expire: an entry holds processes alive.
//! - **The inverse is recorded by performing, not by construction.** A caller
//!   performs an op and records the op that reverses it; the manager decides
//!   which stack that lands on from the phase it is in. So one code path serves
//!   undo, redo, and the original user action, and the three cannot disagree
//!   about what "the opposite" is.
//!
//! Time is passed in as a `f64` of seconds — egui's `input.time`, the same
//! clock the rest of the app pumps on — rather than read from `Instant`, so the
//! expiry rules are unit-testable.

/// Which stack a newly recorded operation belongs on.
///
/// Mirrors `NSUndoManager`'s `isUndoing`/`isRedoing`: an inverse recorded while
/// undoing is a *redo*, one recorded while redoing is an *undo* again, and one
/// recorded at rest is a fresh undo that invalidates the redo stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Undoing,
    Redoing,
}

struct Entry<T> {
    op: T,
    /// When it was recorded, on egui's clock. Each entry expires on its own
    /// schedule — upstream: "This timeout applies per operation… New operations
    /// do not reset the timeout of previous operations."
    at: f64,
}

/// A hard cap on either stack.
///
/// Upstream has none: it relies purely on the timeout, and its documentation
/// warns that a very long `undo-timeout` grows the stack (and so the process
/// count) without bound. geist keeps the warning *and* the cap — an entry holds
/// a live shell, so an unbounded stack is unbounded processes. The oldest entry
/// is dropped, which is the same end the timeout would have reached.
const MAX_ENTRIES: usize = 64;

/// The undo/redo stacks for one process.
pub struct UndoStack<T> {
    undo: Vec<Entry<T>>,
    redo: Vec<Entry<T>>,
    /// `undo-timeout` in seconds. **Zero disables undo entirely** — upstream
    /// says so explicitly, and it is why [`Self::record`] drops rather than
    /// stores.
    timeout: f64,
    phase: Phase,
}

impl<T> UndoStack<T> {
    /// A stack with `undo-timeout` given in milliseconds (the unit
    /// [`crate::config`] parses Ghostty's duration grammar into).
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            timeout: timeout_ms as f64 / 1000.0,
            phase: Phase::Idle,
        }
    }

    /// Apply a new `undo-timeout` (a config reload).
    ///
    /// Existing entries keep the deadline they were recorded with — the timeout
    /// is compared against each entry's own age, so shortening it retires the
    /// old ones at the next [`Self::expire`] rather than at their original
    /// deadline. That is the reading that makes `undo-timeout = 0` mean "off"
    /// immediately, which is the one a user reloading the config wants.
    pub fn set_timeout(&mut self, timeout_ms: u64) {
        self.timeout = timeout_ms as f64 / 1000.0;
    }

    /// Whether undo is switched off entirely (`undo-timeout = 0`).
    pub fn disabled(&self) -> bool {
        self.timeout <= 0.0
    }

    /// Record `op` as the operation that reverses what the caller just did.
    ///
    /// At rest this is a fresh undo entry and the redo stack is dropped —
    /// standard undo-manager semantics: once you diverge, the future you had
    /// undone is gone. While undoing or redoing it lands on the opposite stack
    /// instead, and the other one is left alone.
    pub fn record(&mut self, now: f64, op: T) {
        if self.disabled() {
            return;
        }
        let entry = Entry { op, at: now };
        let stack = match self.phase {
            Phase::Idle => {
                self.redo.clear();
                &mut self.undo
            }
            Phase::Undoing => &mut self.redo,
            Phase::Redoing => &mut self.undo,
        };
        stack.push(entry);
        if stack.len() > MAX_ENTRIES {
            stack.remove(0);
        }
    }

    /// Drop entries older than the timeout. Called every pass, not just when
    /// undo is used: an expired entry is holding a shell open, and the process
    /// should end when the entry does, not whenever someone next presses a key.
    pub fn expire(&mut self, now: f64) {
        let timeout = self.timeout;
        // The stacks are in recording order, so this is a prefix — but a
        // `retain` is honest about that without depending on a monotonic clock.
        self.undo.retain(|e| now - e.at < timeout);
        self.redo.retain(|e| now - e.at < timeout);
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Take the newest undoable operation and enter the undoing phase, so the
    /// inverse the caller records lands on the redo stack.
    ///
    /// The caller **must** call [`Self::end`] once it has performed the op —
    /// including when performing it turns out to be impossible and nothing is
    /// recorded, or the next ordinary action would be filed as a redo.
    pub fn begin_undo(&mut self, now: f64) -> Option<T> {
        self.expire(now);
        let op = self.undo.pop()?.op;
        self.phase = Phase::Undoing;
        Some(op)
    }

    /// The mirror of [`Self::begin_undo`].
    pub fn begin_redo(&mut self, now: f64) -> Option<T> {
        self.expire(now);
        let op = self.redo.pop()?.op;
        self.phase = Phase::Redoing;
        Some(op)
    }

    /// Leave the undoing/redoing phase.
    pub fn end(&mut self) {
        self.phase = Phase::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stack over a payload that is just a label, so the ordering rules can be
    /// read off the assertions.
    fn stack() -> UndoStack<&'static str> {
        UndoStack::new(5_000)
    }

    #[test]
    fn record_then_undo_returns_the_newest_first() {
        let mut s = stack();
        s.record(0.0, "a");
        s.record(0.0, "b");
        assert_eq!(s.begin_undo(0.0), Some("b"));
        s.end();
        assert_eq!(s.begin_undo(0.0), Some("a"));
        s.end();
        assert_eq!(s.begin_undo(0.0), None);
    }

    #[test]
    fn the_inverse_recorded_while_undoing_becomes_the_redo() {
        let mut s = stack();
        s.record(0.0, "close");
        let op = s.begin_undo(0.0).unwrap();
        assert_eq!(op, "close");
        // Performing "close"'s inverse records the op that would re-close it.
        s.record(0.0, "reclose");
        s.end();
        assert!(!s.can_undo());
        assert!(s.can_redo());
        assert_eq!(s.begin_redo(0.0), Some("reclose"));
        // …and redoing records *its* inverse back onto the undo stack.
        s.record(0.0, "restore");
        s.end();
        assert!(s.can_undo());
        assert!(!s.can_redo());
    }

    #[test]
    fn a_fresh_action_clears_the_redo_stack() {
        let mut s = stack();
        s.record(0.0, "a");
        s.begin_undo(0.0);
        s.record(0.0, "a-inverse");
        s.end();
        assert!(s.can_redo());
        // A new user action at rest: the undone future is no longer reachable.
        s.record(0.0, "b");
        assert!(!s.can_redo());
        assert!(s.can_undo());
    }

    #[test]
    fn entries_expire_per_operation_not_as_a_batch() {
        let mut s = UndoStack::new(5_000);
        s.record(0.0, "old");
        s.record(4.0, "new");
        // At t=6 the first is 6s old and gone; the second is only 2s old.
        s.expire(6.0);
        assert_eq!(s.begin_undo(6.0), Some("new"));
        s.end();
        assert!(!s.can_undo());
    }

    #[test]
    fn expiry_also_prunes_the_redo_stack() {
        let mut s = UndoStack::new(5_000);
        s.record(0.0, "a");
        s.begin_undo(0.0);
        s.record(0.0, "a-inverse");
        s.end();
        assert!(s.can_redo());
        s.expire(9.0);
        assert!(!s.can_redo());
    }

    #[test]
    fn a_zero_timeout_disables_recording_entirely() {
        let mut s = UndoStack::new(0);
        assert!(s.disabled());
        s.record(0.0, "a");
        assert!(!s.can_undo());
    }

    #[test]
    fn reloading_a_zero_timeout_retires_what_was_already_recorded() {
        let mut s = UndoStack::new(5_000);
        s.record(0.0, "a");
        s.set_timeout(0);
        s.expire(0.0);
        assert!(!s.can_undo());
    }

    #[test]
    fn the_stack_is_capped_by_dropping_the_oldest() {
        let mut s = UndoStack::new(60_000);
        for _ in 0..MAX_ENTRIES {
            s.record(0.0, "filler");
        }
        s.record(0.0, "newest");
        // One over the cap: the count holds and the newest survived.
        let mut n = 0;
        while let Some(op) = s.begin_undo(0.0) {
            if n == 0 {
                assert_eq!(op, "newest");
            }
            s.end();
            n += 1;
        }
        assert_eq!(n, MAX_ENTRIES);
    }
}
