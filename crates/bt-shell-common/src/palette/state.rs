//! The palette as a state machine: the field, the highlight, the pages, the preview.
//!
//! The machine owns what the keyboard and the pointer change and nothing that is drawn. The view
//! gives it keys, the pointer's movement and the typed text, and **after every call re-derives the
//! list** from [`Palette::scope`] and [`Palette::query`] (the machine does not build rows — the
//! catalog of what exists is the application's) and hands it back through [`Palette::arrive`].
//! What a call returns is only what the state cannot say by itself: that the palette closes, that a
//! row was chosen, that the preview changed.
//!
//! **Pages.** The root is the bottom of a stack of [`Scope`]s. Opening a page keeps the field's
//! text and the row it was opened from; leaving it (esc, ← or ⌫ on an empty field) puts the text
//! back and lands the highlight on that row again, so the list is where the user left it.
//! A page of values (the themes, a setting's choices) lands on the value in effect.
//!
//! **Preview.** A row may show a setting's value without writing it ([`Row::preview`]). The
//! machine keeps what is being shown ([`Palette::preview`]) and reports each change as
//! [`Effect::Preview`] — `Some` to show, `None` to take back whatever was shown. The rule for
//! when:
//!
//! - moving the highlight with the arrows previews the row it lands on;
//! - the pointer previews only on a real movement ([`Palette::point`] is called from the move
//!   event, never from a list sliding under a pointer at rest);
//! - typing takes the preview back and shows nothing while the letters change the list; once the
//!   letters rest ([`REST`]) the view calls [`Palette::rested`] and a highlighted **theme**
//!   shows itself, as if the arrow had landed on it;
//! - opening or leaving a page takes it back; closing takes it back ([`Effect::Close`]'s
//!   `revert`) unless the caller made it permanent ([`Palette::commit_preview`]) because it wrote
//!   the value.

use std::time::Duration;

use super::row::{Kind, Preview, Row, Scope};

/// How long the field must stay unchanged before a highlighted theme shows itself.
pub const REST: Duration = Duration::from_millis(380);

/// What ⏎ is pressed with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modifier {
    /// Plain ⏎.
    None,
    /// ⌘⏎: the row's second use (jump to a command's output, copy a port's address).
    Command,
    /// ⌥⏎: the row's third use (split and wait there, use for dark appearance).
    Option,
}

/// A key the machine acts on. Anything else is typing and arrives through [`Palette::typed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter(Modifier),
    Tab,
    /// →, with whether the caret is at the end of the field: only there does it open a page.
    Right {
        at_end: bool,
    },
    Left,
    Backspace,
    Escape,
}

/// What a call changed that the state cannot say by itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Nothing the caller must act on (the state may still have moved: re-read it).
    None,
    /// The palette is done. `revert`: a preview is showing and must be taken back.
    Close { revert: bool },
    /// Show this value (`Some`), or take back whatever was shown (`None`).
    Preview(Option<Preview>),
    /// The row was chosen with this modifier; what that does is the caller's.
    Activate { id: String, modifier: Modifier },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Landing {
    First,
    Checked,
    Row(String),
}

#[derive(Clone, Debug)]
struct Page {
    scope: Scope,
    /// The id of the row the page was opened from.
    from: Option<String>,
    /// The field's text before the page took it.
    query: String,
}

/// The palette's state.
#[derive(Clone, Debug, Default)]
pub struct Palette {
    query: String,
    highlight: usize,
    stack: Vec<Page>,
    /// Where the highlight lands the next time the list is handed in ([`Self::arrive`]).
    landing: Option<Landing>,
    preview: Option<Preview>,
}

impl Palette {
    pub fn new() -> Palette {
        Palette::default()
    }

    /// The text in the field.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The highlighted row's place in the list handed in last (clamped by the caller's list:
    /// [`Self::cursor`]).
    pub fn highlight(&self) -> usize {
        self.highlight
    }

    /// The page on top; `None` at the root.
    pub fn scope(&self) -> Option<&Scope> {
        self.stack.last().map(|page| &page.scope)
    }

    /// How many pages are open above the root.
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// The value being shown without being written.
    pub fn preview(&self) -> Option<&Preview> {
        self.preview.as_ref()
    }

