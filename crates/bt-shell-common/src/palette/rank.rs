//! Ordering what a query found: scores, nearness, clusters.
//!
//! A row's place comes from its **score** ([`super::matching`]) and, between rows that score
//! alike, from how **near** it is to the user: at the root a row in Recent gains
//! [`RECENT_BONUS`]; among tabs the nth most recently visited gains
//! `max(0, `[`VISIT_BONUS`]` − n · `[`VISIT_STEP`]`)`. The bonus is added to the score, so a
//! near row also beats a slightly better match — "api" typed after "ap" does not jump over the
//! tab just left.
//!
//! In a root search the hits are **clustered** by what they are (commands, settings, themes …),
//! at most [`CLUSTER_LIMIT`] rows each ([`THEME_LIMIT`] for themes), the clusters ordered by
//! their best row. Two fallbacks close every root search, whatever it found: look the query up
//! in the pane, look it up in the command history.

use super::matching::{Match, score};
use super::row::{Kind, Row};

/// Added to the score of a row in Recent, at the root.
pub const RECENT_BONUS: f64 = 12.0;
/// Added to the score of the most recently visited tab.
pub const VISIT_BONUS: f64 = 8.0;
/// Taken off per place further from the most recent visit.
pub const VISIT_STEP: f64 = 1.5;
/// Rows a cluster shows.
pub const CLUSTER_LIMIT: usize = 6;
/// Rows the themes' cluster shows: there are many and they differ only by taste.
pub const THEME_LIMIT: usize = 8;

/// A row the query found.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub row: Row,
    pub score: f64,
    /// The title's letters to mark.
    pub marks: Vec<usize>,
}

/// Hits of one kind of row under their heading.
#[derive(Clone, Debug, PartialEq)]
pub struct Cluster {
    pub title: String,
    pub hits: Vec<Hit>,
}

/// What brings a row nearer than its score says.
#[derive(Clone, Copy, Debug)]
pub enum Nearness<'a> {
    /// Nothing: the score alone.
    None,
    /// The ids in Recent, the latest first.
    Recent(&'a [String]),
    /// The tab rows are in the order the tabs were visited, the latest first.
    Visits,
}

/// Every row the query finds, best first. An empty query finds every row, in the order given.
/// Ties keep the order given.
pub fn search(rows: &[Row], query: &str, nearness: Nearness<'_>) -> Vec<Hit> {
    let mut tab_place = 0usize;
    let mut hits: Vec<Hit> = rows
        .iter()
        .filter_map(|row| {
            // A tab's place counts among **all** the tabs, found or not: the bonus is how
            // recently it was visited, not how many matched before it.
            let place = (row.kind == Kind::Tab).then(|| {
                tab_place += 1;
                tab_place - 1
            });
            let Match { score: base, marks } = score(row, query)?;
            let bonus = match (nearness, place) {
                (Nearness::Recent(ids), _) if ids.contains(&row.id) => RECENT_BONUS,
                (Nearness::Visits, Some(place)) => {
                    (VISIT_BONUS - place as f64 * VISIT_STEP).max(0.0)
                }
                _ => 0.0,
            };
            Some(Hit {
                row: row.clone(),
                score: base + bonus,
                marks,
            })
        })
        .collect();
    if !query.trim().is_empty() {
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    }
    hits
}

/// Hits grouped under the heading of their kind: the clusters ordered by their best hit, the
/// hits in a cluster by score, each cluster cut to its limit. Rows of a kind with no heading are
/// left out.
pub fn cluster(hits: Vec<Hit>) -> Vec<Cluster> {
    let mut clusters: Vec<Cluster> = Vec::new();
    for hit in hits {
        let title = hit.row.kind.cluster();
        if title.is_empty() {
            continue;
        }
        match clusters.iter_mut().find(|c| c.title == title) {
            Some(cluster) => cluster.hits.push(hit),
            None => clusters.push(Cluster {
                title: title.to_owned(),
                hits: vec![hit],
            }),
        }
    }
    for cluster in &mut clusters {
        cluster.hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        let limit = if cluster.title == Kind::Theme.cluster() {
            THEME_LIMIT
        } else {
            CLUSTER_LIMIT
        };
        cluster.hits.truncate(limit);
    }
    clusters.sort_by(|a, b| best(b).total_cmp(&best(a)));
    clusters
}

fn best(cluster: &Cluster) -> f64 {
    cluster.hits.first().map_or(f64::MIN, |hit| hit.score)
}

/// The root search: `query` over every row, Recent nearer, clustered, and closed by the two
/// fallbacks. The caller handles an empty query and a path (`~/…`) before this.
pub fn root(rows: &[Row], query: &str, recent: &[String]) -> Vec<Cluster> {
    let mut clusters = cluster(search(rows, query, Nearness::Recent(recent)));
    clusters.push(fallbacks(query));
    clusters
}

/// The tab page's search: tabs by score and visit order, groups beside them, the cluster with
/// the better best row first.
pub fn tabs(rows: &[Row], query: &str) -> Vec<Cluster> {
    cluster(search(rows, query, Nearness::Visits))
}

/// "Find … in This Pane" and "Search Command History for …", for the trimmed query.
pub fn fallbacks(query: &str) -> Cluster {
    let query = query.trim();
    let find = Row::new(Kind::Search, "find", format!("Find “{query}” in This Pane"));
    let history = Row::new(
        Kind::Search,
        "history",
        format!("Search Command History for “{query}”"),
    );
    Cluster {
        title: Kind::Search.cluster().to_owned(),
        hits: [find, history]
            .into_iter()
            .map(|row| Hit {
                row,
                score: 0.0,
                marks: Vec::new(),
            })
            .collect(),
    }
}

/// The rows Recent names, in its order, taken from `pool`; an id no row answers to (a command
/// the menu no longer has) is dropped.
pub fn recent_rows(recent: &[String], pool: &[Row]) -> Vec<Row> {
    recent
        .iter()
        .filter_map(|id| pool.iter().find(|row| &row.id == id).cloned())
        .collect()
}
