//! The menu as the palette's commands.
//!
//! The palette has no command list of its own: its commands **are** the application menu's
//! items, read as they stand when the palette opens. The toolkit turns its menu into a tree of
//! [`MenuNode`]s (a title, the action's selector, the shortcut, whether the item is on, hidden
//! or an alternate) and this module decides what becomes a row:
//!
//! - a separator, a hidden item, an alternate and an item the menu greys are **not** rows — the
//!   palette never offers what the menu would not run;
//! - an item with an action is a command row (`cmd.<selector>`; a tagged item keeps its tag in
//!   the id, `cmd.resizeSplit:1`, so the four directions are four commands);
//! - an item with a submenu is a **page** row (`scope.menu.<path>`, › in the list): ⏎ opens the
//!   page of the submenu's items. A page whose items are all off is not offered.
//!
//! The items of a submenu are also listed flat in a root search — typing "resize" finds the four
//! directions at once, the submenu's name as their second line — except when the submenu
//! is `paged`: one the shell fills as it opens (the themes, the listening ports, the tabs a split
//! can go to), whose items are many, or named after what is open, and mean something only under
//! their holder.

use std::collections::{HashMap, HashSet};

use super::row::{Kind, Row, Scope};
use super::synonyms::for_selector;

/// The modifier keys of a shortcut.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub control: bool,
    pub option: bool,
    pub shift: bool,
    pub command: bool,
}

impl Modifiers {
    pub const COMMAND: Modifiers = Modifiers {
        control: false,
        option: false,
        shift: false,
        command: true,
    };
}

/// The action of the item that opens the palette itself (View ▸ Command Palette…). The palette
/// does not list itself: choosing it would close the palette to open it again.
pub const PALETTE_ACTION: &str = "toggleCommandPalette:";

/// One item of the menu, as the toolkit read it. `token` is the toolkit's own handle on the
/// item (an index into the list it keeps), which comes back with the row so the item can be run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuNode {
    pub token: usize,
    pub title: String,
    /// The action's selector (`splitRight:`); `None` for an item that does nothing and a holder.
    pub selector: Option<String>,
    pub tag: isize,
    /// The shortcut's key as the menu stores it (`d`, `\u{F702}`), empty for none.
    pub key: String,
    pub modifiers: Modifiers,
    /// What the menu's validation said.
    pub enabled: bool,
    pub hidden: bool,
    pub alternate: bool,
    pub separator: bool,
    pub checked: bool,
    /// The items of the submenu this item opens.
    pub children: Option<Vec<MenuNode>>,
    /// The submenu's items are listed on its page only ([module](self)).
    pub paged: bool,
}

impl MenuNode {
    /// An item that runs `selector`.
    pub fn item(token: usize, title: &str, selector: &str) -> MenuNode {
        MenuNode {
            token,
            title: title.to_owned(),
            selector: Some(selector.to_owned()),
            tag: 0,
            key: String::new(),
            modifiers: Modifiers::COMMAND,
            enabled: true,
            hidden: false,
            alternate: false,
            separator: false,
            checked: false,
            children: None,
            paged: false,
        }
    }

    /// An item that opens a submenu.
    pub fn holder(token: usize, title: &str, children: Vec<MenuNode>) -> MenuNode {
        MenuNode {
            selector: None,
            children: Some(children),
            ..MenuNode::item(token, title, "")
        }
    }

    /// A line between groups of items.
    pub fn separator(token: usize) -> MenuNode {
        MenuNode {
            separator: true,
            selector: None,
            ..MenuNode::item(token, "", "")
        }
    }

    pub fn tag(mut self, tag: isize) -> MenuNode {
        self.tag = tag;
        self
    }

    pub fn shortcut(mut self, key: &str, modifiers: Modifiers) -> MenuNode {
        key.clone_into(&mut self.key);
        self.modifiers = modifiers;
        self
    }

    pub fn disabled(mut self) -> MenuNode {
        self.enabled = false;
        self
    }

    pub fn hidden(mut self) -> MenuNode {
        self.hidden = true;
        self
    }

    pub fn alternate(mut self) -> MenuNode {
        self.alternate = true;
        self
    }

    pub fn checked(mut self) -> MenuNode {
        self.checked = true;
        self
    }

    pub fn paged(mut self) -> MenuNode {
        self.paged = true;
        self
    }
}

