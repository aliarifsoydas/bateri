//! The active environment on the dock's context row: which Python
//! environment is active and which Node version runs here — `(.venv) 3.12`
//! at the row's head, `node 22.11` after the branch.
//!
//! **The shell reports, this reads.** At every prompt the shell sends its
//! variables with the branch (`bt_core::ShellEnvironment`: `VIRTUAL_ENV`,
//! `VIRTUAL_ENV_PROMPT`, `CONDA_PREFIX`, `CONDA_PROMPT_MODIFIER` and the path
//! of the `node` it would run); [`resolve`] turns them, with the directory,
//! into the row's text (`bt_core::DockEnvironment`). It is the pane's
//! background job — a few small file reads — and runs only when the report
//! or the directory changed.
//!
//! **The names are the tools' own.** A virtual environment shows as its
//! `activate` puts it in front of the prompt (`VIRTUAL_ENV_PROMPT`, else its
//! folder's name), conda's as conda does (`CONDA_PROMPT_MODIFIER`): `(base)`
//! included, and nothing at all when the user told conda not to touch the
//! prompt (`changeps1`). Both, when both are active: `(.venv) (base)`, the
//! order a prompt shows them in. The version is the Python that runs — the
//! virtual environment's when there is one.
//!
//! **Nothing is run.** The Node version is read from where the binary is
//! installed — a version folder in its real path (nvm, fnm, n, mise, asdf's
//! installs, Homebrew's Cellar) or the `node_version.h` installed beside it
//! (the official installer) — never from `node --version`: the `node` a
//! directory's `PATH` names can be the directory's own, and a `cd` must not
//! run what a repository put there. A shim that picks the version per
//! directory (volta's, asdf's and mise's shims) names none: the row shows no
//! version rather than a guess. Python's version comes from the
//! environment's own files (`pyvenv.cfg`, conda's package records).
//!
//! **Node only in a Node project**: a `package.json`, `.nvmrc` or
//! `.node-version` in the directory or above it, up to the repository's root
//! — never the home folder itself or `/`, where a stray `package.json` would
//! put Node on every row. The nearest `.nvmrc` or `.node-version` is what the
//! project asks for; a plain version in it (`22`, `v22.11.0`) is compared
//! with the one that runs, an alias (`lts/*`, `node`) is not. `engines.node`
//! in `package.json` is not read: a range is a semver question and the row's
//! warning would need a JSON and semver parser this crate does not carry.
//!
//! Every file is checked to be a regular file before it is opened (a FIFO
//! would block the open) and read up to a small bound.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use bt_core::{DockEnvironment, ShellEnvironment};

use crate::program::pyvenv_value;

/// What the row is set to show (`[shell] python_env`, `node_version`); a
/// part that is off is not read at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wants {
    pub python: bool,
    pub node: bool,
}

impl Wants {
    pub fn any(self) -> bool {
        self.python || self.node
    }
}

/// The longest `.nvmrc`, `.node-version` or `node_version.h` that is read.
const SMALL_FILE_MAX: u64 = 64 * 1024;

/// The longest wanted version shown: past it the file holds something else.
const WANTED_MAX: usize = 32;

/// The row's text for the shell's report made in `cwd`; `home` bounds the
/// Node project's search.
pub fn resolve(
    reported: &ShellEnvironment,
    cwd: &Path,
    home: Option<&Path>,
    wants: Wants,
) -> DockEnvironment {
    let mut shown = DockEnvironment::default();
    if wants.python {
        (shown.python, shown.python_version) = python(reported);
    }
    if wants.node
        && !reported.node.is_empty()
        && let Some(project) = node_project(cwd, home)
        && let Some(version) = node_version(Path::new(&reported.node))
    {
        shown.node = major_minor(&version);
        if let Some((file, wanted)) = project.wanted
            && !satisfies(&version, &wanted)
        {
            shown.node_wanted = format!("{file} {wanted}");
        }
    }
    shown
}

/// The active Python environments' names and the version of the Python that
/// runs; empty strings for none.
fn python(reported: &ShellEnvironment) -> (String, String) {
    let venv = (!reported.virtual_env.is_empty())
        .then(|| venv_name(&reported.virtual_env, &reported.virtual_env_prompt));
    let conda = conda_name(&reported.conda_prompt);
    let names: Vec<String> = venv.into_iter().chain(conda).collect();
    if names.is_empty() {
        return (String::new(), String::new());
    }
    // The virtual environment's Python is the one on `PATH` first.
    let version = if reported.virtual_env.is_empty() {
        conda_python_version(Path::new(&reported.conda_prefix))
    } else {
        venv_python_version(Path::new(&reported.virtual_env))
    };
    (names.join(" "), version.unwrap_or_default())
}