    /// The highlight inside a list of `len` rows: it may point past the end after the list
    /// shrank; it never does for the caller.
    pub fn cursor(&self, len: usize) -> usize {
        self.highlight.min(len.saturating_sub(1))
    }

    /// Takes a pending landing (after opening or leaving a page): the highlight goes to the value
    /// in effect, the row the page was opened from, or the first row. Call it with every list
    /// built; it moves the highlight once per page change and is a no-op otherwise.
    pub fn arrive(&mut self, rows: &[Row]) {
        let Some(landing) = self.landing.take() else {
            return;
        };
        self.highlight = match landing {
            Landing::First => None,
            Landing::Checked => rows.iter().position(|row| row.checked),
            Landing::Row(id) => rows.iter().position(|row| row.id == id),
        }
        .unwrap_or(0);
    }

    /// The text changed (the user typed): the highlight returns to the first row and any
    /// preview is taken back.
    pub fn typed(&mut self, text: &str) -> Effect {
        text.clone_into(&mut self.query);
        self.highlight = 0;
        self.landing = None;
        self.unpreview()
    }

    /// The letters have rested for [`REST`]. If the field still reads `seen` and a theme is
    /// highlighted, it shows itself. Stale calls (the user typed on) change nothing.
    pub fn rested(&mut self, seen: &str, rows: &[Row]) -> Effect {
        if self.query != seen {
            return Effect::None;
        }
        match rows.get(self.cursor(rows.len())) {
            Some(row) if row.kind == Kind::Theme => self.retarget(Some(row)),
            _ => Effect::None,
        }
    }

    /// The pointer moved over row `index`: it is highlighted and, if it previews, shown.
    pub fn point(&mut self, index: usize, rows: &[Row]) -> Effect {
        if index >= rows.len() || index == self.cursor(rows.len()) {
            return Effect::None;
        }
        self.highlight = index;
        self.retarget(rows.get(index))
    }

    /// A click on row `index`: the same as ⏎ on it.
    pub fn click(&mut self, index: usize, rows: &[Row]) -> Effect {
        match rows.get(index) {
            Some(row) => {
                self.highlight = index;
                self.activate(row, Modifier::None)
            }
            None => Effect::None,
        }
    }

    /// A key, over the list on screen.
    pub fn key(&mut self, key: Key, rows: &[Row]) -> Effect {
        let len = rows.len();
        let at = self.cursor(len);
        match key {
            Key::Up | Key::Down => {
                if len == 0 {
                    return Effect::None;
                }
                self.highlight = if key == Key::Down {
                    (at + 1) % len
                } else {
                    (at + len - 1) % len
                };
                self.retarget(rows.get(self.highlight))
            }
            Key::Enter(modifier) => match rows.get(at) {
                Some(row) => self.activate(row, modifier),
                None => Effect::None,
            },
            Key::Tab => match rows.get(at) {
                Some(row) if row.completion.is_some() => {
                    let text = row.completion.clone().unwrap_or_default();
                    self.typed(&text)
                }
                Some(row) => self.open_page_of(row),
                None => Effect::None,
            },
            Key::Right { at_end: true } => match rows.get(at) {
                Some(row) => self.open_page_of(row),
                None => Effect::None,
            },
            Key::Right { at_end: false } => Effect::None,
            Key::Left | Key::Backspace => {
                if self.query.is_empty() {
                    self.pop()
                } else {
                    Effect::None
                }
            }
            Key::Escape => {
                if self.stack.is_empty() {
                    self.close()
                } else {
                    self.pop()
                }
            }
        }
    }

    /// Opens a page. `from` is the id of the row it was opened from; leaving the page lands on
    /// it. A history page opened with a query starts the field with it.
    pub fn push(&mut self, scope: Scope, from: Option<&str>) -> Effect {
        let seed = match &scope {
            Scope::History {
                query: Some(query), ..
            } => query.clone(),
            _ => String::new(),
        };
        self.landing = Some(if scope.lands_on_checked() {
            Landing::Checked
        } else {
            Landing::First
        });
        let before = std::mem::replace(&mut self.query, seed);
        self.stack.push(Page {
            scope,
            from: from.map(str::to_owned),
            query: before,
        });
        self.highlight = 0;
        self.unpreview()
    }

