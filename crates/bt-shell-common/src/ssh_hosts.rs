//! The host names of `~/.ssh/config` → what the command palette lists as servers. **Pure
//! parsing, plus one bounded walk over the files that `Include` names**; AppKit-free.
//!
//! Only names a person would type after `ssh` are listed: a `Host` pattern with a `*` or a `?`
//! stands for many hosts and a `!` pattern excludes one, so none of the three is a server.
//! `HostName`, `User` and the rest are not read — `ssh db-1` already resolves them, and the
//! palette writes that line, it never connects.
//!
//! The file is the user's, so the reader is bounded on every side (depth, file count, bytes per
//! file, host count): a config that includes itself, or a huge generated one, costs a short
//! list, never a stall. A file that cannot be read is skipped without a message — the palette
//! has nowhere to put one, and the other sources still list.
//!
//! **Known limits:** `Match` blocks name no host; an `Include` wildcard works in the last path
//! component only (`~/.ssh/conf.d/*`), which is the form people write; `%`-tokens in an
//! `Include` path are not expanded.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The most hosts the walk returns.
pub const MAX_HOSTS: usize = 400;
/// How deep `Include` is followed (the config itself is depth 0).
pub const MAX_DEPTH: usize = 4;
/// The most files the walk opens, counting the config itself.
pub const MAX_FILES: usize = 64;
/// The most bytes read from one file; the rest is ignored.
pub const MAX_FILE_BYTES: u64 = 1 << 20;

/// One thing a config line says that the palette cares about, in file order — the order is
/// the list's order, and an `Include` is read at the line that names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A concrete host name (no wildcard, not negated).
    Host(String),
    /// A path pattern of an `Include` line, as written.
    Include(String),
}

/// Reads the entries out of one config file's text.
///
/// Keywords are case-insensitive and may be followed by `=` (`Host=db`, `Host = db`); arguments
/// are whitespace-separated, a double-quoted argument keeps its spaces, and an unquoted `#`
/// starts a comment that runs to the end of the line.
pub fn parse(text: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, rest) = split_keyword(line);
        if keyword.eq_ignore_ascii_case("host") {
            for pattern in arguments(rest) {
                if is_plain_host(&pattern) {
                    entries.push(Entry::Host(pattern));
                }
            }
        } else if keyword.eq_ignore_ascii_case("include") {
            for pattern in arguments(rest) {
                entries.push(Entry::Include(pattern));
            }
        }
    }
    entries
}

/// A `Host` pattern that names one machine: no `*`/`?`, not an exclusion, and a name that can be
/// typed after `ssh` ([`is_typeable`]).
fn is_plain_host(pattern: &str) -> bool {
    !pattern.starts_with('!') && !pattern.contains(['*', '?']) && is_typeable(pattern)
}

/// Whether `name` is something the palette may write after `ssh` on a line: not empty, no
/// whitespace or control character (a newline would end the line), and no leading `-` (ssh would
/// read it as an option, `-oProxyCommand=…`). Every host the palette lists passes this, whatever
/// its source.
pub fn is_typeable(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// `Keyword args` / `Keyword=args` / `Keyword = args` → (keyword, args).
fn split_keyword(line: &str) -> (&str, &str) {
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let (keyword, rest) = line.split_at(end);
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest);
    (keyword, rest.trim_start())
}

/// The arguments of a line: whitespace-separated, `"…"` keeps spaces, an unquoted `#` ends the
/// line.
fn arguments(rest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        if first == '#' {
            break;
        }
        let mut word = String::new();
        let mut quoted = false;
        while let Some(&c) = chars.peek() {
            if c == '"' {
                quoted = !quoted;
                chars.next();
            } else if c.is_whitespace() && !quoted {
                break;
            } else {
                word.push(c);
                chars.next();
            }
        }
        out.push(word);
    }
    out
}

/// Where an `Include` pattern points: `~/…` from the home directory, an absolute path as it is,
/// anything else relative to `~/.ssh` (OpenSSH's rule for a user's config). The result may
/// still carry a wildcard in its last component ([`expand`]).
pub fn include_path(pattern: &str, home: &Path) -> PathBuf {
    if let Some(rest) = pattern.strip_prefix("~/") {
        home.join(rest)
    } else if pattern.starts_with('/') {
        PathBuf::from(pattern)
    } else {
        home.join(".ssh").join(pattern)
    }
}

/// `*` (any run) and `?` (one character) against a file name — the glob the last component of
/// an `Include` takes. Case-sensitive, like the file system the config lives on.
pub fn wildcard_matches(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|skip| go(&p[1..], &n[skip..])),
            Some('?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some(c) => n.first() == Some(c) && go(&p[1..], &n[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n)
}

/// The files an `Include` pattern names: itself when it has no wildcard, else the regular
/// files of its directory whose names match, sorted (glob(3)'s order). A directory that cannot
/// be listed gives none.
fn expand(path: &Path) -> Vec<PathBuf> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return vec![path.to_path_buf()];
    };
    if !name.contains(['*', '?']) {
        return vec![path.to_path_buf()];
    }
    let Some(dir) = path.parent() else {
        return Vec::new();
    };
    let Ok(listing) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = listing
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| !n.starts_with('.') && wildcard_matches(name, n))
        })
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    files
}

/// The host names of `~/.ssh/config` and the files it includes, in file order, each name once.
/// Empty when the file is missing or unreadable. Does blocking file reads: call it off the main
/// thread.
pub fn read_hosts(home: &Path) -> Vec<String> {
    let mut walk = Walk {
        home,
        seen: HashSet::new(),
        hosts: Vec::new(),
        files: 0,
    };
    walk.file(&home.join(".ssh").join("config"), 0);
    walk.hosts
}

