//! Keeping placements on their rows while the screen's content moves.
//!
//! A placement is anchored to a grid line, and output moves the lines: every
//! line of text pushed into the history makes the placement's line one
//! smaller. Nothing tells the placement so, which is why it is *looked* for
//! instead (the way the block index follows its rows): before anything reads
//! or changes the placements of a screen, [`follow_main`] asks where the row
//! that stood at the screen's top at the last look is now, and moves every
//! placement by that much.
//!
//! The main screen's rows are told apart by the address of their cells, which
//! follows a row into the history. The alternate screen has no history to
//! look in: its rows are the ring's and an upward scroll rotates the ring, so
//! [`follow_alt`] reads the rotation off the row addresses ([`rotation`]) —
//! enough for the scrolls programs do, not for a scrolling region that leaves
//! the top row alone.
//!
//! Every place that changes the screen's rows *itself* (a clear, a resize)
//! moves the placements with it and then [`settle`]s, so that the next look
//! does not read its own work as a movement.
//!
//! **Known limit, the ring's:** once the scrollback is full a row's buffer is
//! reused as a new bottom row, so the search for the top row can meet it again
//! at the wrong depth after more than a whole history of output between two
//! looks. The row's text is compared to catch that, and when the shift cannot
//! be believed the placements go: no picture is better than one on the wrong
//! line.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::Term;

use super::store::Store;
use crate::block_index::row_text;
use crate::session::{probe_depth, row_identity};

/// The main screen as it was at a look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Seen {
    /// The identity of the row at the screen's top ([`row_identity`]).
    probe: usize,
    /// That row's text ([`row_text`]): the check that the probe is the row
    /// and not a reused buffer.
    text: u64,
    history: usize,
    columns: usize,
    lines: usize,
}

impl Seen {
    pub(super) fn of<T>(term: &Term<T>) -> Self {
        Self {
            probe: row_identity(term, Line(0)),
            text: row_text(term, Line(0)),
            history: term.history_size(),
            columns: term.columns(),
            lines: term.screen_lines(),
        }
    }
}

/// How many rows the content moved up between two looks of the main screen;
/// `None` when it cannot be told.
fn moved<T>(before: &Seen, now: &Seen, term: &Term<T>) -> Option<i32> {
    if before.columns != now.columns || before.lines != now.lines || now.history < before.history {
        return None;
    }
    let rows = if now.probe == before.probe {
        0
    } else {
        probe_depth(term, before.probe)?
    };
    // Output that is not at a full history's bottom pushes exactly as many
    // rows into the history as it moves them; fewer is not a movement.
    let grown = i32::try_from(now.history - before.history).ok()?;
    if rows < grown {
        return None;
    }
    // More than the history grew means rows fell off its top: the ring
    // reused buffers, so the probe must still be the row it was.
    if rows > grown && row_text(term, Line(-rows)) != before.text {
        return None;
    }
    Some(rows)
}

/// Brings the main screen's placements to the grid as it is now.
pub(super) fn follow_main<T>(store: &mut Store, term: &Term<T>, cell: (u16, u16)) {
    if store.placements.is_empty() {
        store.seen = None;
        return;
    }
    let now = Seen::of(term);
    let Some(before) = store.seen.replace(now) else {
        return;
    };
    match moved(&before, &now, term) {
        Some(0) => {}
        Some(rows) => store.lift(rows, term.topmost_line().0, None, cell),
        None => store.placements.clear(),
    }
}

/// Takes the main screen's look as it is now, after the placements were
/// changed by something that knows where they went.
pub(super) fn settle_main<T>(store: &mut Store, term: &Term<T>) {
    store.seen = (!store.placements.is_empty()).then(|| Seen::of(term));
}

fn identities<T>(term: &Term<T>) -> Vec<usize> {
    (0..term.screen_lines())
        .map(|line| row_identity(term, Line(i32::try_from(line).unwrap_or(i32::MAX))))
        .collect()
}

/// How many rows the alternate screen's content moved up (negative: down)
/// between two looks, read off the rows' identities. 0 when nothing moved or
/// the rows were not rotated as a whole.
fn rotation(before: &[usize], after: &[usize]) -> i32 {
    let (Some(&top_before), Some(&top_after)) = (before.first(), after.first()) else {
        return 0;
    };
    if top_before == top_after {
        return 0;
    }
    // An upward scroll by `n` leaves the old row `n` at the top; a downward
    // one leaves the old top row at row `n`. A scroll of `n` rows one way is
    // a rotation of `lines - n` the other, so the smaller is the one meant.
    let up = before.iter().position(|&row| row == top_after);
    let down = after.iter().position(|&row| row == top_before);
    match (up, down) {
        (Some(up), Some(down)) if down < up => -i32::try_from(down).unwrap_or(0),
        (Some(up), _) => i32::try_from(up).unwrap_or(0),
        (None, Some(down)) => -i32::try_from(down).unwrap_or(0),
        (None, None) => 0,
    }
}

/// Brings the alternate screen's placements to the grid as it is now.
pub(super) fn follow_alt<T>(store: &mut Store, term: &Term<T>, cell: (u16, u16)) {
    if store.placements.is_empty() {
        store.rows.clear();
        return;
    }
    let now = identities(term);
    let before = std::mem::replace(&mut store.rows, now);
    if before.len() != store.rows.len() {
        // Resized from under it, or never looked at.
        if !before.is_empty() {
            store.placements.clear();
        }
        return;
    }
    let rows = rotation(&before, &store.rows);
    if rows != 0 {
        let lines = i32::try_from(store.rows.len()).unwrap_or(i32::MAX);
        store.lift(rows, 0, Some(lines), cell);
    }
}

/// The alternate screen's look, as [`settle_main`].
pub(super) fn settle_alt<T>(store: &mut Store, term: &Term<T>) {
    store.rows = if store.placements.is_empty() {
        Vec::new()
    } else {
        identities(term)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rotation_is_read_off_the_row_identities() {
        let rows: Vec<usize> = (10..16).collect();
        assert_eq!(rotation(&rows, &rows), 0);
        // Scrolled up by 2: the old third row is on top, the old first two
        // were recycled at the bottom.
        let up = [12, 13, 14, 15, 10, 11];
        assert_eq!(rotation(&rows, &up), 2);
        // Scrolled down by 1: the old bottom row came round to the top.
        let down = [15, 10, 11, 12, 13, 14];
        assert_eq!(rotation(&rows, &down), -1);
        // Rows nobody knows are not a rotation.
        assert_eq!(rotation(&rows, &[90, 91, 92, 93, 94, 95]), 0);
        assert_eq!(rotation(&[], &[]), 0);
    }
}
