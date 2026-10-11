#!/usr/bin/env python3
"""Writes the palettes bateri adapts from other projects into Rust.

Usage (from the repo root):

    git clone https://github.com/mbadolato/iTerm2-Color-Schemes <dir>
    git -C <dir> checkout <COMMIT below>
    python3 tools/import_themes.py <dir> > crates/bt-core/src/color/schemes.rs

The source is iTerm2-Color-Schemes' `ghostty/` export: one file per scheme,
the sixteen ANSI colors, background, foreground, cursor and selection. The
collection is the most complete and the most watched one (Ghostty ships its
themes from it), and its files are already normalized to one format.

The script only **copies**: every value in the output is a hex number from
the scheme's file, unchanged. bateri's other roles (dim, accent, the status
colors, the search highlights, a selection or cursor that would not work in
bateri) are derived from these in Rust, by `Theme::adapted` — the contrast
arithmetic has one copy and it is there. A scheme is picked here by hand:
well known, maintained, and under a license that lets bateri ship it
(`THEMES[*]["license"]`, checked against the project's own repository; never
a commercial theme). `tools/third_party_notices.py` reads the same list for
the notices that travel with the package, so the list is the one place a
theme is added.

The order of the list is the order of View > Theme >: by title. The shell
splits it into dark and light by the background, so it is not grouped here.
"""

import os
import subprocess
import sys

# The commit the table was generated from; a different checkout stops the
# script, so that regenerating cannot pick up an upstream change silently.
COMMIT = "c02052159ff9a1c438ff419b7451cd33f9ab4934"
SOURCE = "https://github.com/mbadolato/iTerm2-Color-Schemes"

# The projects the schemes come from — the copyright holder and the license
# of each, as their repositories state them.
PROJECTS = {
    "catppuccin": {
        "title": "Catppuccin", "url": "https://github.com/catppuccin/catppuccin",
        "license": "MIT", "copyright": "Copyright (c) 2021 Catppuccin",
    },
    "dracula": {
        "title": "Dracula", "url": "https://github.com/dracula/dracula-theme",
        "license": "MIT", "copyright": "Copyright (c) 2023 Dracula Theme",
    },
    "everforest": {
        "title": "Everforest", "url": "https://github.com/sainnhe/everforest",
        "license": "MIT", "copyright": "Copyright (c) 2019 sainnhe",
    },
    "flexoki": {
        "title": "Flexoki", "url": "https://github.com/kepano/flexoki",
        "license": "MIT", "copyright": "Copyright (c) 2023 Steph Ango",
    },
    "github": {
        "title": "GitHub Theme", "url": "https://github.com/primer/github-vscode-theme",
        "license": "MIT", "copyright": "Copyright (c) 2020 Primer",
    },
    "gruvbox": {
        "title": "Gruvbox", "url": "https://github.com/morhetz/gruvbox",
        "license": "MIT", "copyright": "Copyright (c) Pavel Pertsev (morhetz)",
    },
    "kanagawa": {
        "title": "Kanagawa", "url": "https://github.com/rebelot/kanagawa.nvim",
        "license": "MIT", "copyright": "Copyright (c) 2021 Tommaso Laurenzi",
    },
    "night-owl": {
        "title": "Night Owl", "url": "https://github.com/sdras/night-owl-vscode-theme",
        "license": "MIT", "copyright": "Copyright (c) 2018 Sarah Drasner",
    },
    "nord": {
        "title": "Nord", "url": "https://github.com/nordtheme/nord",
        "license": "MIT", "copyright": "Copyright (c) 2016-present Sven Greb",
    },
    "one-dark": {
        "title": "One Dark", "url": "https://github.com/atom/one-dark-syntax",
        "license": "MIT", "copyright": "Copyright (c) 2016 GitHub Inc.",
    },
    "rose-pine": {
        "title": "Rosé Pine", "url": "https://github.com/rose-pine/rose-pine-theme",
        "license": "MIT", "copyright": "Copyright (c) 2023 Rosé Pine",
    },
    "solarized": {
        "title": "Solarized", "url": "https://github.com/altercation/solarized",
        "license": "MIT", "copyright": "Copyright (c) 2011 Ethan Schoonover",
    },
    "tokyo-night": {
        "title": "Tokyo Night", "url": "https://github.com/folke/tokyonight.nvim",
        "license": "Apache-2.0", "copyright": "Copyright (c) Folke Lemaitre",
    },
}

