# bateri's LOCAL bash wrapper — read as `$ENV` by `bash --posix`.
#
# WHY POSIX MODE: `--rcfile` is ignored by a login shell, and a login shell is what the
# terminal starts (argv[0] is `-bash`, or `-l` where there is no `login(1)`): `logout` works
# and `~/.bash_logout` is read on exit. In POSIX mode an interactive bash reads `$ENV` and no
# other startup file (kitty's method), so this file runs first, leaves POSIX mode and reads
# the login files itself, in bash's own order: `/etc/profile`, then the first readable of
# `~/.bash_profile`, `~/.bash_login`, `~/.profile`. The terminal only starts a bash it has
# asked the version of: 4.4 or newer (`PS0`); on an older one this file is never set up.
#
# THE USER'S FILES ARE READ AT THE TOP LEVEL, not from a function: `declare` and `local`
# inside a function are local, so `declare -x PATH=…` would be undone on return (the trap the
# zsh wrapper measured). They are read, never written (the `make audit` gate). Our hooks are
# installed AFTER they are read, so a `PROMPT_COMMAND` or `PS1` the files assign outright
# cannot take them away.
#
# CONTRACT (the setting side is `bt-shell-macos`'s `launch::integration_env`):
#   ENV              this file; the user's own value comes back below.
#   BATERI_ENV       the user's `ENV`, if there was one. It is put back and never read: a
#                    login bash does not read `ENV`.
#   BATERI_DOCK      `off`: there is no dock in this session. This wrapper has no dock arm
#                    yet, so the prompt is always the user's, with the marks around it.
#   BATERI_BIN, BATERI_SSH_INSTANCE, BATERI_SSH_CWD
#                    the `ssh` function's inputs, as in the zsh wrapper.
# The last four are ours, so they are taken out of the environment too (children have no use
# for them).

# `ENV` is ours only for this start: the user's own value comes back, and without one it goes,
# so child shells do not read this file.
if [ -n "${BATERI_ENV+x}" ]; then
  ENV=$BATERI_ENV
  unset BATERI_ENV
else
  unset ENV
fi

# Read once, before the user's files can see them.
__bateri_bin=${BATERI_BIN-}
__bateri_ssh_instance=${BATERI_SSH_INSTANCE-}
__bateri_ssh_cwd=${BATERI_SSH_CWD-}
unset BATERI_BIN BATERI_SSH_INSTANCE BATERI_SSH_CWD BATERI_DOCK

# POSIX mode's default history file is `~/.sh_history`, set before any startup file is read.
# A login bash keeps `~/.bash_history`, so the default is put back, unless the user's
# environment (an exported `HISTFILE`) named the file: that value is theirs, whatever it is.
# Their files may still set it, and win, as they do in a plain login shell.
if [ "$HISTFILE" = "$HOME/.sh_history" ] && [[ ${HISTFILE@a} != *x* ]]; then
  HISTFILE=$HOME/.bash_history
fi
set +o posix

if [ -r /etc/profile ]; then
  . /etc/profile
fi
for __bateri_login in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
  if [ -r "$__bateri_login" ]; then
    . "$__bateri_login"
    break
  fi
done
unset __bateri_login