/// A virtual environment's name as `activate` shows it: `(name)` from
/// `VIRTUAL_ENV_PROMPT` — CPython before 3.12 writes it with its parentheses
/// and a space, after without, virtualenv and uv without —, else from its
/// folder.
pub fn venv_name(virtual_env: &str, prompt: &str) -> String {
    let prompt = prompt.trim();
    let bare = prompt
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or(prompt)
        .trim();
    let name = if bare.is_empty() {
        Path::new(virtual_env)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        bare.to_owned()
    };
    clean(&format!("({name})"))
}

/// conda's prompt modifier as conda would print it (`(base)`); `None` when
/// conda says nothing.
pub fn conda_name(modifier: &str) -> Option<String> {
    let modifier = clean(modifier.trim());
    (!modifier.is_empty()).then_some(modifier)
}

/// The text without control characters: the names come from the shell's
/// variables, and the row would draw a box for one.
fn clean(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_control()).collect()
}

/// A virtual environment's Python version: its `pyvenv.cfg`
/// (`version_info` from virtualenv and uv, `version` from CPython's `venv`),
/// else the `lib/pythonX.Y` it carries.
fn venv_python_version(root: &Path) -> Option<String> {
    read_regular_file(&root.join("pyvenv.cfg"), SMALL_FILE_MAX)
        .and_then(|text| python_version_of_config(&text))
        .or_else(|| lib_python_version(root))
}

/// The `major.minor` of `pyvenv.cfg`'s version.
pub fn python_version_of_config(text: &str) -> Option<String> {
    ["version_info", "version"]
        .into_iter()
        .find_map(|key| pyvenv_value(text, key))
        .and_then(version_numbers)
        .map(|(major, minor, _)| format!("{major}.{minor}"))
}

/// A conda environment's Python version: the `python-X.Y.Z-….json` record
/// conda keeps of the package, else the `lib/pythonX.Y` it carries.
fn conda_python_version(prefix: &Path) -> Option<String> {
    if prefix.as_os_str().is_empty() {
        return None;
    }
    let records = std::fs::read_dir(prefix.join("conda-meta")).ok();
    records
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .find_map(|entry| conda_record_version(&entry.file_name().to_string_lossy()))
        .or_else(|| lib_python_version(prefix))
}

/// `python-3.11.9-h99e199e_0.json` → `3.11`; another package's record
/// (`python-dateutil-…`, `python_abi-…`) → `None`.
pub fn conda_record_version(name: &str) -> Option<String> {
    let rest = name.strip_prefix("python-")?.strip_suffix(".json")?;
    let (major, minor, _) = version_numbers(rest)?;
    Some(format!("{major}.{minor}"))
}

/// `lib/python3.12` under an environment's root → `3.12`.
fn lib_python_version(root: &Path) -> Option<String> {
    std::fs::read_dir(root.join("lib"))
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let (major, minor) = name.strip_prefix("python")?.split_once('.')?;
            let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
            (digits(major) && digits(minor)).then(|| format!("{major}.{minor}"))
        })
}

