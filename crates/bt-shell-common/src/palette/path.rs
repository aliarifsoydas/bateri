//! Typing a path in the field: `~/Pro`, `/usr/lo`.
//!
//! A query that begins with `~` or `/` is a path, not a search: it splits into the **directory**
//! whose entries are listed and the **stem** the entries must begin with. Listing the directory
//! is the caller's (it reads the disk, or asks the shell of a remote tab); this is the rule that
//! cuts the text and decides which entries stay.

use super::fold::{Folded, word};

/// A path query, cut at its last slash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathQuery {
    /// The directory to list: `~`, `/`, or the text before the last slash (`~/Projects`).
    pub base: String,
    /// What the entries' names must begin with: the text after the last slash.
    pub stem: String,
}

impl PathQuery {
    /// The path the trimmed query is, or `None` when it does not begin with `~` or `/`.
    pub fn parse(query: &str) -> Option<PathQuery> {
        let raw = query.trim();
        if !raw.starts_with(['~', '/']) {
            return None;
        }
        let (base, stem) = match raw.rfind('/') {
            None => ("~".to_owned(), String::new()),
            Some(0) => ("/".to_owned(), raw[1..].to_owned()),
            Some(cut) => (raw[..cut].to_owned(), raw[cut + 1..].to_owned()),
        };
        Some(PathQuery { base, stem })
    }

    /// The base with its slash: what an entry's path begins with.
    pub fn prefix(&self) -> String {
        if self.base == "/" {
            "/".to_owned()
        } else {
            format!("{}/", self.base)
        }
    }

    /// The path of an entry of the directory.
    pub fn path_of(&self, name: &str) -> String {
        format!("{}{name}", self.prefix())
    }

    /// Whether an entry stays: its name begins with the stem, case and accents aside.
    pub fn accepts(&self, name: &str) -> bool {
        Folded::new(name).starts_with(&word(&self.stem))
    }

    /// The letters of an entry's path to mark: the directory and the stem typed so far.
    pub fn marks(&self) -> Vec<usize> {
        (0..self.prefix().chars().count() + self.stem.chars().count()).collect()
    }

    /// What ⇥ puts in the field for an entry's path: the path and a slash, to look inside.
    pub fn complete(path: &str) -> String {
        if path.ends_with('/') {
            path.to_owned()
        } else {
            format!("{path}/")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(query: &str) -> (String, String) {
        let q = PathQuery::parse(query).expect("a path");
        (q.base, q.stem)
    }

    #[test]
    fn only_a_tilde_or_a_slash_makes_a_path() {
        assert_eq!(PathQuery::parse("split"), None);
        assert_eq!(PathQuery::parse(""), None);
        assert_eq!(PathQuery::parse("  Projects/api"), None);
        assert!(PathQuery::parse("  ~/x").is_some());
    }

    #[test]
    fn the_query_is_cut_at_its_last_slash() {
        assert_eq!(cut("~"), ("~".into(), "".into()));
        assert_eq!(cut("~/"), ("~".into(), "".into()));
        assert_eq!(cut("~/Pro"), ("~".into(), "Pro".into()));
        assert_eq!(cut("~/Projects/"), ("~/Projects".into(), "".into()));
        assert_eq!(cut("~/Projects/we"), ("~/Projects".into(), "we".into()));
        assert_eq!(cut("/"), ("/".into(), "".into()));
        assert_eq!(cut("/us"), ("/".into(), "us".into()));
        assert_eq!(cut("/usr/lo"), ("/usr".into(), "lo".into()));
    }

    #[test]
    fn entries_are_named_and_kept_by_their_stem() {
        let q = PathQuery::parse("~/pro").unwrap();
        assert_eq!(q.prefix(), "~/");
        assert_eq!(q.path_of("Projects"), "~/Projects");
        assert!(q.accepts("Projects"));
        assert!(q.accepts("PRÓJECTS"));
        assert!(!q.accepts("Documents"));
        let root = PathQuery::parse("/u").unwrap();
        assert_eq!(root.path_of("usr"), "/usr");
        assert_eq!(root.marks(), vec![0, 1]);
        assert_eq!(q.marks().len(), "~/pro".len());
        assert_eq!(PathQuery::complete("~/Projects"), "~/Projects/");
        assert_eq!(PathQuery::complete("/"), "/");
    }
}
