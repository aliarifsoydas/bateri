# This is a fork of vte 0.15.0

`vte` is the terminal parser inside `alacritty_terminal`. bateri keeps its own copy
here and points cargo at it with `[patch.crates-io]` in the root `Cargo.toml`. The name
and version are upstream's (0.15.0), so `alacritty_terminal` resolves to the fork without
knowing it.

The files are the `vte` 0.15.0 package from crates.io (Apache-2.0 OR MIT; both license
files are here), without `.cargo-ok` and `Cargo.lock`. The first commit that added this
directory was upstream's, unchanged; every change since is in git history and listed
below. Upstream's code keeps upstream's formatting, and the workspace's `fmt` and
`clippy` do not look at it. Its tests, upstream's and ours, run with `make test`.

## Changes

- **A character cut by a read keeps the bytes after it.** `Parser::advance_partial_utf8`
  completes a character cut at the end of one `advance` by copying up to three bytes of
  the next into a buffer. When the buffer held the rest of that character, more complete
  characters and the start of one that was itself cut, it printed the first character
  but reported every valid byte of the buffer as consumed, so whatever stood between
  the characters was never printed (`C2 | B7 20 C2 B7` printed `··`, not `· ·`). It now
  reports only the first character's bytes; the rest is parsed from the input as usual.
  Tests: `partial_utf8_keeps_the_bytes_after_the_character` and
  `utf8_cut_anywhere_dispatches_what_the_whole_does` (every one- and two-way cut of a
  text mixing one- to four-byte characters, ASCII and an escape sequence).

## When `alacritty_terminal` asks for a newer `vte`

`make audit` goes red (cargo only warns that the patch is unused, then builds the
registry's `vte`, so the changes above would silently disappear). Take the new
version's package from crates.io, add it as a commit of its own so the diff to the
previous one is readable, then bring each change above forward and bump the version in
`vendor/vte/Cargo.toml` and in the `Cargo.toml` comment next to `[patch.crates-io]`.
`diff -r` between the package and this directory is the list of changes.