/// A row of the menu and the item to run for it.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuRow {
    pub row: Row,
    /// The [`MenuNode::token`] of the item a command row runs; `None` for a page row.
    pub token: Option<usize>,
}

/// What the menu gives the palette.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MenuRows {
    /// The root's rows: every command in the menu (but those of a paged submenu), and every
    /// page row.
    pub flat: Vec<MenuRow>,
    /// The page of each submenu: the key of its [`Scope::Menu`] and the rows it lists.
    pub pages: Vec<(String, Vec<MenuRow>)>,
}

impl MenuRows {
    /// The rows of the root, for the matcher.
    pub fn root_rows(&self) -> Vec<Row> {
        self.flat.iter().map(|entry| entry.row.clone()).collect()
    }

    /// The rows of the page `key` names.
    pub fn page(&self, key: &str) -> Option<&[MenuRow]> {
        self.pages
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, rows)| rows.as_slice())
    }

    /// The item a row runs: the token of the row with this id, on the root or on any page.
    pub fn token_of(&self, id: &str) -> Option<usize> {
        self.flat
            .iter()
            .chain(self.pages.iter().flat_map(|(_, rows)| rows))
            .find(|entry| entry.row.id == id)
            .and_then(|entry| entry.token)
    }
}

/// The rows of a menu bar: `menus` are the bar's items, each holding its menu's items.
pub fn rows(menus: &[MenuNode]) -> MenuRows {
    let mut out = MenuRows::default();
    let mut seen: HashMap<String, String> = HashMap::new();
    for menu in menus {
        let Some(items) = &menu.children else {
            continue;
        };
        if menu.hidden {
            continue;
        }
        let path = vec![menu.title.clone()];
        walk(items, &path, false, &mut out, &mut seen);
    }
    out
}

/// One level of a menu: its rows (for the page of the item that holds it), the flat list and the
/// pages filled on the way.
fn walk(
    items: &[MenuNode],
    path: &[String],
    under_paged: bool,
    out: &mut MenuRows,
    seen: &mut HashMap<String, String>,
) -> Vec<MenuRow> {
    let mut level: Vec<MenuRow> = Vec::new();
    let ambiguous = ambiguous_keys(items);
    for node in items {
        if node.separator || node.hidden || node.alternate {
            continue;
        }
        if let Some(children) = &node.children {
            // A submenu the menu greys (Open Port without a port, Mark … as on a local tab) is
            // not offered, whatever its items would say.
            if !node.enabled {
                continue;
            }
            let mut below = path.to_vec();
            below.push(node.title.clone());
            let inside = walk(children, &below, under_paged || node.paged, out, seen);
            if inside.is_empty() {
                continue;
            }
            let key = slug(&below);
            let mut holder = Row::new(Kind::Scope, &format!("menu.{key}"), node.title.clone())
                .subtitle(path.join(" › "))
                .opens(Scope::Menu(key.clone()), true);
            holder.id = unique(holder.id, &node.title, seen);
            let entry = MenuRow {
                row: holder,
                token: None,
            };
            if !under_paged && !out.flat.iter().any(|listed| listed.row.id == entry.row.id) {
                out.flat.push(entry.clone());
            }
            let page: Vec<MenuRow> = inside
                .into_iter()
                .map(|mut inner| {
                    // On its own page a row needs no second line to say where it is.
                    inner.row.subtitle.clear();
                    inner
                })
                .collect();
            level.push(entry);
            out.pages.push((key, page));
            continue;
        }
        let Some(selector) = &node.selector else {
            continue;
        };
        if !node.enabled || node.title.trim().is_empty() || selector == PALETTE_ACTION {
            continue;
        }
        let base = command_key(node, selector);
        let mut row = command(node, selector, path, &base);
        if ambiguous.contains(&base) {
            // The same action under several titles (the themes): the title tells them apart.
            row.id = format!(
                "{}{}:{}",
                Kind::Command.prefix(),
                base,
                slug(&[node.title.clone()])
            );
        }
        row.id = unique(row.id, &node.title, seen);
        let entry = MenuRow {
            row,
            token: Some(node.token),
        };
        if !under_paged && !out.flat.iter().any(|listed| listed.row.id == entry.row.id) {
            out.flat.push(entry.clone());
        }
        if !level.iter().any(|listed| listed.row.id == entry.row.id) {
            level.push(entry);
        }
    }
    level
}