/// The Node project around `cwd`, if there is one.
#[derive(Debug, PartialEq, Eq)]
struct Project {
    /// The nearest `.nvmrc` or `.node-version`: its name and the version it
    /// asks for.
    wanted: Option<(&'static str, String)>,
}

/// The files that make a directory a Node project's; the version files say
/// what it asks for, in the order version managers read them.
const VERSION_FILES: [&str; 2] = [".nvmrc", ".node-version"];

/// Walks from `cwd` up to the repository's root (the directory with `.git`),
/// never into `home` itself or `/`.
fn node_project(cwd: &Path, home: Option<&Path>) -> Option<Project> {
    if !cwd.is_absolute() {
        return None;
    }
    let mut found = false;
    let mut wanted = None;
    for dir in cwd.ancestors() {
        if dir.parent().is_none() || Some(dir) == home {
            break;
        }
        found |= is_file(&dir.join("package.json"));
        for name in VERSION_FILES {
            if wanted.is_none() {
                let path = dir.join(name);
                if is_file(&path) {
                    found = true;
                    wanted = read_regular_file(&path, SMALL_FILE_MAX)
                        .and_then(|text| wanted_version(&text))
                        .map(|version| (name, version));
                }
            }
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    found.then_some(Project { wanted })
}

/// What a `.nvmrc` or `.node-version` asks for: its first line without a
/// comment and without a leading `v` (`v22.11.0` → `22.11.0`); `None` for an
/// empty or unreadable file.
pub fn wanted_version(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .find(|line| !line.is_empty())?;
    let line = line.strip_prefix('v').unwrap_or(line);
    (line.chars().count() <= WANTED_MAX && !line.chars().any(char::is_control))
        .then(|| line.to_owned())
}

/// Whether the `actual` version (`22.11.0`) is what `wanted` asks for: every
/// number `wanted` names matches (`22`, `22.11`, `22.11.0`). An alias or a
/// range (`lts/iron`, `node`, `>=18`) is not compared: it says nothing this
/// side can check, and a warning has to be right.
pub fn satisfies(actual: &str, wanted: &str) -> bool {
    let numbers: Option<Vec<u64>> = wanted.split('.').map(|part| part.parse().ok()).collect();
    let Some(numbers) = numbers else {
        return true;
    };
    let Some((major, minor, patch)) = version_numbers(actual) else {
        return true;
    };
    numbers
        .iter()
        .zip([major, minor, patch])
        .all(|(wanted, actual)| *wanted == actual)
}

/// The version of the `node` at `path` (`22.11.0`), from where it is
/// installed; `None` when that does not say.
fn node_version(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    let real = std::fs::canonicalize(path).ok()?;
    if !is_file(&real) {
        return None;
    }
    node_version_in_path(&real).or_else(|| {
        let prefix = real.parent()?.parent()?;
        read_regular_file(&prefix.join("include/node/node_version.h"), SMALL_FILE_MAX)
            .and_then(|text| node_version_of_header(&text))
    })
}

/// A version folder in an installed `node`'s real path, right under a
/// folder named for Node: nvm's `versions/node/v22.11.0`, fnm's
/// `node-versions/v22.11.0`, n's and mise's `node/22.11.0`, asdf's
/// `nodejs/22.11.0`, Homebrew's `Cellar/node/22.11.0` or
/// `Cellar/node@20/20.18.0_1`. Near the binary only: a numbered folder
/// higher up is someone's project.
pub fn node_version_in_path(path: &Path) -> Option<String> {
    let names: Vec<String> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    names
        .windows(2)
        .rev()
        .take(4)
        .find_map(|pair| {
            let [above, folder] = pair else {
                return None;
            };
            let folder = folder.strip_prefix('v').unwrap_or(folder);
            above.starts_with("node").then(|| version_numbers(folder))?
        })
        .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
}

/// The version in Node's `node_version.h`
/// (`#define NODE_MAJOR_VERSION 22` and its minor and patch lines).
pub fn node_version_of_header(text: &str) -> Option<String> {
    let value = |name: &str| {
        text.lines().find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some("#define") && words.next() == Some(name))
                .then(|| words.next()?.parse::<u64>().ok())?
        })
    };
    Some(format!(
        "{}.{}.{}",
        value("NODE_MAJOR_VERSION")?,
        value("NODE_MINOR_VERSION")?,
        value("NODE_PATCH_VERSION")?
    ))
}

/// `22.11.0` → `22.11`.
fn major_minor(version: &str) -> String {
    version_numbers(version)
        .map(|(major, minor, _)| format!("{major}.{minor}"))
        .unwrap_or_default()
}

/// The leading `major.minor[.patch]` of a version (`3.12.4.final.0`,
/// `20.18.0_1`, `3.12`); the patch is `0` when absent. `None` unless it
/// starts with two numbers.
fn version_numbers(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.split('.');
    let number = |part: Option<&str>| -> Option<u64> {
        let digits: String = part?.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    };
    let major = number(parts.next())?;
    let minor = number(parts.next())?;
    let patch = number(parts.next()).unwrap_or(0);
    Some((major, minor, patch))
}

fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// A regular file's text, at most `max` bytes read; `None` for anything
/// else. The type is checked before opening: a FIFO would block the open.
pub(crate) fn read_regular_file(path: &Path, max: u64) -> Option<String> {
    if !is_file(path) {
        return None;
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(max)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// The home directory the Node project's search stops at.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute() && home.parent().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::TempRoot;

    const ALL: Wants = Wants {
        python: true,
        node: true,
    };

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("dir");
        std::fs::write(path, text).expect("write");
    }

    fn report(fields: [&str; 5]) -> ShellEnvironment {
        let [
            virtual_env,
            virtual_env_prompt,
            conda_prefix,
            conda_prompt,
            node,
        ] = fields;
        ShellEnvironment {
            virtual_env: virtual_env.into(),
            virtual_env_prompt: virtual_env_prompt.into(),
            conda_prefix: conda_prefix.into(),
            conda_prompt: conda_prompt.into(),
            node: node.into(),
        }
    }

    #[test]
    fn a_virtual_environment_is_named_as_its_activate_names_it() {
        // CPython before 3.12 keeps the parentheses and the space, after
        // without; virtualenv and uv without; an unset prompt is the folder.
        assert_eq!(venv_name("/p/.venv", "(.venv) "), "(.venv)");
        assert_eq!(venv_name("/p/.venv", ".venv"), "(.venv)");
        assert_eq!(venv_name("/p/.venv", "api-py3.12"), "(api-py3.12)");
        assert_eq!(venv_name("/home/me/.virtualenvs/api", ""), "(api)");
        assert_eq!(venv_name("/p/.venv", "(.ve\u{1b}nv)"), "(.venv)");
        // conda's is printed as conda prints it, and nothing when it says nothing.
        assert_eq!(conda_name("(base) ").as_deref(), Some("(base)"));
        assert_eq!(conda_name(""), None);
        assert_eq!(conda_name("  "), None);
    }

    #[test]
    fn the_python_version_comes_from_the_environments_own_files() {
        assert_eq!(
            python_version_of_config("home = /usr/bin\nversion = 3.12.4\n").as_deref(),
            Some("3.12")
        );
        assert_eq!(
            python_version_of_config("home = /x\nversion_info = 3.11.9.final.0\n").as_deref(),
            Some("3.11")
        );
        assert_eq!(python_version_of_config("home = /x\n"), None);
        assert_eq!(
            conda_record_version("python-3.11.9-h99e199e_0.json").as_deref(),
            Some("3.11")
        );
        assert_eq!(
            conda_record_version("python-dateutil-2.9.0-pyhd8ed1ab_0.json"),
            None
        );
        assert_eq!(conda_record_version("python_abi-3.11-4_cp311.json"), None);

        let root = TempRoot::new("environment-python");
        let venv = root.0.join("api/.venv");
        write(
            &venv.join("pyvenv.cfg"),
            "home = /usr/bin\nversion = 3.12.4\n",
        );
        let conda = root.0.join("miniconda3");
        write(&conda.join("conda-meta/python-3.11.9-h0_0.json"), "{}");
        let bare = root.0.join("bare");
        std::fs::create_dir_all(bare.join("lib/python3.10")).expect("lib");

        let venv_path = venv.to_string_lossy().into_owned();
        let conda_path = conda.to_string_lossy().into_owned();
        let bare_path = bare.to_string_lossy().into_owned();
        let shown = |fields| python(&report(fields));
        assert_eq!(
            shown([&venv_path, ".venv", "", "", ""]),
            ("(.venv)".into(), "3.12".into())
        );
        assert_eq!(
            shown(["", "", &conda_path, "(base) ", ""]),
            ("(base)".into(), "3.11".into())
        );
        // Both: the order a prompt shows them in, the venv's Python.
        assert_eq!(
            shown([&venv_path, ".venv", &conda_path, "(base) ", ""]),
            ("(.venv) (base)".into(), "3.12".into())
        );
        // conda told not to touch the prompt: nothing of its own shows.
        assert_eq!(
            shown(["", "", &conda_path, "", ""]),
            (String::new(), String::new())
        );
        // No pyvenv.cfg: the lib folder it carries.
        assert_eq!(
            shown([&bare_path, "", "", "", ""]),
            ("(bare)".into(), "3.10".into())
        );
        // A gone environment keeps its name and has no version.
        assert_eq!(
            shown(["/nowhere/.venv", "", "", "", ""]),
            ("(.venv)".into(), String::new())
        );
    }

    #[test]
    fn the_node_version_is_read_from_where_node_is_installed() {
        let path = |text: &str| node_version_in_path(Path::new(text));
        assert_eq!(
            path("/Users/me/.nvm/versions/node/v22.23.3/bin/node").as_deref(),
            Some("22.23.3")
        );
        assert_eq!(
            path("/Users/me/.local/share/fnm/node-versions/v20.18.0/installation/bin/node")
                .as_deref(),
            Some("20.18.0")
        );
        assert_eq!(
            path("/opt/homebrew/Cellar/node@20/20.18.0_1/bin/node").as_deref(),
            Some("20.18.0")
        );
        assert_eq!(
            path("/Users/me/.local/share/mise/installs/node/22.11.0/bin/node").as_deref(),
            Some("22.11.0")
        );
        assert_eq!(
            path("/Users/me/.asdf/installs/nodejs/18.20.4/bin/node").as_deref(),
            Some("18.20.4")
        );
        // A plain install, a shim and a numbered folder far up name none.
        assert_eq!(path("/usr/local/bin/node"), None);
        assert_eq!(path("/Users/me/.volta/bin/volta-shim"), None);
        assert_eq!(path("/Users/me/node/1.2.3/work/app/tools/x/bin/node"), None);

        let header = "#define NODE_MAJOR_VERSION 22\n#define NODE_MINOR_VERSION 11\n\
                      #define NODE_PATCH_VERSION 0\n";
        assert_eq!(node_version_of_header(header).as_deref(), Some("22.11.0"));
        assert_eq!(
            node_version_of_header("#define NODE_MAJOR_VERSION 22\n"),
            None
        );

        // The official installer: the header beside the binary's prefix.
        let root = TempRoot::new("environment-node");
        let prefix = root.0.join("usr/local");
        write(&prefix.join("bin/node"), "");
        write(&prefix.join("include/node/node_version.h"), header);
        assert_eq!(
            node_version(&prefix.join("bin/node")).as_deref(),
            Some("22.11.0")
        );
        // A relative path names nothing this side can find.
        assert_eq!(node_version(Path::new("node_modules/.bin/node")), None);
    }

    #[test]
    fn what_a_project_asks_for_is_compared_only_when_it_is_a_version() {
        assert_eq!(wanted_version("v22.11.0\n").as_deref(), Some("22.11.0"));
        assert_eq!(
            wanted_version("# pinned\n22 # lts\n").as_deref(),
            Some("22")
        );
        assert_eq!(wanted_version("lts/iron").as_deref(), Some("lts/iron"));
        assert_eq!(wanted_version("\n  \n"), None);
        assert!(satisfies("22.11.0", "22"));
        assert!(satisfies("22.11.0", "22.11"));
        assert!(satisfies("22.11.0", "22.11.0"));
        assert!(!satisfies("18.20.4", "22"));
        assert!(!satisfies("22.11.0", "22.12"));
        assert!(satisfies("18.20.4", "lts/iron"), "an alias is not compared");
        assert!(satisfies("18.20.4", ">=20"), "a range is not compared");
    }

    #[test]
    fn node_shows_only_in_a_node_project() {
        let root = TempRoot::new("environment-project");
        let home = root.0.join("home");
        let prefix = home.join(".nvm/versions/node/v18.20.4");
        write(&prefix.join("bin/node"), "");
        let node = prefix.join("bin/node").to_string_lossy().into_owned();
        let repo = home.join("Projects/studio");
        std::fs::create_dir_all(repo.join(".git")).expect("git");
        write(&repo.join(".nvmrc"), "22\n");
        write(&repo.join("packages/web/package.json"), "{}");
        let rust = home.join("Projects/bateri");
        std::fs::create_dir_all(rust.join(".git")).expect("git");
        // A stray package.json in the home folder is not a project.
        write(&home.join("package.json"), "{}");

        let at = |dir: &Path| {
            let shown = resolve(&report(["", "", "", "", &node]), dir, Some(&home), ALL);
            (shown.node, shown.node_wanted)
        };
        // Deep in a monorepo: the package is a project, the root's .nvmrc is
        // what it asks for, and the Node that runs is not that.
        assert_eq!(
            at(&repo.join("packages/web")),
            ("18.20".into(), ".nvmrc 22".into())
        );
        // The search stops at the repository's root and never reads the home folder.
        assert_eq!(at(&rust), (String::new(), String::new()));
        assert_eq!(at(&home), (String::new(), String::new()));
        // A project that asks for what runs gives no warning.
        write(&repo.join(".nvmrc"), "v18\n");
        assert_eq!(at(&repo), ("18.20".into(), String::new()));
        // Off is not read at all; no node on the shell's PATH shows nothing.
        let off = Wants {
            python: true,
            node: false,
        };
        assert!(resolve(&report(["", "", "", "", &node]), &repo, Some(&home), off).is_empty());
        assert!(resolve(&report(["", "", "", "", ""]), &repo, Some(&home), ALL).is_empty());
    }
}