struct Walk<'a> {
    home: &'a Path,
    seen: HashSet<String>,
    hosts: Vec<String>,
    files: usize,
}

impl Walk<'_> {
    fn file(&mut self, path: &Path, depth: usize) {
        use std::io::Read as _;
        if self.files >= MAX_FILES || self.hosts.len() >= MAX_HOSTS {
            return;
        }
        let Ok(file) = std::fs::File::open(path) else {
            return;
        };
        self.files += 1;
        let mut bytes = Vec::new();
        if file.take(MAX_FILE_BYTES).read_to_end(&mut bytes).is_err() {
            return;
        }
        let text = String::from_utf8_lossy(&bytes);
        for entry in parse(&text) {
            match entry {
                Entry::Host(name) => {
                    if self.hosts.len() < MAX_HOSTS && self.seen.insert(name.clone()) {
                        self.hosts.push(name);
                    }
                }
                Entry::Include(pattern) if depth < MAX_DEPTH => {
                    for included in expand(&include_path(&pattern, self.home)) {
                        self.file(&included, depth + 1);
                    }
                }
                Entry::Include(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(text: &str) -> Vec<String> {
        parse(text)
            .into_iter()
            .filter_map(|e| match e {
                Entry::Host(h) => Some(h),
                Entry::Include(_) => None,
            })
            .collect()
    }

    #[test]
    fn plain_hosts_are_listed_and_patterns_are_not() {
        let text = "Host db-1 db-2\n  HostName 10.0.0.5\nHost *\n  ForwardAgent no\n\
                    Host *.internal web-?\nHost !bastion staging\n";
        assert_eq!(hosts(text), ["db-1", "db-2", "staging"]);
    }

    #[test]
    fn the_keyword_is_case_insensitive_and_takes_an_equals_sign() {
        let text = "host lower\nHOST UPPER\nHost=eq\nHost = spaced\n\tHost\ttabbed\n";
        assert_eq!(hosts(text), ["lower", "UPPER", "eq", "spaced", "tabbed"]);
    }

    #[test]
    fn comments_and_quotes_are_read_like_the_file_means_them() {
        let text = "# Host hidden\nHost shown # Host trailing\nHost \"quoted name\" plain\n";
        // A name with a space cannot be typed after `ssh`, so it is not listed.
        assert_eq!(hosts(text), ["shown", "plain"]);
    }

    #[test]
    fn a_name_ssh_would_read_as_an_option_or_that_ends_the_line_is_not_a_host() {
        assert!(is_typeable("db-1"));
        assert!(is_typeable("deploy@db-1"));
        assert!(!is_typeable("-oProxyCommand=x"));
        assert!(!is_typeable("a\nb"));
        assert!(!is_typeable(""));
        assert_eq!(hosts("Host -oProxyCommand=x ok\n"), ["ok"]);
    }

    #[test]
    fn match_blocks_name_no_host_and_a_value_called_host_is_not_a_host() {
        let text = "Match host db-1\n  User root\nHostName host.example.com\nHost real\n";
        assert_eq!(hosts(text), ["real"]);
    }

    #[test]
    fn includes_come_back_in_place_with_their_patterns_as_written() {
        let entries = parse("Host a\nInclude conf.d/* ~/other\nHost b\ninclude=\"x y\"\n");
        assert_eq!(
            entries,
            [
                Entry::Host("a".into()),
                Entry::Include("conf.d/*".into()),
                Entry::Include("~/other".into()),
                Entry::Host("b".into()),
                Entry::Include("x y".into()),
            ]
        );
    }

    #[test]
    fn an_include_is_relative_to_the_ssh_directory_unless_it_says_otherwise() {
        let home = Path::new("/Users/a");
        assert_eq!(
            include_path("conf.d/*", home),
            Path::new("/Users/a/.ssh/conf.d/*")
        );
        assert_eq!(
            include_path("~/work/ssh", home),
            Path::new("/Users/a/work/ssh")
        );
        assert_eq!(include_path("/etc/ssh/x", home), Path::new("/etc/ssh/x"));
    }

    #[test]
    fn the_wildcard_of_an_include_matches_whole_names() {
        assert!(wildcard_matches("*", "anything"));
        assert!(wildcard_matches("*.conf", "work.conf"));
        assert!(!wildcard_matches("*.conf", "work.conf.bak"));
        assert!(wildcard_matches("w?rk", "work"));
        assert!(!wildcard_matches("w?rk", "wrk"));
        assert!(wildcard_matches("a*b*c", "aXXbYYc"));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bt-ssh-hosts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ssh/conf.d")).unwrap();
        dir
    }

    #[test]
    fn the_walk_follows_includes_in_order_and_lists_each_name_once() {
        let home = scratch("walk");
        std::fs::write(
            home.join(".ssh/config"),
            "Host first\nInclude conf.d/*\nHost last first\n",
        )
        .unwrap();
        std::fs::write(home.join(".ssh/conf.d/b.conf"), "Host from-b\n").unwrap();
        std::fs::write(home.join(".ssh/conf.d/a.conf"), "Host from-a first\n").unwrap();
        std::fs::write(home.join(".ssh/conf.d/.hidden"), "Host hidden\n").unwrap();
        assert_eq!(read_hosts(&home), ["first", "from-a", "from-b", "last"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_config_that_includes_itself_stops_at_the_depth_bound() {
        let home = scratch("loop");
        std::fs::write(home.join(".ssh/config"), "Host loop\nInclude config\n").unwrap();
        assert_eq!(read_hosts(&home), ["loop"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_missing_config_lists_nothing() {
        let home = scratch("missing");
        assert!(read_hosts(&home).is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }
}