# Percent-encodes `$1` (RFC 3986's unreserved set + `/`) into `REPLY`, byte by byte: `LC_ALL=C`
# makes every element a byte and `'c` its value.
__bateri_percent() {
  local LC_ALL=C s=$1 out= c i n
  for (( i = 0; i < ${#s}; i++ )); do
    c=${s:i:1}
    case $c in
      [A-Za-z0-9/._~-]) out+=$c ;;
      *)
        printf -v n '%d' "'$c"
        printf -v c '%%%02X' $(( n & 255 ))
        out+=$c
        ;;
    esac
  done
  REPLY=$out
}

# Reports the working directory with OSC 7, per prompt.
#
# THE AUTHORITY IS EMPTY (`file:///…`), not `file://$HOSTNAME…`: the terminal's gate counts a
# named host as foreign, and an empty one cannot go stale when the machine is renamed.
__bateri_cwd() {
  local REPLY
  __bateri_percent "$PWD"
  printf '\033]7;file://%s\007' "$REPLY"
}

# The command blocks: the zsh wrapper's `D`/`A`/`C`, with the block's id in `bt_block=` and,
# on the prompt's cells, in an OSC 8 anchor (`__bateri_prompt`) — the terminal reads which row
# a block started on from the grid, so it stays right after scrolling and reflow.
#
# THE CLOSING NEEDS `PS0` (bash 4.4): the only hook between reading a command and running it
# without a `DEBUG` trap (which is the user's). It closes the anchor and marks the output's
# start (`C`). `PS0` is expanded in this shell, and an arithmetic subscript is how it sets the
# "a command ran" flag without printing (`${…[…]-}`: empty even under `set -u`); an empty line
# reads no command and prints no `PS0`, so it closes no block — zsh's `preexec` rule.
__bateri_block=0
__bateri_ran=0
PS0='${__bateri_none[__bateri_ran=1]-}\e]8;;\a\e]133;C\a'"${PS0-}"

# Before the prompt: the finished command's `D`, the next prompt's `A`, the directory. The
# status of the command that ran is kept: a `PROMPT_COMMAND` after ours may read `$?`.
#
# `D` closes the FINISHED block, so its id is the counter's value before it moves. An empty
# line, or a line cancelled with Ctrl-C, ran nothing: it opens a new block with no `D` for the
# old one, as in zsh.
__bateri_precmd() {
  local status=$?
  if (( __bateri_ran )); then
    __bateri_ran=0
    printf '\033]133;D;%s;bt_block=%s\007' "$status" "$__bateri_block"
  fi
  __bateri_block=$(( __bateri_block + 1 ))
  printf '\033]133;A;bt_block=%s\007' "$__bateri_block"
  __bateri_cwd
  return $status
}

# The user's prompt with the anchor in front and the end mark behind.
#
# The anchor is an id-carrying OSC 8 link laid over the prompt's cells, and it lives IN `PS1`:
# readline draws the prompt again without running `PROMPT_COMMAND` (Ctrl-L, a resize), and a
# mark printed from a hook would be missing from that copy. It is a prefix: OSC 8 does not
# nest, so a prompt link of the user's replaces ours for the cells after it (a known limit, a
# stripe missing, not a wrong one).
#
# The id is written into the string each time, so the string is rebuilt at every prompt from
# the user's own text (`__bateri_user_ps1`). A `PS1` that is not the one left last time is a
# new text of the user's (a theme rewrites it at every prompt, `PROMPT_COMMAND` or not) and is
# the one wrapped; one that is, is wrapped again with the new id. `promptvars` is not relied
# on for the id: a user may turn it off.
#
# LAST in `PROMPT_COMMAND`: starship and powerline write `PS1` from their own entries, and
# this one must see what they wrote.
__bateri_user_ps1=
__bateri_ps1=
__bateri_prompt() {
  local status=$?
  if [[ $PS1 != "$__bateri_ps1" ]]; then
    __bateri_user_ps1=$PS1
  fi
  PS1='\[\e]8;;bateri://block/'$__bateri_block'\a\]'$__bateri_user_ps1'\[\e]133;B\a\]'
  __bateri_ps1=$PS1
  return $status
}

# First in `PROMPT_COMMAND`, so `$?` is still the command's when it runs; the prompt last.
# A bash 5.1 array keeps its other elements. A string uses newlines as the joiner: the user's
# value may end in a comment or a `;`.
if [[ ${PROMPT_COMMAND@a} == *a* ]]; then
  PROMPT_COMMAND=(__bateri_precmd "${PROMPT_COMMAND[@]}" __bateri_prompt)
else
  __bateri_nl=$'\n'
  PROMPT_COMMAND="__bateri_precmd${PROMPT_COMMAND:+$__bateri_nl$PROMPT_COMMAND}$__bateri_nl"__bateri_prompt
  unset __bateri_nl
fi

# Runs the user's `ssh`, wrapped when bateri says so. The rules are the zsh wrapper's and the
# binary's (`ssh_wrap::decide`): which call is interactive, the settings, `ssh -G`.
# `bateri ssh-argv` prints the wrapped arguments, each followed by a NUL, or nothing — and
# nothing (also a missing binary or any failure) means plain `command ssh "$@"`. `--tty` only
# when stdin and stdout are terminals. Inside a local multiplexer nothing is wrapped: the pane
# sees tmux or screen, not the ssh, so the session could never prove it ran. After a wrapped
# session ends `bateri ssh-fell-back` says whether to rerun it plain (a router, a server that
# refused our command). `--block` is this command's block; `--cwd` is only for the first `ssh`
# of a tab opened from a remote one.
#
# An answer is read NUL by NUL; one that does not end with a NUL is not ours: plain `ssh`.
__bateri_ssh() {
  local arg rc
  local -a tty=() instance=() cwd=() wrapped=() plain=()
  if [[ -n $__bateri_ssh_cwd ]]; then
    cwd=(--cwd "$__bateri_ssh_cwd")
  fi
  __bateri_ssh_cwd=
  if [[ -n ${TMUX-} || -n ${STY-} ]]; then
    command ssh "$@"
    return
  fi
  [[ -t 0 && -t 1 ]] && tty=(--tty)
  [[ -n $__bateri_ssh_instance ]] && instance=(--instance "$__bateri_ssh_instance")
  if [[ -x $__bateri_bin ]]; then
    arg=
    while IFS= read -r -d '' arg; do
      wrapped+=("$arg")
    done < <(command "$__bateri_bin" ssh-argv "${tty[@]}" --block "$__bateri_block" \
      "${instance[@]}" "${cwd[@]}" -- "$@" 2>/dev/null)
    [[ -n $arg ]] && wrapped=()
  fi
  if (( ${#wrapped[@]} )); then
    command ssh "${wrapped[@]}"
    rc=$?
    arg=
    while IFS= read -r -d '' arg; do
      plain+=("$arg")
    done < <(command "$__bateri_bin" ssh-fell-back --rc "$rc" "${instance[@]}" \
      -- "${wrapped[@]}" 2>/dev/null)
    [[ -n $arg ]] && plain=()
    if (( ${#plain[@]} )); then
      command ssh "${plain[@]}"
      return
    fi
    return $rc
  fi
  command ssh "$@"
}

# The `ssh` function, defined AFTER the user's files, so a function or alias of theirs is seen
# and wins (a known limit: theirs bypasses the integration). Only with the binary's path.
if [[ -n $__bateri_bin ]] && ! declare -F ssh > /dev/null && ! alias ssh > /dev/null 2>&1; then
  ssh() { __bateri_ssh "$@"; }
fi