/// The key a menu item's command goes by: its selector, and its tag when it has one.
fn command_key(node: &MenuNode, selector: &str) -> String {
    let mut key = selector.trim_end_matches(':').to_owned();
    if node.tag != 0 {
        key.push_str(&format!(":{}", node.tag));
    }
    key
}

/// The keys one level of a menu gives to items of different titles: the themes all run
/// `selectTheme:`, and only their titles say which.
fn ambiguous_keys(items: &[MenuNode]) -> HashSet<String> {
    let mut titles: HashMap<String, HashSet<&str>> = HashMap::new();
    for node in items {
        let live = !node.separator
            && !node.hidden
            && !node.alternate
            && node.children.is_none()
            && node.enabled;
        if let (true, Some(selector)) = (live, &node.selector) {
            titles
                .entry(command_key(node, selector))
                .or_default()
                .insert(node.title.as_str());
        }
    }
    titles
        .into_iter()
        .filter(|(_, titles)| titles.len() > 1)
        .map(|(key, _)| key)
        .collect()
}

/// `id` for a row titled `title`: the same id again for the same title is the same command (the
/// menu lists Show Next Tab under two shortcuts); the same id for another title is another
/// command and gets a number.
fn unique(id: String, title: &str, seen: &mut HashMap<String, String>) -> String {
    let mut candidate = id.clone();
    let mut number = 1;
    loop {
        match seen.get(&candidate) {
            None => {
                seen.insert(candidate.clone(), title.to_owned());
                return candidate;
            }
            Some(known) if known == title => return candidate,
            Some(_) => {
                number += 1;
                candidate = format!("{id}.{number}");
            }
        }
    }
}

/// The row of a menu item that runs `selector`, under the key `key`.
fn command(node: &MenuNode, selector: &str, path: &[String], key: &str) -> Row {
    let mut row = Row::new(Kind::Command, key, node.title.clone())
        .keywords(for_selector(selector))
        .subtitle(path.join(" › "))
        .checked(node.checked);
    if let Some(glyphs) = shortcut(&node.key, node.modifiers) {
        row = row.shortcut(glyphs);
    }
    row
}