# (name, title, the scheme's file in `ghostty/`, project). The name is what
# `[appearance] theme` takes; the title is what the menus show.
THEMES = [
    ("catppuccin-frappe", "Catppuccin Frappé", "Catppuccin Frappe", "catppuccin"),
    ("catppuccin-latte", "Catppuccin Latte", "Catppuccin Latte", "catppuccin"),
    ("catppuccin-macchiato", "Catppuccin Macchiato", "Catppuccin Macchiato", "catppuccin"),
    ("catppuccin-mocha", "Catppuccin Mocha", "Catppuccin Mocha", "catppuccin"),
    ("dracula", "Dracula", "Dracula", "dracula"),
    ("everforest-dark", "Everforest Dark", "Everforest Dark Med", "everforest"),
    ("everforest-light", "Everforest Light", "Everforest Light Med", "everforest"),
    ("flexoki-dark", "Flexoki Dark", "Flexoki Dark", "flexoki"),
    ("flexoki-light", "Flexoki Light", "Flexoki Light", "flexoki"),
    ("github-dark", "GitHub Dark", "GitHub Dark Default", "github"),
    ("github-light", "GitHub Light", "GitHub Light Default", "github"),
    ("gruvbox-dark", "Gruvbox Dark", "Gruvbox Dark", "gruvbox"),
    ("gruvbox-light", "Gruvbox Light", "Gruvbox Light", "gruvbox"),
    ("kanagawa-wave", "Kanagawa Wave", "Kanagawa Wave", "kanagawa"),
    ("night-owl", "Night Owl", "Night Owl", "night-owl"),
    ("nord", "Nord", "Nord", "nord"),
    ("one-dark", "One Dark", "Atom One Dark", "one-dark"),
    ("rose-pine", "Rosé Pine", "Rose Pine", "rose-pine"),
    ("rose-pine-dawn", "Rosé Pine Dawn", "Rose Pine Dawn", "rose-pine"),
    ("rose-pine-moon", "Rosé Pine Moon", "Rose Pine Moon", "rose-pine"),
    ("solarized-dark", "Solarized Dark", "iTerm2 Solarized Dark", "solarized"),
    ("solarized-light", "Solarized Light", "iTerm2 Solarized Light", "solarized"),
    ("tokyo-night", "Tokyo Night", "TokyoNight Night", "tokyo-night"),
    ("tokyo-night-day", "Tokyo Night Day", "TokyoNight Day", "tokyo-night"),
]

KEYS = ("background", "foreground", "cursor-color", "selection-background")


def read_scheme(path):
    """A ghostty theme file -> {key: "rrggbb"} with `ansi` as a 16-list."""
    values = {"ansi": [None] * 16}
    with open(path, encoding="utf-8") as f:
        for line in f:
            key, sep, value = line.strip().partition(" = ")
            if not sep:
                continue
            if key == "palette":
                index, _, color = value.partition("=")
                values["ansi"][int(index)] = hex_of(color, path)
            elif key in KEYS:
                values[key] = hex_of(value, path)
    missing = [k for k in KEYS if k not in values]
    if missing or None in values["ansi"]:
        sys.exit(f"{path}: missing {missing or 'palette entries'}")
    return values


def hex_of(value, path):
    digits = value.strip().removeprefix("#").lower()
    if len(digits) != 6 or any(c not in "0123456789abcdef" for c in digits):
        sys.exit(f"{path}: not a #rrggbb color: {value!r}")
    return digits


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: import_themes.py <iTerm2-Color-Schemes checkout>")
    root = sys.argv[1]
    head = subprocess.run(["git", "-C", root, "rev-parse", "HEAD"],
                          check=True, capture_output=True, text=True).stdout.strip()
    if head != COMMIT:
        sys.exit(f"checkout is at {head}, the table is pinned to {COMMIT}; "
                 "update COMMIT on purpose")
    titles = [title for _, title, _, _ in THEMES]
    if titles != sorted(titles, key=str.casefold):
        sys.exit("THEMES must be ordered by title")

    out = [
        "//! The palettes bateri adapts from other projects — **generated** by",
        "//! `tools/import_themes.py` from iTerm2-Color-Schemes' `ghostty/` export",
        f"//! (<{SOURCE}>, commit",
        f"//! `{COMMIT[:12]}`). Edit the script and run it again; a value changed",
        "//! here by hand is lost then.",
        "//!",
        "//! Every number is the scheme's own: the four roles a terminal palette",
        "//! carries and its sixteen ANSI colors, as its authors chose them. The",
        "//! roles bateri adds are derived from them by [`super::Theme::adapted`].",
        "//! The license and the copyright of each project are in the script's",
        "//! `PROJECTS`; the package carries them (`THIRD-PARTY-LICENSES.txt`).",
        "",
        "use super::Scheme;",
        "",
        "// One field per line and the ANSI colors four to a line, in the order",
        "// of `Theme::BATERI`'s table: rustfmt would open the sixteen into a",
        "// column.",
        "#[rustfmt::skip]",
        f"pub(super) const SCHEMES: [Scheme; {len(THEMES)}] = [",
    ]
    for name, title, file, project in THEMES:
        if project not in PROJECTS:
            sys.exit(f"{name}: unknown project {project!r}")
        values = read_scheme(os.path.join(root, "ghostty", file))
        ansi = values["ansi"]
        out += [
            "    Scheme {",
            f"        name: \"{name}\",",
            f"        title: \"{title}\",",
            f"        background: 0x{values['background']},",
            f"        foreground: 0x{values['foreground']},",
            f"        cursor: 0x{values['cursor-color']},",
            f"        selection: 0x{values['selection-background']},",
            "        ansi: [",
        ]
        for row in range(4):
            colors = ", ".join(f"0x{c}" for c in ansi[row * 4:row * 4 + 4])
            out.append(f"            {colors},")
        out += ["        ],", "    },"]
    out += ["];", ""]
    sys.stdout.write("\n".join(out))


if __name__ == "__main__":
    main()
