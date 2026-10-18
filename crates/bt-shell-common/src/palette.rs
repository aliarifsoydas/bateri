//! The command palette's rules: what a row is, how a query finds it, how the pages and the
//! keys move, and what a setting's value is.
//!
//! Everything here is pure and tried without a window; the toolkit draws the list and feeds the
//! machine ([`Palette`]) its keys. The Linux shell will draw the same palette from the same rules.
//!
//! - [`Row`], [`Kind`], [`Scope`] — a line of the list, as data.
//! - [`matching`] and [`rank`] — letters in order and word starts, keywords and second lines;
//!   the nearness of Recent and of the tab just left; the clusters and their fallbacks.
//! - [`menu_rows`] — the application menu read as commands, and [`root_list`] / [`page_list`] — the
//!   list a query leaves on a page, under its headings.
//! - [`Palette`] — the field, the highlight, the page stack and the preview, driven by [`Key`]s.
//! - [`catalog`] — the settings the palette changes, each value an edit the settings model owns.
//! - [`Recent`] and [`Visits`] — what was chosen last and which tab was looked at last.
//! - [`synonyms`] — the words a menu command is also found by, keyed by its selector.
//! - [`PathQuery`] — a path typed in the field.
//!
//! The palette's commands are the **menu's own** (the shell reads the menu, so a command exists
//! in the palette exactly when it exists in the menu); the rows of the menu are built by the
//! shell, the rule that finds them is here.

mod catalog;
mod fold;
mod list;
mod matching;
mod menu;
mod path;
mod rank;
mod recent;
mod row;
mod state;
mod synonyms;
mod visits;

pub use catalog::{
    Choice, Control, Section, Setting, all as settings, find as setting, rows as setting_rows,
};
pub use list::{Fallbacks, List, RECENT_HEADING, page as page_list, root as root_list};
pub use matching::{Match, score};
pub use menu::{
    MenuNode, MenuRow, MenuRows, Modifiers, PALETTE_ACTION, rows as menu_rows, shortcut,
};
pub use path::PathQuery;
pub use rank::{
    CLUSTER_LIMIT, Cluster, Hit, Nearness, RECENT_BONUS, THEME_LIMIT, VISIT_BONUS, VISIT_STEP,
    cluster, fallbacks, recent_rows, root, search, tabs,
};
pub use recent::{HEADER as RECENT_HEADER, LIMIT as RECENT_LIMIT, Recent};
pub use row::{Kind, Preview, Row, Scope, Slot, Status, Tone, command_id};
pub use state::{Effect, Key, Modifier, Palette, REST};
pub use synonyms::for_selector as command_words;
pub use visits::Visits;

#[cfg(test)]
mod tests;
