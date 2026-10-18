//! The themes the palette lists: the embedded ones in their groups and the user's own, as rows
//! with their colours, and what choosing one writes.
//!
//! A theme page is for one **slot** ([`Slot`]): the one theme (`theme`, which also takes "Match
//! System"), or the theme for the dark or the light appearance. Highlighting a row previews it for
//! that slot ([`Row::preview`]); choosing it writes the slot's key.
//!
//! ⌥⏎ on the page of the one theme is the third use of a row: it writes the theme only for the
//! appearance the system has now (`dark_theme` or `light_theme`) and turns Match System on, so a
//! theme picked for the dark mode does not pin the light one to it.

use std::path::Path;

use bt_core::{SYSTEM_THEME, Settings, SettingsEdit, Theme};

use super::catalog;
use super::row::{Kind, Row, Slot, Swatch};
use super::state::Modifier;
use crate::settings::{ThemeLoaded, load_theme, user_theme_names};

/// Which list of the theme menu a theme comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// bateri's own.
    Own,
    /// The adapted palettes on a dark ground.
    Dark,
    /// The adapted palettes on a light ground.
    Light,
    /// The files in `themes/`.
    Yours,
}

impl Group {
    /// The second line of a theme's row.
    pub fn label(self) -> &'static str {
        match self {
            Group::Own => "bateri",
            Group::Dark => "Dark",
            Group::Light => "Light",
            Group::Yours => "Yours",
        }
    }
}

/// A theme the palette can offer.
#[derive(Clone, Debug, PartialEq)]
pub struct ThemeEntry {
    /// What the settings file takes.
    pub name: String,
    /// What the row shows: an embedded theme's title, a user theme's name.
    pub title: String,
    pub group: Group,
    pub swatch: Swatch,
}

/// A theme's colours as the row shows them.
pub fn swatch(theme: &Theme) -> Swatch {
    Swatch {
        ground: theme.background,
        // ANSI red, green, yellow, blue, magenta, cyan: the colours a theme is known by.
        dots: [
            theme.ansi[1],
            theme.ansi[2],
            theme.ansi[3],
            theme.ansi[4],
            theme.ansi[5],
            theme.ansi[6],
        ],
    }
}

/// Every theme in the theme menu's order: bateri's own, the dark ones, the light ones, then the
/// user's. With a `root` a file in `themes/` that shadows an embedded name shows its own colours
/// (the same resolution the window uses), and a user theme that cannot be read is left out — the
/// menu would list it, but choosing it would change nothing.
pub fn entries(root: Option<&Path>) -> Vec<ThemeEntry> {
    let resolve = |name: &str| -> Option<Theme> {
        match root {
            Some(root) => match load_theme(Some(root), name) {
                ThemeLoaded::Found(theme, _) => Some(theme),
                ThemeLoaded::Failed(_) => Theme::embedded(name),
            },
            None => Theme::embedded(name),
        }
    };
    let mut out = Vec::new();
    let [own, dark, light] = Theme::embedded_groups();
    for (group, names) in [
        (Group::Own, own),
        (Group::Dark, dark),
        (Group::Light, light),
    ] {
        for name in names {
            if let Some(theme) = resolve(name) {
                out.push(ThemeEntry {
                    name: name.to_owned(),
                    title: Theme::embedded_title(name).unwrap_or(name).to_owned(),
                    group,
                    swatch: swatch(&theme),
                });
            }
        }
    }
    if let Some(root) = root {
        for name in user_theme_names(root) {
            if let ThemeLoaded::Found(theme, _) = load_theme(Some(root), &name) {
                out.push(ThemeEntry {
                    title: name.clone(),
                    name,
                    group: Group::Yours,
                    swatch: swatch(&theme),
                });
            }
        }
    }
    out
}

/// The theme `settings` holds for `slot`.
pub fn current(slot: Slot, settings: &Settings) -> &str {
    match slot {
        Slot::Active => &settings.theme,
        Slot::Dark => &settings.dark_theme,
        Slot::Light => &settings.light_theme,
    }
}

fn title_of<'a>(entries: &'a [ThemeEntry], name: &'a str) -> &'a str {
    entries
        .iter()
        .find(|entry| entry.name == name)
        .map_or(name, |entry| entry.title.as_str())
}

/// The rows of `slot`'s page: for the one theme "Match System" first (its second line says which
/// themes it switches between, light then dark), then every theme; the one in effect is checked.
/// Each row previews itself when highlighted.
pub fn rows(slot: Slot, entries: &[ThemeEntry], settings: &Settings) -> Vec<Row> {
    let key = catalog::theme_setting(slot).key();
    let held = current(slot, settings);
    let mut rows = Vec::with_capacity(entries.len() + 1);
    if slot == Slot::Active {
        rows.push(
            Row::new(Kind::Theme, SYSTEM_THEME, "Match System")
                .keywords("auto automatic system macos appearance light dark theme")
                .subtitle(format!(
                    "{} · {}",
                    title_of(entries, &settings.light_theme),
                    title_of(entries, &settings.dark_theme)
                ))
                .checked(held == SYSTEM_THEME)
                .preview(key, SYSTEM_THEME),
        );
    }
    for entry in entries {
        rows.push(
            Row::new(Kind::Theme, &entry.name, entry.title.clone())
                .keywords(format!("{} theme colors", entry.group.label()))
                .subtitle(entry.group.label())
                .checked(entry.name == held)
                .swatch(entry.swatch)
                .preview(key, &entry.name),
        );
    }
    rows
}

