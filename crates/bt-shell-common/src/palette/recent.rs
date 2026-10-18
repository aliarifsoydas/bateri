//! Recent: the last few things the user chose in the palette, kept across launches.
//!
//! Only the **identity** of a command, a setting or a page is kept ([`super::Kind::remembered`]:
//! `cmd.splitRight`, `set.appearance.theme`, `scope.tabs`) — never a tab, a command's text, a
//! folder or a host: those are the user's data, and the file is the application's own state.
//!
//! The text form is line based, as the other small files beside the session layout are: the first
//! line names the version ([`HEADER`]), then one id per line, the latest first. Reading is
//! tolerant where the content is a convenience: a line that is no id of a remembered kind is
//! skipped, repeats and anything past [`LIMIT`] are dropped, and a file of a version this build
//! does not know reads as empty (a newer build may have given the lines another meaning).
//! Nothing here touches the disk; the shell owns the file's place and its removal.

/// How many ids are kept.
pub const LIMIT: usize = 5;

/// The first line of the file, version included.
pub const HEADER: &str = "bateri-palette-recent 1";

const REMEMBERED_PREFIXES: [&str; 3] = ["cmd.", "set.", "scope."];

/// The ids chosen last, the latest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recent {
    ids: Vec<String>,
}

impl Recent {
    pub fn new() -> Recent {
        Recent::default()
    }

    /// The ids, latest first.
    pub fn ids(&self) -> &[String] {
        &self.ids
    }

    /// Records a choice. Returns whether Recent changed — `false` for an id of a kind that is
    /// not remembered, or the one already first.
    pub fn choose(&mut self, id: &str) -> bool {
        if !remembers(id) || self.ids.first().is_some_and(|first| first == id) {
            return false;
        }
        self.ids.retain(|seen| seen != id);
        self.ids.insert(0, id.to_owned());
        self.ids.truncate(LIMIT);
        true
    }

    /// The file's text.
    pub fn encode(&self) -> String {
        let mut text = String::from(HEADER);
        text.push('\n');
        for id in &self.ids {
            text.push_str(id);
            text.push('\n');
        }
        text
    }

    /// Reads a file's text; anything unreadable is an empty Recent.
    pub fn decode(text: &str) -> Recent {
        let mut lines = text.lines();
        if lines.next().map(str::trim_end) != Some(HEADER) {
            return Recent::default();
        }
        let mut ids: Vec<String> = Vec::new();
        for line in lines {
            let id = line.trim_end();
            if remembers(id) && !ids.iter().any(|seen| seen == id) {
                ids.push(id.to_owned());
            }
            if ids.len() == LIMIT {
                break;
            }
        }
        Recent { ids }
    }
}

/// Whether an id is of a remembered kind and shaped like an id: a prefix, then a key of
/// letters, digits and `_ . : -` only (a selector keeps its colon, a setting its dots).
fn remembers(id: &str) -> bool {
    REMEMBERED_PREFIXES.iter().any(|prefix| {
        id.strip_prefix(prefix).is_some_and(|key| {
            !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_commands_settings_and_pages_are_remembered() {
        let mut recent = Recent::new();
        assert!(recent.choose("cmd.splitRight"));
        assert!(recent.choose("set.appearance.theme"));
        assert!(recent.choose("scope.tabs"));
        for not_remembered in [
            "tab.7",
            "grp.2",
            "hist.4",
            "dir.~/Projects",
            "host.db-1",
            "port.5173",
            "theme.nord",
            "cmd.",
            "",
        ] {
            assert!(!recent.choose(not_remembered), "{not_remembered}");
        }
        assert_eq!(
            recent.ids(),
            ["scope.tabs", "set.appearance.theme", "cmd.splitRight"]
        );
    }

    #[test]
    fn a_choice_made_again_moves_up_and_only_five_are_kept() {
        let mut recent = Recent::new();
        for n in 0..7 {
            recent.choose(&format!("cmd.c{n}"));
        }
        assert_eq!(recent.ids().len(), LIMIT);
        assert_eq!(recent.ids()[0], "cmd.c6");
        assert!(recent.choose("cmd.c3"));
        assert_eq!(recent.ids()[0], "cmd.c3");
        assert_eq!(recent.ids().iter().filter(|id| *id == "cmd.c3").count(), 1);
        // The one already first changes nothing, so nothing needs writing.
        assert!(!recent.choose("cmd.c3"));
    }

    #[test]
    fn the_text_round_trips_and_names_its_version() {
        let mut recent = Recent::new();
        recent.choose("cmd.newTab");
        recent.choose("set.font.size");
        let text = recent.encode();
        assert!(text.starts_with("bateri-palette-recent 1\n"));
        assert_eq!(Recent::decode(&text), recent);
    }

    #[test]
    fn a_reader_skips_what_it_does_not_know_and_refuses_a_newer_file() {
        let text = "bateri-palette-recent 1\n\
                    cmd.splitRight\n\
                    tab.9\n\
                    nonsense\n\
                    cmd.splitRight\n\
                    set.terminal.cursor extra words\n\
                    scope.folders\n";
        let read = Recent::decode(text);
        assert_eq!(read.ids(), ["cmd.splitRight", "scope.folders"]);
        // Longer than the limit: the first five, no more.
        let long = format!(
            "{HEADER}\n{}",
            (0..9).map(|n| format!("cmd.c{n}\n")).collect::<String>()
        );
        assert_eq!(Recent::decode(&long).ids().len(), LIMIT);
        // A version this build does not know, a missing header, an empty file.
        assert_eq!(
            Recent::decode("bateri-palette-recent 2\ncmd.newTab\n"),
            Recent::new()
        );
        assert_eq!(Recent::decode("cmd.newTab\n"), Recent::new());
        assert_eq!(Recent::decode(""), Recent::new());
    }
}