/// The key of a page: the titles on the way to the submenu, spelled in the letters an id
/// allows (`Window.Resize-Split`).
fn slug(path: &[String]) -> String {
    path.iter()
        .map(|title| {
            let mut part = String::new();
            let mut gap = false;
            for c in title.chars() {
                if c.is_ascii_alphanumeric() {
                    if gap && !part.is_empty() {
                        part.push('-');
                    }
                    gap = false;
                    part.push(c);
                } else {
                    gap = true;
                }
            }
            part
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// A shortcut as the glyphs a menu shows, in the order macOS writes them (⌃⌥⇧⌘ and the key):
/// `⇧⌘D`, `⌥⌘←`, `⌘+`. `None` for an item with no key.
///
/// A key that is a shifted symbol (`{`, `}`, `|`) is shown as the key that types it with ⇧ —
/// ⇧⌘[ — which is what the user presses; `+` stays as the menu shows it, `⌘+`.
pub fn shortcut(key: &str, modifiers: Modifiers) -> Option<String> {
    let (text, shifted) = key_glyph(key)?;
    // A key with no ⌃⌥⇧⌘ is no shortcut the user could press from the menu (AppKit's Start
    // Dictation is fn-D): it is not shown as a bare letter.
    if !(modifiers.control || modifiers.option || modifiers.shift || modifiers.command || shifted) {
        return None;
    }
    let mut out = String::new();
    if modifiers.control {
        out.push('⌃');
    }
    if modifiers.option {
        out.push('⌥');
    }
    if modifiers.shift || shifted {
        out.push('⇧');
    }
    if modifiers.command {
        out.push('⌘');
    }
    out.push_str(&text);
    Some(out)
}

/// The glyph of a key, and whether it is typed with ⇧ on top of the modifiers.
fn key_glyph(key: &str) -> Option<(String, bool)> {
    let mut chars = key.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let glyph = |text: &str| Some((text.to_owned(), false));
    match first {
        '\u{F700}' => glyph("↑"),
        '\u{F701}' => glyph("↓"),
        '\u{F702}' => glyph("←"),
        '\u{F703}' => glyph("→"),
        '\u{F729}' => glyph("↖"),
        '\u{F72B}' => glyph("↘"),
        '\u{F72C}' => glyph("⇞"),
        '\u{F72D}' => glyph("⇟"),
        '\t' => glyph("⇥"),
        '\r' | '\u{3}' => glyph("↩"),
        '\u{8}' | '\u{7f}' => glyph("⌫"),
        '\u{1b}' => glyph("⎋"),
        ' ' => glyph("Space"),
        '{' => Some(("[".to_owned(), true)),
        '}' => Some(("]".to_owned(), true)),
        '|' => Some(("\\".to_owned(), true)),
        c if c.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&c) => None,
        c => Some((c.to_uppercase().collect(), false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Nearness, Recent, search};

    const CMD: Modifiers = Modifiers::COMMAND;

    fn with(modifiers: &[char]) -> Modifiers {
        Modifiers {
            control: modifiers.contains(&'c'),
            option: modifiers.contains(&'o'),
            shift: modifiers.contains(&'s'),
            command: true,
        }
    }

    fn bar() -> Vec<MenuNode> {
        let directions = ["Left", "Right", "Up", "Down"]
            .iter()
            .enumerate()
            .map(|(tag, title)| {
                MenuNode::item(20 + tag, title, "resizeSplit:")
                    .tag(tag as isize)
                    .shortcut(
                        ["\u{F702}", "\u{F703}", "\u{F700}", "\u{F701}"][tag],
                        with(&['c']),
                    )
            })
            .collect();
        vec![
            MenuNode::holder(
                0,
                "Shell",
                vec![
                    MenuNode::item(1, "New Tab", "newTab:").shortcut("t", CMD),
                    MenuNode::item(2, "Split Right", "splitRight:").shortcut("d", CMD),
                    MenuNode::item(3, "Split Down", "splitDown:").shortcut("d", with(&['s'])),
                    MenuNode::separator(4),
                    MenuNode::item(5, "Cancel Upload", "cancelUpload:").disabled(),
                    MenuNode::item(6, "Quit and End Programs", "quitAndEndPrograms:").alternate(),
                    MenuNode::item(7, "Secret", "secret:").hidden(),
                ],
            ),
            MenuNode::holder(
                10,
                "Window",
                vec![
                    MenuNode::holder(11, "Resize Split", directions),
                    MenuNode::holder(
                        12,
                        "Open Port",
                        vec![MenuNode::item(13, "localhost:5173", "openPort:").tag(5173)],
                    )
                    .paged(),
                    MenuNode::holder(
                        14,
                        "Select Group",
                        vec![MenuNode::item(15, "Group 1", "selectGroup:").disabled()],
                    ),
                ],
            ),
        ]
    }

    fn ids(rows: &[MenuRow]) -> Vec<&str> {
        rows.iter().map(|entry| entry.row.id.as_str()).collect()
    }

    #[test]
    fn only_what_the_menu_would_run_is_a_row() {
        let menu = rows(&bar());
        let flat = ids(&menu.flat);
        assert!(flat.contains(&"cmd.splitRight"));
        // A separator, a hidden item, an alternate and a greyed item are not commands.
        for absent in ["cmd.cancelUpload", "cmd.quitAndEndPrograms", "cmd.secret"] {
            assert!(!flat.contains(&absent), "{absent}");
        }
        // A submenu whose items are all off is not a page.
        assert!(!flat.contains(&"scope.menu.Window.Select-Group"));
        assert!(menu.page("Window.Select-Group").is_none());
    }

    #[test]
    fn a_tag_keeps_the_four_directions_four_commands() {
        let menu = rows(&bar());
        let flat = ids(&menu.flat);
        for id in [
            "cmd.resizeSplit",
            "cmd.resizeSplit:1",
            "cmd.resizeSplit:2",
            "cmd.resizeSplit:3",
        ] {
            assert!(flat.contains(&id), "{id}");
        }
        assert_eq!(menu.token_of("cmd.resizeSplit:2"), Some(22));
        // Every id is one Recent keeps whole.
        for entry in &menu.flat {
            let id = &entry.row.id;
            let text = format!("{}\n{id}\n", crate::palette::RECENT_HEADER);
            assert_eq!(Recent::decode(&text).ids(), [id.clone()], "{id}");
        }
    }

    #[test]
    fn a_submenu_is_a_page_and_its_items_are_found_flat_with_its_name() {
        let menu = rows(&bar());
        let holder = menu
            .flat
            .iter()
            .find(|entry| entry.row.id == "scope.menu.Window.Resize-Split")
            .expect("the page row");
        assert_eq!(
            holder.row.opens,
            Some(Scope::Menu("Window.Resize-Split".to_owned()))
        );
        assert!(holder.row.enters);
        assert_eq!(holder.token, None);
        let page = menu.page("Window.Resize-Split").expect("its page");
        assert_eq!(page.len(), 4);
        assert!(page.iter().all(|entry| entry.row.subtitle.is_empty()));
        // On the root the same items carry the submenu's name, and "resize left" finds the one.
        let left = menu
            .flat
            .iter()
            .find(|entry| entry.row.id == "cmd.resizeSplit")
            .expect("flat");
        assert_eq!(left.row.subtitle, "Window › Resize Split");
        let hits = search(&menu.root_rows(), "resize left", Nearness::None);
        assert_eq!(hits[0].row.id, "cmd.resizeSplit");
    }

    #[test]
    fn a_paged_submenu_lists_its_items_on_its_page_only() {
        let menu = rows(&bar());
        assert!(!ids(&menu.flat).contains(&"cmd.openPort:5173"));
        assert_eq!(
            ids(menu.page("Window.Open-Port").expect("page")),
            ["cmd.openPort:5173"]
        );
        assert!(ids(&menu.flat).contains(&"scope.menu.Window.Open-Port"));
        // Its item still runs from the page.
        assert_eq!(menu.token_of("cmd.openPort:5173"), Some(13));
    }

    #[test]
    fn a_submenu_the_menu_greys_is_not_a_page() {
        let bar = vec![MenuNode::holder(
            0,
            "Shell",
            vec![
                MenuNode::holder(
                    1,
                    "Mark Host as",
                    vec![MenuNode::item(2, "Production", "markHost:")],
                )
                .disabled(),
            ],
        )];
        let menu = rows(&bar);
        assert!(menu.flat.is_empty());
        assert!(menu.pages.is_empty());
    }

    #[test]
    fn the_second_line_names_where_a_command_is_in_the_menu() {
        let menu = rows(&bar());
        let split = &menu.flat[1].row;
        assert_eq!(split.title, "Split Right");
        assert_eq!(split.subtitle, "Shell");
    }

    #[test]
    fn spr_finds_split_right_from_the_menu() {
        let menu = rows(&bar());
        let hits = search(&menu.root_rows(), "spr", Nearness::None);
        assert_eq!(hits[0].row.id, "cmd.splitRight");
        assert_eq!(hits[0].row.shortcut.as_deref(), Some("⌘D"));
    }

    #[test]
    fn a_command_is_found_once_though_two_items_run_it() {
        let twice = vec![MenuNode::holder(
            0,
            "Window",
            vec![
                MenuNode::item(1, "Show Next Tab", "showNextTab:").shortcut("}", CMD),
                MenuNode::item(2, "Show Next Tab", "showNextTab:").shortcut(
                    "\t",
                    Modifiers {
                        control: true,
                        ..Modifiers::default()
                    },
                ),
            ],
        )];
        let menu = rows(&twice);
        assert_eq!(menu.flat.len(), 1);
        assert_eq!(menu.flat[0].token, Some(1));
    }

    #[test]
    fn the_palette_does_not_list_itself() {
        let bar = vec![MenuNode::holder(
            0,
            "View",
            vec![
                MenuNode::item(1, "Command Palette…", PALETTE_ACTION).shortcut("p", with(&['s'])),
                MenuNode::item(2, "Bigger", "makeFontBigger:"),
            ],
        )];
        let flat = rows(&bar).flat;
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].row.id, "cmd.makeFontBigger");
    }

    #[test]
    fn items_that_share_an_action_and_differ_by_title_are_told_apart_by_it() {
        let bar = vec![MenuNode::holder(
            0,
            "View",
            vec![
                MenuNode::holder(
                    1,
                    "Theme",
                    vec![
                        MenuNode::item(2, "bateri", "selectTheme:"),
                        MenuNode::item(3, "Catppuccin Mocha", "selectTheme:"),
                        MenuNode::item(4, "Nord", "selectTheme:").checked(),
                        MenuNode::item(5, "Match System", "matchSystemTheme:"),
                    ],
                )
                .paged(),
            ],
        )];
        let menu = rows(&bar);
        let page = menu.page("View.Theme").expect("page");
        assert_eq!(
            ids(page),
            [
                "cmd.selectTheme:bateri",
                "cmd.selectTheme:Catppuccin-Mocha",
                "cmd.selectTheme:Nord",
                "cmd.matchSystemTheme"
            ]
        );
        assert_eq!(menu.token_of("cmd.selectTheme:Nord"), Some(4));
        // A paged submenu stays off the root.
        assert_eq!(ids(&menu.flat), ["scope.menu.View.Theme"]);
    }

    #[test]
    fn one_id_under_one_title_is_one_command_and_under_another_title_a_second() {
        let bar = vec![MenuNode::holder(
            0,
            "Window",
            vec![
                MenuNode::item(1, "Rename Tab…", "renameTab:"),
                MenuNode::holder(
                    2,
                    "More",
                    vec![MenuNode::item(3, "Rename Tab…", "renameTab:")],
                ),
                MenuNode::holder(
                    4,
                    "Other",
                    vec![MenuNode::item(5, "Rename Differently", "renameTab:")],
                ),
            ],
        )];
        let menu = rows(&bar);
        // The title seen twice is the one command, on the root once and on each page it is in.
        assert_eq!(
            ids(menu.page("Window.More").expect("page")),
            ["cmd.renameTab"]
        );
        // The other title is another command, and takes a number.
        assert_eq!(
            ids(menu.page("Window.Other").expect("page")),
            ["cmd.renameTab.2"]
        );
        assert_eq!(
            ids(&menu.flat),
            [
                "cmd.renameTab",
                "scope.menu.Window.More",
                "cmd.renameTab.2",
                "scope.menu.Window.Other"
            ]
        );
        assert_eq!(menu.token_of("cmd.renameTab.2"), Some(5));
    }

    #[test]
    fn a_checked_item_is_a_checked_row() {
        let on = vec![MenuNode::holder(
            0,
            "View",
            vec![MenuNode::item(1, "Side", "tabsAtSide:").checked()],
        )];
        assert!(rows(&on).flat[0].row.checked);
    }

    #[test]
    fn shortcuts_are_written_as_the_menu_writes_them() {
        let glyphs = |key: &str, modifiers: Modifiers| shortcut(key, modifiers);
        assert_eq!(glyphs("d", CMD).as_deref(), Some("⌘D"));
        assert_eq!(glyphs("d", with(&['s'])).as_deref(), Some("⇧⌘D"));
        assert_eq!(glyphs("k", with(&['o'])).as_deref(), Some("⌥⌘K"));
        assert_eq!(glyphs("v", with(&['c'])).as_deref(), Some("⌃⌘V"));
        assert_eq!(
            glyphs("\u{F702}", with(&['c', 'o', 's'])).as_deref(),
            Some("⌃⌥⇧⌘←")
        );
        assert_eq!(glyphs("\u{F729}", CMD).as_deref(), Some("⌘↖"));
        assert_eq!(glyphs("\u{F72D}", CMD).as_deref(), Some("⌘⇟"));
        assert_eq!(glyphs("+", CMD).as_deref(), Some("⌘+"));
        assert_eq!(glyphs("{", CMD).as_deref(), Some("⇧⌘["));
        assert_eq!(glyphs("}", with(&['o'])).as_deref(), Some("⌥⇧⌘]"));
        assert_eq!(glyphs("|", CMD).as_deref(), Some("⇧⌘\\"));
        assert_eq!(glyphs("\t", with(&['c'])).as_deref(), Some("⌃⌘⇥"));
        assert_eq!(glyphs("\r", with(&['s'])).as_deref(), Some("⇧⌘↩"));
        assert_eq!(glyphs(",", CMD).as_deref(), Some("⌘,"));
        assert_eq!(glyphs("0", CMD).as_deref(), Some("⌘0"));
        // No key: no shortcut, whatever the modifiers; no modifier: not a shortcut either.
        assert_eq!(glyphs("", CMD), None);
        assert_eq!(glyphs("d", Modifiers::default()), None);
        assert_eq!(glyphs("🎤", Modifiers::default()), None);
    }

    #[test]
    fn page_keys_use_only_letters_an_id_allows() {
        assert_eq!(
            slug(&["Window".to_owned(), "Move Tab to Group".to_owned()]),
            "Window.Move-Tab-to-Group"
        );
        assert_eq!(
            slug(&["Shell".to_owned(), "Mark “db” as".to_owned()]),
            "Shell.Mark-db-as"
        );
        let mut recent = Recent::new();
        assert!(recent.choose("scope.menu.Window.Move-Tab-to-Group"));
    }
}