/// What choosing the theme `name` on `slot`'s page writes, in the order it is written. `dark` is
/// the system's appearance now. ⌘⏎ is not a use of a theme row.
pub fn edits(slot: Slot, name: &str, modifier: Modifier, dark: bool) -> Vec<SettingsEdit> {
    let own = catalog::theme_setting(slot);
    let Some(edit) = own.edit(name) else {
        return Vec::new();
    };
    match (slot, modifier) {
        (_, Modifier::Command) => Vec::new(),
        (Slot::Active, Modifier::Option) if name != SYSTEM_THEME => {
            let appearance = catalog::theme_setting(if dark { Slot::Dark } else { Slot::Light });
            let mut out = Vec::new();
            out.extend(appearance.edit(name));
            out.extend(own.edit(SYSTEM_THEME));
            out
        }
        _ => vec![edit],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, title: &str, group: Group) -> ThemeEntry {
        ThemeEntry {
            name: name.to_owned(),
            title: title.to_owned(),
            group,
            swatch: swatch(&Theme::BATERI),
        }
    }

    fn some() -> Vec<ThemeEntry> {
        vec![
            entry("bateri", "bateri", Group::Own),
            entry("nord", "Nord", Group::Dark),
            entry("rose-pine-dawn", "Rosé Pine Dawn", Group::Light),
        ]
    }

    #[test]
    fn the_embedded_themes_come_in_the_menus_groups_with_their_titles() {
        let all = entries(None);
        assert_eq!(all.len(), Theme::embedded_names().count());
        assert_eq!(all[0].group, Group::Own);
        let nord = all.iter().find(|entry| entry.name == "nord");
        assert_eq!(nord.map(|entry| entry.title.as_str()), Some("Nord"));
        // Dark ones before light ones, as the menu has them.
        let first_light = all.iter().position(|entry| entry.group == Group::Light);
        let last_dark = all.iter().rposition(|entry| entry.group == Group::Dark);
        assert!(first_light > last_dark);
    }

    #[test]
    fn a_page_for_the_one_theme_starts_with_match_system_and_checks_what_is_in_effect() {
        let settings = Settings {
            theme: "nord".to_owned(),
            ..Settings::default()
        };
        let rows = rows(Slot::Active, &some(), &settings);
        assert_eq!(rows[0].id, "theme.system");
        assert_eq!(rows[0].title, "Match System");
        assert!(!rows[0].checked);
        let checked: Vec<_> = rows.iter().filter(|row| row.checked).collect();
        assert_eq!(checked.len(), 1);
        assert_eq!(checked[0].id, "theme.nord");
        assert_eq!(checked[0].subtitle, "Dark");
        assert!(checked[0].swatch.is_some());
    }

    #[test]
    fn match_system_says_which_themes_it_switches_between_light_first() {
        let settings = Settings {
            theme: SYSTEM_THEME.to_owned(),
            light_theme: "rose-pine-dawn".to_owned(),
            dark_theme: "nord".to_owned(),
            ..Settings::default()
        };
        let rows = rows(Slot::Active, &some(), &settings);
        assert!(rows[0].checked);
        assert_eq!(rows[0].subtitle, "Rosé Pine Dawn · Nord");
    }

    #[test]
    fn the_appearance_pages_have_no_match_system_and_preview_their_own_key() {
        let rows = rows(Slot::Dark, &some(), &Settings::default());
        assert!(rows.iter().all(|row| row.id != "theme.system"));
        let preview = rows[1].preview.clone().expect("a theme previews");
        assert_eq!(preview.key, "appearance.dark_theme");
        assert_eq!(preview.token, rows[1].id.trim_start_matches("theme."));
        let one = super::rows(Slot::Active, &some(), &Settings::default());
        assert_eq!(
            one[0].preview.as_ref().map(|p| p.key.as_str()),
            Some("appearance.theme")
        );
    }

    #[test]
    fn a_plain_choice_writes_the_slots_own_key() {
        assert_eq!(
            edits(Slot::Active, "nord", Modifier::None, true),
            [SettingsEdit::Theme("nord".to_owned())]
        );
        assert_eq!(
            edits(Slot::Dark, "nord", Modifier::None, false),
            [SettingsEdit::DarkTheme("nord".to_owned())]
        );
        assert_eq!(
            edits(Slot::Light, "nord", Modifier::Option, true),
            [SettingsEdit::LightTheme("nord".to_owned())]
        );
        assert_eq!(
            edits(Slot::Active, SYSTEM_THEME, Modifier::None, true),
            [SettingsEdit::Theme(SYSTEM_THEME.to_owned())]
        );
    }

    #[test]
    fn option_return_on_the_one_themes_page_writes_the_theme_for_this_appearance_and_match_system()
    {
        assert_eq!(
            edits(Slot::Active, "nord", Modifier::Option, true),
            [
                SettingsEdit::DarkTheme("nord".to_owned()),
                SettingsEdit::Theme(SYSTEM_THEME.to_owned()),
            ]
        );
        assert_eq!(
            edits(Slot::Active, "nord", Modifier::Option, false),
            [
                SettingsEdit::LightTheme("nord".to_owned()),
                SettingsEdit::Theme(SYSTEM_THEME.to_owned()),
            ]
        );
        // Match System itself has no appearance to pin.
        assert_eq!(
            edits(Slot::Active, SYSTEM_THEME, Modifier::Option, true),
            [SettingsEdit::Theme(SYSTEM_THEME.to_owned())]
        );
        assert!(edits(Slot::Active, "nord", Modifier::Command, true).is_empty());
    }
}
