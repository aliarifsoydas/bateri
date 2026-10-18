//! The list on screen: the rows a query leaves on a page, under their headings.
//!
//! The state machine ([`super::Palette`]) knows the field and the highlight; the list is
//! derived from them after every call — [`root`] for the bottom of the stack, [`page`] for a
//! page of one thing — and handed back. A list is rows in order and the headings that stand
//! before some of them; the highlight indexes the rows, never a heading.

use super::rank::{Hit, Nearness, recent_rows, root as clustered, search};
use super::row::{Kind, Row};

/// The heading above Recent at the root.
pub const RECENT_HEADING: &str = "Recent";
/// The heading above the pages the empty root offers.
pub const GO_TO_HEADING: &str = "Go to";

/// What the closing "Find … in This Pane" / "Search Command History for …" rows may offer: a
/// fallback whose target does not exist yet is left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fallbacks {
    pub find: bool,
    pub history: bool,
}

impl Fallbacks {
    pub const ALL: Fallbacks = Fallbacks {
        find: true,
        history: true,
    };
}

/// The rows of a page and where its headings go.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct List {
    pub hits: Vec<Hit>,
    /// `(index of the first row under it, heading)`, ascending.
    pub headings: Vec<(usize, String)>,
}

impl List {
    /// The rows, for the state machine.
    pub fn rows(&self) -> Vec<Row> {
        self.hits.iter().map(|hit| hit.row.clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    pub fn len(&self) -> usize {
        self.hits.len()
    }

    /// The heading that stands right before row `index`.
    pub fn heading_before(&self, index: usize) -> Option<&str> {
        self.headings
            .iter()
            .find(|(at, _)| *at == index)
            .map(|(_, heading)| heading.as_str())
    }
}

/// The root: while the field is empty Recent and then the pages that are not a menu's (Themes…,
/// All Settings…); with a query, the clusters of what it finds with the fallbacks `offer` closing
/// them.
pub fn root(rows: &[Row], query: &str, recent: &[String], offer: Fallbacks) -> List {
    let mut list = List::default();
    if query.trim().is_empty() {
        let recent = recent_rows(recent, rows);
        if !recent.is_empty() {
            list.headings.push((0, RECENT_HEADING.to_owned()));
        }
        let pages: Vec<Row> = rows
            .iter()
            .filter(|row| {
                row.kind == Kind::Scope
                    && !row.id.starts_with("scope.menu.")
                    && !recent.iter().any(|seen| seen.id == row.id)
            })
            .cloned()
            .collect();
        list.hits = recent.into_iter().map(unscored).collect();
        if !pages.is_empty() {
            list.headings
                .push((list.hits.len(), GO_TO_HEADING.to_owned()));
            list.hits.extend(pages.into_iter().map(unscored));
        }
        return list;
    }
    for cluster in clustered(rows, query, recent) {
        let is_fallback = cluster.title == Kind::Search.cluster();
        let hits: Vec<Hit> = cluster
            .hits
            .into_iter()
            .filter(|hit| {
                !is_fallback
                    || match hit.row.id.as_str() {
                        "search.find" => offer.find,
                        "search.history" => offer.history,
                        _ => true,
                    }
            })
            .collect();
        if hits.is_empty() {
            continue;
        }
        list.headings.push((list.hits.len(), cluster.title));
        list.hits.extend(hits);
    }
    list
}

/// A page of one thing: the rows the query finds, best first, no headings.
pub fn page(rows: &[Row], query: &str) -> List {
    List {
        hits: search(rows, query, Nearness::None),
        headings: Vec::new(),
    }
}

fn unscored(row: Row) -> Hit {
    Hit {
        row,
        score: 0.0,
        marks: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<Row> {
        [
            ("newTab", "New Tab"),
            ("splitRight", "Split Right"),
            ("splitDown", "Split Down"),
            ("clearToStart", "Clear to Start"),
        ]
        .into_iter()
        .map(|(selector, title)| Row::new(Kind::Command, selector, title))
        .collect()
    }

    #[test]
    fn an_empty_field_lists_recent_under_its_heading() {
        let recent = vec!["cmd.splitDown".to_owned(), "cmd.gone".to_owned()];
        let list = root(&rows(), "  ", &recent, Fallbacks::ALL);
        assert_eq!(list.len(), 1);
        assert_eq!(list.hits[0].row.id, "cmd.splitDown");
        assert_eq!(list.heading_before(0), Some(RECENT_HEADING));
        assert_eq!(list.heading_before(1), None);
        assert!(root(&rows(), "", &[], Fallbacks::ALL).is_empty());
    }

    #[test]
    fn an_empty_field_also_offers_the_pages_that_are_not_a_menus_once() {
        let mut pool = rows();
        pool.push(Row::new(Kind::Scope, "themes", "Themes…"));
        pool.push(Row::new(Kind::Scope, "settings", "All Settings…"));
        pool.push(Row::new(Kind::Scope, "menu.Edit", "Edit"));
        let list = root(&pool, "", &[], Fallbacks::ALL);
        let ids: Vec<_> = list.hits.iter().map(|hit| hit.row.id.as_str()).collect();
        assert_eq!(ids, ["scope.themes", "scope.settings"]);
        assert_eq!(list.heading_before(0), Some(GO_TO_HEADING));
        // A page Recent already shows is not listed twice.
        let recent = vec!["scope.themes".to_owned()];
        let list = root(&pool, "", &recent, Fallbacks::ALL);
        let ids: Vec<_> = list.hits.iter().map(|hit| hit.row.id.as_str()).collect();
        assert_eq!(ids, ["scope.themes", "scope.settings"]);
        assert_eq!(list.heading_before(0), Some(RECENT_HEADING));
        assert_eq!(list.heading_before(1), Some(GO_TO_HEADING));
    }

    #[test]
    fn a_query_lists_clusters_and_closes_with_the_fallbacks_that_exist() {
        let list = root(&rows(), "spr", &[], Fallbacks::ALL);
        assert_eq!(list.hits[0].row.id, "cmd.splitRight");
        assert_eq!(list.heading_before(0), Some("Commands"));
        let ids: Vec<_> = list.hits.iter().map(|hit| hit.row.id.as_str()).collect();
        assert_eq!(&ids[ids.len() - 2..], ["search.find", "search.history"]);

        let find_only = root(
            &rows(),
            "spr",
            &[],
            Fallbacks {
                find: true,
                history: false,
            },
        );
        let ids: Vec<_> = find_only
            .hits
            .iter()
            .map(|hit| hit.row.id.as_str())
            .collect();
        assert_eq!(ids.last(), Some(&"search.find"));
        assert!(!ids.contains(&"search.history"));
    }

    #[test]
    fn a_query_that_finds_nothing_still_offers_the_fallbacks() {
        let list = root(&rows(), "zzzz", &[], Fallbacks::ALL);
        assert_eq!(list.len(), 2);
        assert_eq!(list.heading_before(0), Some("Search"));
    }

    #[test]
    fn a_page_is_the_rows_the_query_finds_without_headings() {
        let found = page(&rows(), "split");
        assert_eq!(found.len(), 2);
        assert!(found.headings.is_empty());
        assert_eq!(page(&rows(), "").len(), 4);
    }
}