    /// Leaves the page on top. At the root nothing happens.
    pub fn pop(&mut self) -> Effect {
        let Some(page) = self.stack.pop() else {
            return Effect::None;
        };
        self.query = page.query;
        self.highlight = 0;
        self.landing = Some(page.from.map_or(Landing::First, Landing::Row));
        self.unpreview()
    }

    /// The palette is dismissed (esc at the root, a click outside, the shortcut again).
    pub fn close(&mut self) -> Effect {
        let revert = self.preview.take().is_some();
        self.query.clear();
        self.stack.clear();
        self.highlight = 0;
        self.landing = None;
        Effect::Close { revert }
    }

    /// What was being shown has been written: it stays, and closing need not take it back.
    pub fn commit_preview(&mut self) {
        self.preview = None;
    }

    fn open_page_of(&mut self, row: &Row) -> Effect {
        match &row.opens {
            Some(scope) => self.push(scope.clone(), Some(&row.id)),
            None => Effect::None,
        }
    }

    /// ⏎: a row that opens a page by itself does so; any other is chosen.
    fn activate(&mut self, row: &Row, modifier: Modifier) -> Effect {
        if modifier == Modifier::None
            && row.enters
            && let Some(scope) = &row.opens
        {
            return self.push(scope.clone(), Some(&row.id));
        }
        Effect::Activate {
            id: row.id.clone(),
            modifier,
        }
    }

    /// The highlight is on `row`: show what it previews, take back what the last row showed.
    fn retarget(&mut self, row: Option<&Row>) -> Effect {
        let wanted = row.and_then(|row| row.preview.clone());
        if wanted == self.preview {
            return Effect::None;
        }
        self.preview.clone_from(&wanted);
        Effect::Preview(wanted)
    }

    fn unpreview(&mut self) -> Effect {
        if self.preview.take().is_some() {
            Effect::Preview(None)
        } else {
            Effect::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(name: &str) -> Row {
        Row::new(Kind::Theme, name, name).preview("appearance.theme", name)
    }

    fn command(name: &str) -> Row {
        Row::new(Kind::Command, name, name)
    }

    fn shown(name: &str) -> Effect {
        Effect::Preview(Some(Preview {
            key: "appearance.theme".into(),
            token: name.into(),
        }))
    }

    #[test]
    fn the_arrows_wrap_around_the_list() {
        let rows = [command("a"), theme("nord"), theme("dracula")];
        let mut p = Palette::new();
        // Up from the first row lands on the last, a theme: it shows itself.
        assert_eq!(p.key(Key::Up, &rows), shown("dracula"));
        assert_eq!(p.highlight(), 2);
        // Down from the last lands on the first, a command: the theme is taken back.
        assert_eq!(p.key(Key::Down, &rows), Effect::Preview(None));
        assert_eq!(p.highlight(), 0);
    }

    #[test]
    fn moving_over_themes_previews_each_and_leaving_them_takes_it_back() {
        let rows = [command("a"), theme("nord"), theme("dracula")];
        let mut p = Palette::new();
        assert_eq!(p.key(Key::Down, &rows), shown("nord"));
        assert_eq!(p.key(Key::Down, &rows), shown("dracula"));
        assert_eq!(p.preview().map(|v| v.token.as_str()), Some("dracula"));
        // Wrapping onto the command: nothing to show, so the last is taken back.
        assert_eq!(p.key(Key::Down, &rows), Effect::Preview(None));
        assert!(p.preview().is_none());
        // And a row that previews nothing, after one that previews nothing, says nothing.
        let two = [command("a"), command("b")];
        assert_eq!(p.key(Key::Down, &two), Effect::None);
    }

    #[test]
    fn typing_takes_the_preview_back_and_a_rested_field_shows_the_theme() {
        let rows = [theme("nord"), theme("dracula")];
        let mut p = Palette::new();
        p.key(Key::Down, &rows);
        assert_eq!(p.typed("dra"), Effect::Preview(None));
        assert_eq!(p.highlight(), 0);
        // Typed on before the rest ended: the stale call changes nothing.
        p.typed("drac");
        assert_eq!(p.rested("dra", &rows[1..]), Effect::None);
        assert!(p.preview().is_none());
        // Rested: the highlighted theme shows itself.
        assert_eq!(p.rested("drac", &rows[1..]), shown("dracula"));
        // A second rest on the same row says nothing again.
        assert_eq!(p.rested("drac", &rows[1..]), Effect::None);
        // Only themes show themselves on rest, not any previewing row.
        let choice = [Row::new(Kind::Choice, "beam", "Beam").preview("terminal.cursor", "beam")];
        let mut q = Palette::new();
        q.typed("be");
        assert_eq!(q.rested("be", &choice), Effect::None);
    }

    #[test]
    fn the_pointer_previews_only_when_it_moves_to_another_row() {
        let rows = [theme("nord"), theme("dracula")];
        let mut p = Palette::new();
        assert_eq!(p.point(0, &rows), Effect::None, "already highlighted");
        assert_eq!(p.point(1, &rows), shown("dracula"));
        assert_eq!(p.point(1, &rows), Effect::None);
        assert_eq!(p.point(9, &rows), Effect::None, "past the list");
    }

    #[test]
    fn a_page_opens_from_a_row_and_leaving_it_lands_on_that_row_with_the_field_as_it_was() {
        let from = Row::new(Kind::Setting, "terminal.cursor", "Cursor Shape")
            .opens(Scope::Choices("terminal.cursor".into()), true);
        let rows = [command("a"), command("b"), from.clone()];
        let mut p = Palette::new();
        p.typed("cursor");
        p.key(Key::Down, &rows);
        p.key(Key::Down, &rows);
        assert_eq!(
            p.key(Key::Enter(Modifier::None), &rows),
            Effect::None,
            "⏎ on a row that enters opens its page"
        );
        assert_eq!(p.scope(), Some(&Scope::Choices("terminal.cursor".into())));
        assert_eq!(p.query(), "");
        assert_eq!(p.depth(), 1);
        // On the page the highlight lands on the value in effect.
        let page = [
            Row::new(Kind::Choice, "block", "Block"),
            Row::new(Kind::Choice, "beam", "Beam").checked(true),
        ];
        p.arrive(&page);
        assert_eq!(p.highlight(), 1);
        // The landing is taken once.
        p.key(Key::Up, &page);
        p.arrive(&page);
        assert_eq!(p.highlight(), 0);
        // esc: one page up, the field and the row as they were.
        assert_eq!(p.key(Key::Escape, &page), Effect::None);
        assert_eq!(p.depth(), 0);
        assert_eq!(p.query(), "cursor");
        p.arrive(&rows);
        assert_eq!(p.highlight(), 2);
    }

    #[test]
    fn a_theme_page_lands_on_the_theme_in_effect_and_leaving_takes_the_preview_back() {
        let source = Row::new(Kind::Scope, "themes", "Themes…")
            .opens(Scope::Theme(super::super::row::Slot::Active), true);
        let mut p = Palette::new();
        p.key(Key::Enter(Modifier::None), &[source]);
        let page = [
            theme("nord"),
            theme("dracula").checked(true),
            theme("linen"),
        ];
        p.arrive(&page);
        assert_eq!(p.highlight(), 1);
        assert_eq!(p.key(Key::Down, &page), shown("linen"));
        // esc takes the shown theme back as it leaves the page.
        assert_eq!(p.key(Key::Escape, &page), Effect::Preview(None));
        assert_eq!(p.depth(), 0);
    }

    #[test]
    fn esc_at_the_root_closes_and_says_whether_a_preview_must_be_taken_back() {
        let rows = [theme("nord")];
        let mut p = Palette::new();
        assert_eq!(p.key(Key::Escape, &rows), Effect::Close { revert: false });
        p.key(Key::Down, &rows);
        assert!(p.preview().is_some());
        assert_eq!(p.key(Key::Escape, &rows), Effect::Close { revert: true });
        assert!(p.preview().is_none());
        // Written for good: closing has nothing to take back.
        let mut q = Palette::new();
        q.key(Key::Down, &rows);
        q.commit_preview();
        assert_eq!(q.close(), Effect::Close { revert: false });
    }

    #[test]
    fn enter_chooses_a_row_unless_the_row_opens_a_page_by_itself() {
        let tab = Row::new(Kind::Tab, "7", "api").opens(Scope::TabActions("tab.7".into()), false);
        let mut p = Palette::new();
        assert_eq!(
            p.key(Key::Enter(Modifier::None), std::slice::from_ref(&tab)),
            Effect::Activate {
                id: "tab.7".into(),
                modifier: Modifier::None
            }
        );
        assert_eq!(p.depth(), 0);
        // The second and third uses are always a choice, even on a row that enters.
        let setting = Row::new(Kind::Setting, "font.size", "Font Size")
            .opens(Scope::Choices("font.size".into()), true);
        assert_eq!(
            p.key(
                Key::Enter(Modifier::Command),
                std::slice::from_ref(&setting)
            ),
            Effect::Activate {
                id: "set.font.size".into(),
                modifier: Modifier::Command
            }
        );
        assert_eq!(
            p.key(Key::Enter(Modifier::Option), std::slice::from_ref(&setting)),
            Effect::Activate {
                id: "set.font.size".into(),
                modifier: Modifier::Option
            }
        );
        // Nothing to choose from.
        assert_eq!(p.key(Key::Enter(Modifier::None), &[]), Effect::None);
        assert_eq!(p.key(Key::Down, &[]), Effect::None);
    }

    #[test]
    fn tab_completes_a_folder_and_otherwise_opens_the_page_and_the_right_arrow_needs_the_caret_at_the_end()
     {
        let folder = Row::new(Kind::Folder, "~/Projects", "~/Projects").completion("~/Projects/");
        let tab = Row::new(Kind::Tab, "7", "api").opens(Scope::TabActions("tab.7".into()), false);
        let mut p = Palette::new();
        p.typed("~/Pro");
        assert_eq!(p.key(Key::Tab, std::slice::from_ref(&folder)), Effect::None);
        assert_eq!(p.query(), "~/Projects/");
        assert_eq!(p.depth(), 0);
        // →, caret not at the end: the caret moves, the palette does not.
        assert_eq!(
            p.key(Key::Right { at_end: false }, std::slice::from_ref(&tab)),
            Effect::None
        );
        assert_eq!(p.depth(), 0);
        p.key(Key::Right { at_end: true }, std::slice::from_ref(&tab));
        assert_eq!(p.scope(), Some(&Scope::TabActions("tab.7".into())));
        // ⇥ on a row with neither a completion nor a page does nothing.
        let mut q = Palette::new();
        assert_eq!(q.key(Key::Tab, &[command("a")]), Effect::None);
    }

    #[test]
    fn backspace_and_left_leave_a_page_only_from_an_empty_field() {
        let rows = [command("a")];
        let mut p = Palette::new();
        p.push(Scope::Folders, None);
        p.typed("x");
        assert_eq!(p.key(Key::Backspace, &rows), Effect::None);
        assert_eq!(
            p.depth(),
            1,
            "the field is edited by the text system, not left"
        );
        p.typed("");
        p.key(Key::Left, &rows);
        assert_eq!(p.depth(), 0);
        // At the root, ⌫ on an empty field leaves nothing to leave.
        assert_eq!(p.key(Key::Backspace, &rows), Effect::None);
    }

    #[test]
    fn a_history_page_opened_with_a_query_starts_the_field_with_it() {
        let mut p = Palette::new();
        p.typed("cargo");
        p.push(
            Scope::History {
                jump: false,
                query: Some("cargo".into()),
            },
            Some("search.history"),
        );
        assert_eq!(p.query(), "cargo");
        p.pop();
        assert_eq!(p.query(), "cargo");
    }

    #[test]
    fn a_click_is_enter_on_that_row() {
        let rows = [command("a"), command("b")];
        let mut p = Palette::new();
        assert_eq!(
            p.click(1, &rows),
            Effect::Activate {
                id: "cmd.b".into(),
                modifier: Modifier::None
            }
        );
        assert_eq!(p.highlight(), 1);
        assert_eq!(p.click(5, &rows), Effect::None);
    }

    #[test]
    fn the_cursor_stays_inside_a_list_that_shrank() {
        let mut p = Palette::new();
        let rows = [command("a"), command("b"), command("c")];
        p.key(Key::Down, &rows);
        p.key(Key::Down, &rows);
        assert_eq!(p.cursor(3), 2);
        assert_eq!(p.cursor(1), 0);
        assert_eq!(p.cursor(0), 0);
    }
}
