use crate::model::Config;

pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// `text` as a `case` pattern that ignores ASCII case: `[vV][iI][mM]`.
fn caseless(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphabetic() {
                format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase())
            } else {
                quote(&c.to_string())
            }
        })
        .collect()
}

/// A `case` pattern that is true when an allowlisted program appears as a
/// whole word on the command line. The shell runs it in-process; only a
/// match costs a fork. False positives (`echo ssh`) cost one save.
pub fn bash_pattern(config: &Config) -> String {
    let mut alternatives = Vec::new();
    for program in &config.allowed_programs {
        if program == "*" {
            return "*".into();
        }
        // With match_program_basename the allowlist compares basenames
        // ignoring case (Config::allows), so the pattern does too.
        let name = if config.match_program_basename && !program.contains('/') {
            caseless(program)
        } else {
            quote(program)
        };
        alternatives.push(format!("{name}|{name}' '*|*' '{name}|*' '{name}' '*"));
        if config.match_program_basename && !program.contains('/') {
            alternatives.push(format!("*'/'{name}|*'/'{name}' '*"));
        }
    }
    if alternatives.is_empty() {
        "''".into()
    } else {
        alternatives.join("|")
    }
}

// The trap is one call whose last argument is the `$_` it found, so `$_`
// is unchanged after it; a chained trap gets the original `$?` and `$_`
// through `__revive_ret`, and its status is passed on for extdebug.
// The prompt after a command killed by SIGHUP, SIGKILL or SIGTERM (status
// 129, 137, 143) does not save: a shutdown kills every pane's program that
// way, and saving then would record every pane idle. The status is taken by
// the first DEBUG trap after the command, which runs before any
// PROMPT_COMMAND entry, whatever order the rc file left them in.
// A relaunch whose PROMPT_COMMAND entry was dropped is reported once the rc
// file is done (no `source` frame), unless the entry is still there to run.
const SNIPPET: &str = r#"__revive_bin=@BIN@
__revive_hints=@HINTS@
__revive_precmd_run=__revive_precmd
# `set -m` puts the save in its own process group: from a line the rc file
# runs, before job control is on, it would join the shell's, and capture
# would then find the pane's foreground group shared and refuse it.
# REVIVE_COMMAND and REVIVE_SEQ record what the save was scheduled for; the
# save itself takes the hint from the hints file.
__revive_save() {
  ( set -m; ( sleep "$1"; REVIVE_SHELL_PID=$$ REVIVE_COMMAND="$2" REVIVE_SEQ="${3-}" "$__revive_bin" --host core@PATHS@ autosave --force ) >/dev/null 2>&1 & )
}
__revive_save_throttled() {
  if [ "${__revive_stamp-}" != "$SECONDS" ]; then
    __revive_stamp=$SECONDS
    __revive_save 0.5 "$1" "$2"
  elif [ "${__revive_stamp2-}" != "$SECONDS" ]; then
    __revive_stamp2=$SECONDS
    __revive_save 1.5 "$1" "$2"
  fi
}
# The file holds the command number, then the running command's hint if it
# has one: a deferred save takes the hint from here, so a save that fires
# after a later command started (one whose own save the limit dropped)
# serves that command.
__revive_retire() {
  __revive_hinted=
  __revive_seq=$(( ${__revive_seq:-0} + 1 ))
  printf '%s\n%s' "$__revive_seq" "${1-}" 2>/dev/null >| "$__revive_hints/$$" || :
}
# Whether `$1` is one of the commands PROMPT_COMMAND runs, compared with
# whitespace removed (bash reprints `echo   a>/dev/null` as
# `echo a > /dev/null`) and only as a whole command, so a typed `vi` is not
# taken for part of `__revive_precmd`. Nothing runs ahead of the user's
# entries, which therefore still see the command's PIPESTATUS and `$_`.
# The split list is kept until PROMPT_COMMAND changes.
__revive_is_prompt_entry() {
  # Joined by hand: bash 3.2 splits `${PROMPT_COMMAND[*]}` on a `;` IFS.
  local __revive_one __revive_now= __revive_item
  for __revive_item in ${PROMPT_COMMAND[@]+"${PROMPT_COMMAND[@]}"}; do
    __revive_now+="$__revive_item;"
  done
  if [[ ${__revive_pc_of-} != "$__revive_now" ]]; then
    __revive_pc_of=$__revive_now
    __revive_pc=";$__revive_now"
    __revive_pc=${__revive_pc//$'\n'/;}
    __revive_pc=${__revive_pc//||/;}
    __revive_pc=${__revive_pc//&&/;}
    __revive_pc=${__revive_pc//|/;}
    __revive_pc=${__revive_pc//&/;}
    # A compound entry (`if x; then y; fi`) runs its commands with a keyword
    # in front; drop those so `x` and `y` are found as commands.
    # Split by hand: bash 3.2's `IFS=';' read` drops the `;` from a
    # here-string before splitting it.
    local __revive_part __revive_rest=$__revive_pc
    __revive_pc=';'
    while [[ $__revive_rest == *';'* ]]; do
      __revive_part=${__revive_rest%%;*}
      __revive_rest=${__revive_rest#*;}
      # A trailing comment is no part of the command bash reprints.
      __revive_now=${__revive_part%%[[:space:]]#*}
      [[ $__revive_now == *[\'\"]* ]] || __revive_part=$__revive_now
      [[ $__revive_part == '#'* ]] && __revive_part=
      while :; do
        __revive_part=${__revive_part#"${__revive_part%%[![:space:]]*}"}
        case $__revive_part in
          # `(( n++ ))` is one arithmetic command, not a subshell.
          '(('*) break ;;
          '('*) __revive_part=${__revive_part#(} ;;
          time[[:space:]]-p[[:space:]]*) __revive_part=${__revive_part#time*-p} ;;
          if[[:space:]]*|then[[:space:]]*|elif[[:space:]]*|else[[:space:]]*|while[[:space:]]*|until[[:space:]]*|do[[:space:]]*|'{'[[:space:]]*|'!'[[:space:]]*|time[[:space:]]*)
            __revive_part=${__revive_part#*[[:space:]]} ;;
          # `case w in p) x`: bash runs `case w in ` and then `x`.
          case[[:space:]]*[[:space:]]in[[:space:]]*)
            __revive_now=${__revive_part%%[[:space:]]in[[:space:]]*}
            __revive_pc+="${__revive_now//[[:space:]]/}in;"
            __revive_part=${__revive_part#*[[:space:]]in[[:space:]]}
            __revive_part=${__revive_part#*)} ;;
          *) break ;;
        esac
      done
      # `eval "x"` runs `x` as a command of its own as well.
      case $__revive_part in
        eval[[:space:]]*)
          __revive_now=${__revive_part#eval}
          __revive_now=${__revive_now//\"/}
          __revive_now=${__revive_now//\'/}
          __revive_pc+="${__revive_now//[[:space:]]/};" ;;
      esac
      __revive_pc+="${__revive_part//[[:space:]]/};"
    done
  fi
  __revive_one=${1//[[:space:]]/}
  case ${__revive_pc-} in *";$__revive_one;"*) return 0 ;; esac
  return 1
}
# This runs before every command the shell runs, so its common path uses
# only `[[ ]]` and `case`, which cost a fraction of `[ ]` and redirections.
__revive_preexec() {
  [[ ${BASH_SUBSHELL:-0} = 0 ]] || return "${2:-0}"
  if [[ -n ${__revive_relaunch_cmd+x} ]]; then
    case " ${FUNCNAME[*]} " in
      *" source "*) ;;
      *)
        case "${PROMPT_COMMAND[*]-}" in
          *__revive_relaunch*) ;;
          *)
            printf '\033[2mherdr-revive: the relaunch did not run because PROMPT_COMMAND was replaced after the herdr-revive line: %s\033[0m\n' "$__revive_relaunch_cmd" >&2
            unset __revive_relaunch_cmd
            unset -f __revive_relaunch
            ;;
        esac
        ;;
    esac
  fi
  if [[ -n ${__revive_pending-} && -z ${__revive_rc+x} && -n ${__revive_rc0-} ]]; then
    __revive_rc=$__revive_rc0
    # Kept until the prompt hook has run: a PROMPT_COMMAND entry the text
    # match below misses is taken for a command, and must not save a
    # shutdown as an idle pane.
    case $__revive_rc in 129|137|143) __revive_killed=$SECONDS ;; esac
  fi
  # A hint names one command. Whatever this shell runs next (a function,
  # `$cmd`, the next part of a pipeline) or the prompt retires it, so a
  # save that comes later does not lend it to another program.
  [[ -z ${__revive_hinted-} ]] || __revive_retire
  local __revive_cmd=$1
  case $__revive_cmd in
    *[\'\"]*)
      __revive_cmd=${__revive_cmd//\'/}
      __revive_cmd=${__revive_cmd//\"/}
      ;;
  esac
  case "$__revive_cmd" in
    __revive_*) ;;
    # A job brought back (`fg`, `%1`): no program word to match or hint, but
    # its program holds the pane again, so it saves as a start does.
    fg|fg[[:space:]]*|%*)
      __revive_held && return "${2:-0}"
      __revive_pending=1; unset __revive_rc
      __revive_retire
      __revive_save_throttled "" "$__revive_seq" || : ;;
    @PATTERN@)
      # A PROMPT_COMMAND entry is not a new command: counting one would drop
      # the status taken above and save after a shutdown kill, and under
      # `*` would save at every prompt. Asked only here, so a command that
      # does not match costs no more than the pattern.
      if [ -n "$1" ] && __revive_is_prompt_entry "$1"; then
        return "${2:-0}"
      fi
      __revive_held && return "${2:-0}"
      __revive_pending=1; unset __revive_rc
      # The command is passed as a hint only when it runs as typed on the
      # terminal: this trap runs with the redirections of a group or loop
      # around it already applied (`{ dd; } < img`, `exec >log`), so its own
      # streams show them; and a function or alias would run something
      # else than its name. Both tests are builtins: no fork per command.
      local __revive_hint= __revive_word=${1%%[[:space:]]*}
      # A quoted or escaped name (`'top'`, `\top`) still runs a function of
      # that name, so look it up without its quotes.
      __revive_word=${__revive_word//\\/}
      __revive_word=${__revive_word//\'/}
      __revive_word=${__revive_word//\"/}
      # A builtin starts no program, so a start save would see nothing;
      # skipping it leaves the two saves a second allows to programs
      # (`clear; cd x; top` still saves while top runs). The prompt after
      # it saves as usual.
      case $__revive_word in
        cd|pushd|popd|dirs|export|unset|set|shopt|alias|unalias|echo|printf|read|true|false|:|test|'['|'[['|local|declare|typeset|readonly|history|source|.|hash|type|umask|ulimit|let|shift|trap|getopts|enable|help|bind|complete|compgen|compopt|mapfile|readarray|times|jobs)
          return "${2:-0}" ;;
      esac
      # bash-preexec passes `history 1`, which is an earlier line when the
      # command is kept out of history (HISTIGNORE, `set +o history`): it
      # counts only when it is the command bash is about to run. Loaded
      # after the hook, bash-preexec runs the hook's trap once per line,
      # so later commands of the line never retire the hint: no hint then.
      if [ -t 0 ] && [ -t 1 ] && [ -t 2 ] \
        && ! declare -F -- "$__revive_word" >/dev/null 2>&1 \
        && ! alias -- "$__revive_word" >/dev/null 2>&1 \
        && { [ -z "${__bp_imported-}${bash_preexec_imported-}" ] \
          || [ -n "${__revive_relaunching-}" ] \
          || { [ -n "${__revive_via_bp-}" ] && [ "$1" = "$BASH_COMMAND" ]; }; }; then
        __revive_hint=$1
      fi
      # The save checks this number against the file: a deferred save whose
      # command was followed by another one (`a && b`) must not lend its
      # hint to the next program. A builtin redirection, no fork.
      __revive_retire "$__revive_hint"
      __revive_hinted=${__revive_hint:+1}
      __revive_save_throttled "$__revive_hint" "$__revive_seq" || : ;;
  esac
  return "${2:-0}"
}
# The hook's functions turn xtrace off inside (the braces send the trace of
# that switch to /dev/null), so `set -x` shows the user's commands rather
# than the hook's: only its call and a line or two around it.
# A kill status holds saves until the prompt hook clears it, or for the
# second it came in and the next: a shell whose PROMPT_COMMAND lost the hook
# must not stop saving for good.
__revive_held() {
  [[ -n ${__revive_killed-} ]] && (( SECONDS - __revive_killed < 2 ))
}
__revive_precmd() {
  { local __revive_entry=$? __revive_x=$- __revive_status; set +x; } 2>/dev/null
  __revive_status=${__revive_rc-$__revive_entry}
  unset __revive_rc __revive_rc0
  # The trap already retired a hint before this entry; bash-preexec runs
  # this without the trap.
  [ -z "${__revive_hinted-}" ] || __revive_retire
  if [ -n "${__revive_pending-}" ]; then
    __revive_pending=
    case "$__revive_status" in 129|137|143) ;; *) __revive_held || __revive_save 0.5 "" ;; esac
  fi
  __revive_killed=
  { case $__revive_x in *x*) set -x ;; esac; } 2>/dev/null
  # Entries after this one still see the status they would have seen. The
  # entry reads `${__revive_precmd_run:-:} && :`: a failure before the last
  # `&&` trips neither `set -e` nor the ERR trap, and a child bash that
  # inherits an exported PROMPT_COMMAND, without this function, runs `:`.
  return "$__revive_entry"
}
if [ -n "${__bp_imported-}" ] || [ -n "${bash_preexec_imported-}" ]; then
  # bash-preexec calls this without the trap, so it brings its own
  # xtrace guard.
  __revive_bp_preexec() {
    local __revive_x=
    case $- in *x*) { __revive_x=1; set +x; } 2>/dev/null ;; esac
    __revive_via_bp=1 __revive_preexec "$@"
    [[ -z $__revive_x ]] || { set -x; } 2>/dev/null
  }
  case " ${preexec_functions[*]-} " in
    *" __revive_bp_preexec "*|*" __revive_preexec "*) ;;
    *) preexec_functions+=(__revive_bp_preexec) ;;
  esac
  case " ${precmd_functions[*]-} " in
    *" __revive_precmd "*) ;;
    *) precmd_functions+=(__revive_precmd) ;;
  esac
else
  # Defines the trap's function, chaining `$1`: the DEBUG trap the shell
  # had. The caller sets the trap itself: bash 3.2 drops a DEBUG trap set
  # inside a function when the function returns. The trap's status is the
  # user's trap's (extdebug skips on it), behind `&&` so that it trips
  # neither `set -e` nor the ERR trap; `: "$_"` keeps `$_` as it was.
  __revive_trap='__revive_debug "$?" "$_" && : "$_"'
  __revive_install_trap() {
    local __revive_prev=$1
    # Already the hook's (the snippet evaluated again): nothing to set.
    case "$__revive_prev" in *__revive_debug*|*__revive_preexec*) return 1 ;; esac
    __revive_prev=${__revive_prev#trap -- \'}
    __revive_prev=${__revive_prev%\' DEBUG}
    __revive_prev=${__revive_prev//\'\\\'\'/\'}
    if [ -n "$__revive_prev" ] && eval "__revive_prev_debug() { $__revive_prev
}" 2>/dev/null; then
      __revive_ret() { return "$1"; }
      __revive_debug() {
        # A tool loaded later may wrap this trap and call it bare (starship's
        # bash init does): then the status is unknown, not empty.
        __revive_rc0=${1-}
        # Bash runs a trap body at top level, where a failing `a && b` list
        # trips neither `set -e` nor the ERR trap; in a function it would.
        # So every status the user's trap gives goes through `&& :`, and
        # the `$?` passed on is set in a condition. Each branch starts with
        # that `$?` and `$_`.
        if __revive_ret "${1:-0}" "${2-}"; then
          __revive_prev_debug && :
        else
          __revive_prev_debug && :
        fi
        local __revive_st=$?
        case $- in *x*) { __revive_x=1; set +x; } 2>/dev/null ;; *) __revive_x= ;; esac
        case $BASH_COMMAND in
          *[\'\"]*|fg|fg[[:space:]]*|%*|@PATTERN@) __revive_preexec "$BASH_COMMAND" ;;
          *) [[ -z ${__revive_hinted-}${__revive_relaunch_cmd+x}${__revive_pending-} ]] \
            || __revive_preexec "$BASH_COMMAND" ;;
        esac
        [[ -z $__revive_x ]] || { set -x; } 2>/dev/null
        return "$__revive_st"
      }
    else
      __revive_debug() {
        case $- in *x*) { __revive_x=1; set +x; } 2>/dev/null ;; *) __revive_x= ;; esac
        __revive_rc0=${1-}
        # The common case skips the call: a command that cannot match (no
        # quote to strip, not the pattern) while no hint, relaunch or
        # pending status needs looking at.
        case $BASH_COMMAND in
          *[\'\"]*|fg|fg[[:space:]]*|%*|@PATTERN@) __revive_preexec "$BASH_COMMAND" ;;
          *) [[ -z ${__revive_hinted-}${__revive_relaunch_cmd+x}${__revive_pending-} ]] \
            || __revive_preexec "$BASH_COMMAND" ;;
        esac
        [[ -z $__revive_x ]] || { set -x; } 2>/dev/null
      }
    fi
  }
  case "$(declare -p PROMPT_COMMAND 2>/dev/null)" in
    *__revive_precmd*) ;;
    "declare -a"*) PROMPT_COMMAND=(${PROMPT_COMMAND[@]+"${PROMPT_COMMAND[@]}"} '${__revive_precmd_run:-:} && :') ;;
    *) PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND
}"'${__revive_precmd_run:-:} && :' ;;
  esac
  # Bash hides the DEBUG trap from a file run with `.` or `source`, and a
  # trap set there replaces it for good. So when another file runs this
  # one (~/.profile running ~/.bashrc), the trap goes in at the first
  # prompt, where the shell's own trap shows; a later line that replaces
  # PROMPT_COMMAND then leaves the hook off. A shell with the hook already
  # (the rc file sourced again) keeps its trap.
  if declare -F __revive_debug >/dev/null; then
    :
  elif [[ $- == *i* && ${#BASH_SOURCE[@]} -gt 1 ]]; then
    __revive_trap_wanted=1
    __revive_wanted_entry='[[ -z ${__revive_trap_wanted-} ]] || { __revive_deferred "$(trap -p DEBUG)" && trap "$__revive_trap" DEBUG; }'
    # Runs once: takes its entry out of PROMPT_COMMAND again, so entries
    # after it keep their `$?` from the next prompt on.
    __revive_deferred() {
      unset __revive_trap_wanted
      local __revive_i
      case "$(declare -p PROMPT_COMMAND 2>/dev/null)" in
        "declare -a"*)
          for __revive_i in "${!PROMPT_COMMAND[@]}"; do
            [[ ${PROMPT_COMMAND[__revive_i]} != "$__revive_wanted_entry" ]] \
              || unset 'PROMPT_COMMAND[__revive_i]'
          done ;;
        *) PROMPT_COMMAND=${PROMPT_COMMAND/$'\n'"$__revive_wanted_entry"/} ;;
      esac
      __revive_install_trap "$1"
    }
    case "$(declare -p PROMPT_COMMAND 2>/dev/null)" in
      *__revive_trap_wanted*) ;;
      "declare -a"*) PROMPT_COMMAND+=("$__revive_wanted_entry") ;;
      *) PROMPT_COMMAND="$PROMPT_COMMAND
$__revive_wanted_entry" ;;
    esac
  else
    # Last, so none of the lines above runs under the trap.
    __revive_install_trap "$(trap -p DEBUG)" && trap "$__revive_trap" DEBUG
  fi
fi
"#;

const RELAUNCH_HEAD: &str = r#"__revive_relaunch() {
  __revive_relaunch() { return; }
  # The relaunched program is a real command, though PROMPT_COMMAND runs it.
  unset __revive_relaunch_cmd
"#;

const RELAUNCH_TAIL: &str = r#"  __revive_rc=$?
  return "$__revive_rc"
}
__revive_relaunch_run=__revive_relaunch
case "$(declare -p PROMPT_COMMAND 2>/dev/null)" in
  "declare -a"*) PROMPT_COMMAND=(${PROMPT_COMMAND[@]+"${PROMPT_COMMAND[@]}"} '${__revive_relaunch_run:-:} && :') ;;
  *) PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND
}"'${__revive_relaunch_run:-:} && :' ;;
esac
"#;

/// Runs the relaunched command once, from the first prompt: after the rest of
/// the rc file has run (lines below the marker block often set `PATH`), and
/// with job control on, so the command leads its own process group and
/// capture records it rather than the shell. A command started inside a
/// function never reaches the `DEBUG` trap, so it reports itself to the hook.
/// The entry stays in `PROMPT_COMMAND` as a no-op that keeps `$?`: editing
/// the user's text at run time could leave it unparsable. It is appended,
/// so the user's own entries still see each command's `PIPESTATUS` and `$_`.
/// `words` is the program's own argv, quoted: the hook passes it as the
/// command's hint, which the `cd ... && command ...` text could never be.
pub fn bash_relaunch(command: &str, words: Option<&str>) -> String {
    let quoted = quote(command);
    let hint = quote(words.unwrap_or(command));
    format!(
        "{RELAUNCH_HEAD}  ! declare -F __revive_preexec >/dev/null || __revive_relaunching=1 __revive_preexec {hint}\n{command}\n{RELAUNCH_TAIL}__revive_relaunch_cmd={quoted}\n"
    )
}

/// Where each shell writes the number of its latest matching command, so a
/// save can tell whether the hint it carries is still that command's.
pub fn hints_dir(state_dir: &std::path::Path) -> std::path::PathBuf {
    state_dir.join("hints")
}

/// Whether `seq` is the latest command number shell `pid` wrote.
pub fn hint_is_current(state_dir: &std::path::Path, pid: &str, seq: &str) -> bool {
    !seq.is_empty() && latest(state_dir, pid).is_some_and(|(latest, _)| latest == seq)
}

/// Shell `pid`'s latest command number and the hint of the command running
/// now, which is empty once that command has ended or when it had none.
pub fn latest(state_dir: &std::path::Path, pid: &str) -> Option<(String, String)> {
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let text = std::fs::read_to_string(hints_dir(state_dir).join(pid)).ok()?;
    let (seq, hint) = text.split_once('\n').unwrap_or((&text, ""));
    Some((seq.trim().to_string(), hint.to_string()))
}

pub fn bash_snippet(config: &Config, binary: &str, hints: &str) -> String {
    bash_snippet_with(config, binary, hints, "")
}

/// `paths` is put in every save's command line as given: `init`'s own
/// `--config-dir`/`--state-dir`/`--socket`, each quoted, or nothing.
pub fn bash_snippet_with(config: &Config, binary: &str, hints: &str, paths: &str) -> String {
    SNIPPET
        .replace("@PATHS@", paths)
        .replace("@BIN@", &quote(binary))
        .replace("@HINTS@", &quote(hints))
        .replace("@PATTERN@", &bash_pattern(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Config;
    use std::process::{Command, Stdio};

    /// Where test shells write their command numbers; never read back
    /// except by the tests that check it.
    const HINTS: &str = "/tmp";

    fn config(programs: &[&str], basename: bool) -> Config {
        Config {
            allowed_programs: programs.iter().map(|s| (*s).into()).collect(),
            match_program_basename: basename,
            auto_save: true,
            ..Config::default()
        }
    }

    #[test]
    fn patterns_match_the_program_as_a_word_and_nothing_else() {
        assert_eq!(bash_pattern(&config(&[], false)), "''");
        assert_eq!(bash_pattern(&config(&["*"], false)), "*");
        assert_eq!(quote("it's"), "'it'\\''s'");
        // Run each pattern in bash: which command lines it takes.
        let cases: &[(&[&str], bool, &str, bool)] = &[
            (&["ssh"], false, "ssh", true),
            (&["ssh"], false, "ssh host", true),
            (&["ssh"], false, "sudo ssh host", true),
            (&["ssh"], false, "sudo ssh", true),
            (&["ssh"], false, "sshd", false),
            (&["ssh"], false, "echo sshd", false),
            (&["ssh"], false, "/usr/bin/ssh host", false),
            (&["ssh"], false, "SSH host", false),
            (&["ssh"], true, "/usr/bin/ssh host", true),
            (&["vim"], true, "/usr/local/bin/Vim notes", true),
            (&["vim"], true, "VIM notes", true),
            (&["vim"], true, "vimdiff a b", false),
            (&["/usr/bin/ssh"], true, "/usr/bin/ssh host", true),
            (&["/usr/bin/ssh"], true, "/usr/bin/SSH host", false),
            (&["g++"], true, "/usr/bin/G++ a.c", true),
        ];
        for (programs, basename, line, matches) in cases {
            let pattern = bash_pattern(&config(programs, *basename));
            let out = bash(
                &format!("case \"$1\" in {pattern}) echo yes ;; *) echo no ;; esac"),
                &[line],
                &[],
            );
            let want = if *matches { "yes" } else { "no" };
            assert_eq!(
                out.trim(),
                want,
                "{programs:?} {basename} {line:?}: {pattern}"
            );
        }
    }

    /// Runs `script` with `args` as `$1...`; the shell must exit 0 and write
    /// nothing to stderr.
    fn bash(script: &str, args: &[&str], env: &[(&str, &str)]) -> String {
        let output = Command::new("bash")
            .arg("-c")
            .arg(script)
            .arg("_")
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(stderr.is_empty(), "{stderr}");
        String::from_utf8(output.stdout).unwrap()
    }

    /// `bash` with its three streams on a pseudo-terminal, as in a pane: the
    /// hook passes a command hint only when they are a terminal. `script`
    /// supplies the pty; `$1` reaches the script through the environment.
    fn bash_tty(script: &str, arg: &str, env: &[(&str, &str)]) {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let file = dir.path().join("script.sh");
        std::fs::write(&file, format!("set -- \"$REVIVE_TEST_ARG\"\n{script}\n")).unwrap();
        let mut command = if cfg!(target_os = "macos") {
            let mut command = Command::new("script");
            command.args(["-q", "/dev/null", "bash"]).arg(&file);
            command
        } else {
            let mut command = Command::new("script");
            command.args(["-qec", &format!("bash {}", file.display()), "/dev/null"]);
            command
        };
        let status = command
            .env("REVIVE_TEST_ARG", arg)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// A directory holding an executable `name` that does nothing, for a
    /// command that must be a program rather than a shell function.
    fn program(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, "#!/bin/sh\n:\n").unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        dir
    }

    fn path_with(dir: &tempfile::TempDir) -> String {
        format!(
            "{}:{}",
            dir.path().display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    fn stub() -> (tempfile::TempDir, String, std::path::PathBuf) {
        stub_logging("\"${REVIVE_SHELL_PID-unset} $*\"")
    }

    /// A stand-in binary that appends `line` (a shell word) to `$REVIVE_STUB_LOG`.
    fn stub_logging(line: &str) -> (tempfile::TempDir, String, std::path::PathBuf) {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let log = dir.path().join("log");
        let bin = dir.path().join("herdr-revive");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\nprintf '%s\\n' {line} >> \"$REVIVE_STUB_LOG\"\n"),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        (dir, bin.to_str().unwrap().to_string(), log)
    }

    fn wait_lines(log: &std::path::Path, count: usize) -> Vec<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let lines = read_lines(log);
            if lines.len() >= count || std::time::Instant::now() > deadline {
                return lines;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn read_lines(log: &std::path::Path) -> Vec<String> {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        text.lines().map(String::from).collect()
    }

    fn syntax_ok(snippet: &str) -> bool {
        let check = Command::new("bash")
            .arg("-n")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut check.stdin.as_ref().unwrap(), snippet.as_bytes()).unwrap();
        check.wait_with_output().unwrap().status.success()
    }

    #[test]
    fn snippet_parses_and_saves_only_around_allowlisted_commands() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        assert!(syntax_ok(&snippet));
        assert!(syntax_ok(&bash_snippet(&config(&["*"], true), &bin, HINTS)));
        assert!(syntax_ok(&bash_snippet(&config(&[], true), &bin, HINTS)));
        let shell = bash(
            r#"echo $$
eval "$1"
__revive_preexec "cat ssh.log"
__revive_preexec "echo sshd"
__revive_precmd
__revive_preexec "sudo ssh host"
__revive_precmd
SECONDS=10
__revive_preexec "command 'ssh' 'host'"
sleep 1.2"#,
            &[&snippet],
            &[("REVIVE_STUB_LOG", log.to_str().unwrap())],
        );
        let lines = wait_lines(&log, 3);
        let expected = format!("{} --host core autosave --force", shell.trim());
        assert_eq!(lines, vec![expected; 3]);
        drop(dir);
    }

    #[test]
    fn snippet_chains_existing_traps_and_prompt_commands() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        let out = bash(
            r#"trap 'echo prev >> "$REVIVE_STUB_LOG"' DEBUG
PROMPT_COMMAND="echo pc"
eval "$1"
: marker
echo "$PROMPT_COMMAND""#,
            &[&snippet],
            &[("REVIVE_STUB_LOG", log.to_str().unwrap())],
        );
        assert_eq!(out.trim(), "echo pc\n${__revive_precmd_run:-:} && :");
        assert!(std::fs::read_to_string(&log).unwrap().contains("prev"));
        let out = bash(
            r#"PROMPT_COMMAND=(one two)
eval "$1"
printf '%s|' "${PROMPT_COMMAND[@]}""#,
            &[&snippet],
            &[],
        );
        assert_eq!(out, "one|two|${__revive_precmd_run:-:} && :|");
        let out = bash(
            r#"__bp_imported=1; preexec_functions=(); precmd_functions=(a)
eval "$1"
echo "${preexec_functions[0]} ${precmd_functions[1]} $(trap -p DEBUG)""#,
            &[&snippet],
            &[],
        );
        assert_eq!(out.trim(), "__revive_bp_preexec __revive_precmd");
        drop(dir);
    }

    #[test]
    fn prompt_commands_ending_in_a_terminator_still_run() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        for previous in ["echo pc;", "echo pc &", "echo pc\n"] {
            let out = bash(
                r#"PROMPT_COMMAND=$2
eval "$1"
__revive_pending=1
eval "$PROMPT_COMMAND"
wait
echo "pending=${__revive_pending-}""#,
                &[&snippet, previous],
                &[("REVIVE_STUB_LOG", log.to_str().unwrap())],
            );
            assert_eq!(out, "pc\npending=\n", "{previous:?}");
        }
        assert_eq!(wait_lines(&log, 3).len(), 3);
        drop(dir);
    }

    #[test]
    fn evaluating_the_snippet_repeatedly_registers_once() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        let user = dir.path().join("user");
        let saves = dir.path().join("saves");
        let env = [
            ("USER_LOG", user.to_str().unwrap()),
            ("SAVE_LOG", saves.to_str().unwrap()),
            ("REVIVE_STUB_LOG", log.to_str().unwrap()),
        ];
        let cases = [
            (
                "PROMPT_COMMAND='echo pc'",
                "echo pc\n${__revive_precmd_run:-:} && :\ntrap -- '__revive_debug \"$?\" \"$_\" && : \"$_\"' DEBUG",
            ),
            (
                "PROMPT_COMMAND='echo pc'\ntrap 'printf \"%s\\n\" \"$BASH_COMMAND\" >> \"$USER_LOG\"' DEBUG",
                "echo pc\n${__revive_precmd_run:-:} && :\ntrap -- '__revive_debug \"$?\" \"$_\" && : \"$_\"' DEBUG",
            ),
            (
                "PROMPT_COMMAND=(one)",
                "one ${__revive_precmd_run:-:} && :\ntrap -- '__revive_debug \"$?\" \"$_\" && : \"$_\"' DEBUG",
            ),
            (
                "__bp_imported=1; preexec_functions=(); precmd_functions=()",
                "__revive_bp_preexec __revive_precmd",
            ),
        ];
        for (setup, registered) in cases {
            let _ = std::fs::remove_file(&user);
            let _ = std::fs::remove_file(&saves);
            let script = format!(
                r#"ssh() {{ :; }}
{setup}
eval "$1"
eval "$1"
eval "$1"
__revive_save() {{ echo save >> "$SAVE_LOG"; }}
if [ -n "${{__bp_imported-}}" ]; then
  __revive_preexec "ssh host"
  echo "${{preexec_functions[*]}} ${{precmd_functions[*]}}"
else
  ssh host
  echo "${{PROMPT_COMMAND[*]}}"
  trap -p DEBUG
fi
__revive_precmd"#
            );
            let out = bash(&script, &[&snippet], &env);
            assert_eq!(out.trim(), registered, "{setup}");
            assert_eq!(read_lines(&saves), vec!["save"; 2], "{setup}");
            if setup.contains("USER_LOG") {
                let user_lines = read_lines(&user);
                assert_eq!(
                    user_lines.iter().filter(|l| *l == "ssh host").count(),
                    1,
                    "{user_lines:?}"
                );
            }
        }
        drop(dir);
    }

    #[test]
    fn the_prompt_after_a_command_killed_by_hup_kill_or_term_does_not_save() {
        let (dir, bin, _) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        let saves = dir.path().join("saves");
        for (status, expected) in [(143, 1), (129, 1), (137, 1), (130, 2), (1, 2), (0, 2)] {
            for (setup, after) in [
                ("PROMPT_COMMAND='echo pc'", ""),
                ("PROMPT_COMMAND=(one)", ""),
                ("", "PROMPT_COMMAND=\"history -a; $PROMPT_COMMAND\""),
                (
                    "PROMPT_COMMAND=(one)",
                    "PROMPT_COMMAND=(one \"${PROMPT_COMMAND[@]}\")",
                ),
                (
                    "__bp_imported=1; preexec_functions=(); precmd_functions=()",
                    "",
                ),
            ] {
                let _ = std::fs::remove_file(&saves);
                let status = status.to_string();
                bash(
                    r#"ssh() { return "$STATUS"; }
one() { :; }
eval "$2"
eval "$1"
eval "$3"
__revive_save() { echo "save $1" >> "$SAVE_LOG"; }
case "$(declare -p PROMPT_COMMAND 2>/dev/null)" in
  "declare -a"*) array=1 ;;
  *) array= ;;
esac
if [ -n "${__bp_imported-}" ]; then
  __revive_preexec "ssh host"
  ssh host
  __revive_precmd
elif [ -n "$array" ]; then
  ssh host
  for c in "${PROMPT_COMMAND[@]}"; do eval "$c"; done
else
  ssh host
  eval "$PROMPT_COMMAND"
fi
echo "pending=${__revive_pending-}""#,
                    &[&snippet, setup, after],
                    &[
                        ("SAVE_LOG", saves.to_str().unwrap()),
                        ("STATUS", status.as_str()),
                        ("HISTFILE", "/dev/null"),
                    ],
                );
                assert_eq!(
                    read_lines(&saves),
                    vec!["save 0.5"; expected],
                    "exit {status} with {setup} then {after}"
                );
            }
        }
        drop(dir);
    }

    #[test]
    fn a_kill_status_holds_off_saves_until_the_prompt() {
        let (dir, bin, _) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        let saves = dir.path().join("saves");
        // A kill holds off every save up to the prompt, for the second it
        // came in and the next: a PROMPT_COMMAND entry the hook takes for a
        // command cannot then save a shutdown. Five seconds on, the next
        // command saves again, and so does its prompt.
        for (first, second, expected) in [(0, 143, 2), (143, 0, 3)] {
            let _ = std::fs::remove_file(&saves);
            let (first, second) = (first.to_string(), second.to_string());
            bash(
                r#"ssh() { return "$1"; }
PROMPT_COMMAND='history -a'
eval "$1"
PROMPT_COMMAND="history -a; $PROMPT_COMMAND"
__revive_save() { echo "save $1" >> "$SAVE_LOG"; }
SECONDS=0
ssh "$FIRST"; SECONDS=5; ssh "$SECOND"
eval "$PROMPT_COMMAND""#,
                &[&snippet],
                &[
                    ("SAVE_LOG", saves.to_str().unwrap()),
                    ("FIRST", first.as_str()),
                    ("SECOND", second.as_str()),
                    ("HISTFILE", "/dev/null"),
                ],
            );
            assert_eq!(
                read_lines(&saves).len(),
                expected,
                "ssh {first}; ssh {second}"
            );
        }
        drop(dir);
    }

    #[test]
    fn the_trap_leaves_dollar_underscore_and_status_as_they_were() {
        let (dir, bin, _) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        let log = dir.path().join("trap");
        let run = |hook: &str, user_trap: &str| {
            let _ = std::fs::remove_file(&log);
            let out = bash(
                r#"eval "$2"
eval "$1"
LOG=$TRAP_LOG
cd /tmp
echo foo lastarg >/dev/null
echo "U=$_"
mkdir -p "$DIR/dd" && echo "M=$_"
false argB
echo "S=$? L=$_""#,
                &[hook, user_trap],
                &[
                    ("TRAP_LOG", log.to_str().unwrap()),
                    ("DIR", dir.path().to_str().unwrap()),
                ],
            );
            (out, read_lines(&log))
        };
        for user_trap in [
            "",
            r#"trap 'printf "%s %s\n" "$?" "$_" >> "${LOG:-/dev/null}"' DEBUG"#,
        ] {
            let without = run("", user_trap);
            let with = run(&snippet, user_trap);
            assert_eq!(with, without, "{user_trap:?}");
            assert!(with.0.starts_with("U=lastarg\nM="), "{with:?}");
            assert!(with.0.ends_with("/dd\nS=1 L=argB\n"), "{with:?}");
            assert_eq!(with.1.is_empty(), user_trap.is_empty(), "{with:?}");
        }
        // This trap sets `$_` itself, so only what it sees can match.
        let echoing = r#"trap 'echo "trap $? $_" >> "${LOG:-/dev/null}"' DEBUG"#;
        let without = run("", echoing).1;
        let with = run(&snippet, echoing).1;
        assert!(without.contains(&"trap 1 argB".to_string()), "{without:?}");
        assert!(with.contains(&"trap 1 argB".to_string()), "{with:?}");
        assert_eq!(with[..3], without[..3]);
        drop(dir);
    }

    #[test]
    fn a_chained_debug_trap_still_skips_commands_under_extdebug() {
        let (dir, bin, _) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        let saves = dir.path().join("saves");
        let out = bash(
            r#"ssh() { echo remote; }
shopt -s extdebug
trap '[[ $BASH_COMMAND != "echo forbidden" ]]' DEBUG
eval "$1"
__revive_save() { echo save >> "$SAVE_LOG"; }
echo forbidden
echo allowed
ssh host
trap -p DEBUG"#,
            &[&snippet],
            &[("SAVE_LOG", saves.to_str().unwrap())],
        );
        assert_eq!(
            out,
            "allowed\nremote\ntrap -- '__revive_debug \"$?\" \"$_\" && : \"$_\"' DEBUG\n"
        );
        // extdebug also runs the trap as `ssh` is entered, still naming `ssh host`.
        assert!(!read_lines(&saves).is_empty());
        drop(dir);
    }

    #[test]
    fn the_relaunch_runs_once_at_the_first_prompt_after_the_whole_rc_file() {
        let (dir, bin, _) = stub();
        let hook = bash_snippet(&config(&["probe"], false), &bin, HINTS);
        let relaunch = bash_relaunch(
            " cd -- '/' && command 'probe' 'host'",
            Some("'probe' 'host'"),
        );
        assert!(syntax_ok(&relaunch));
        let saves = dir.path().join("saves");
        let late = dir.path().join("late");
        std::fs::create_dir(&late).unwrap();
        std::fs::write(
            late.join("probe"),
            "#!/bin/sh\necho \"probe $1 in $PWD with ${LATE-unset}\"\nexit \"${STATUS-0}\"\n",
        )
        .unwrap();
        std::fs::set_permissions(
            late.join("probe"),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let path = format!("{}:{}", late.display(), std::env::var("PATH").unwrap());
        let script = r#"one() { :; }
eval "$3"
eval "$1"
eval "$2"
PATH=$LATE_PATH
export LATE=set
__revive_save() { echo "save $1" >> "$SAVE_LOG"; }
prompt() {
  case "$(declare -p PROMPT_COMMAND 2>/dev/null)" in
    "declare -a"*) for c in "${PROMPT_COMMAND[@]}"; do eval "$c"; done ;;
    *) eval "${PROMPT_COMMAND-}" ;;
  esac
}
printf '%s|' "${PROMPT_COMMAND[@]}"; echo
cd /tmp
prompt
echo "cmd=${__revive_relaunch_cmd-unset}"
printf '%s|' "${PROMPT_COMMAND[@]}"; echo
prompt"#;
        let cases = [
            (
                "PROMPT_COMMAND='echo pc'",
                hook.as_str(),
                "echo pc\n${__revive_relaunch_run:-:} && :\n${__revive_precmd_run:-:} && :|",
                "pc\n",
                2,
            ),
            (
                "PROMPT_COMMAND=(one)",
                hook.as_str(),
                "one|${__revive_relaunch_run:-:} && :|${__revive_precmd_run:-:} && :|",
                "",
                2,
            ),
            ("", "", "${__revive_relaunch_run:-:} && :|", "", 0),
        ];
        for (setup, hook, before, user, prompt_saves) in cases {
            let _ = std::fs::remove_file(&saves);
            let out = bash(
                script,
                &[&relaunch, hook, setup],
                &[
                    ("SAVE_LOG", saves.to_str().unwrap()),
                    ("LATE_PATH", path.as_str()),
                ],
            );
            assert_eq!(
                out,
                // The user's entries run before the relaunch, which comes
                // after them in PROMPT_COMMAND.
                format!("{before}\n{user}probe host in / with set\ncmd=unset\n{before}\n{user}"),
                "{setup}"
            );
            assert_eq!(
                read_lines(&saves),
                vec!["save 0.5"; prompt_saves],
                "{setup}"
            );
        }
        let _ = std::fs::remove_file(&saves);
        bash(
            script,
            &[&relaunch, &hook, "PROMPT_COMMAND='echo pc'"],
            &[
                ("SAVE_LOG", saves.to_str().unwrap()),
                ("LATE_PATH", path.as_str()),
                ("STATUS", "143"),
            ],
        );
        assert_eq!(read_lines(&saves), vec!["save 0.5"], "only the start saves");
        drop(dir);
    }

    #[test]
    fn an_ignored_or_empty_debug_trap_counts_as_none() {
        let (dir, bin, _) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        let saves = dir.path().join("saves");
        for previous in ["", "# note"] {
            let _ = std::fs::remove_file(&saves);
            let out = bash(
                r#"ssh() { :; }
trap "$2" DEBUG
eval "$1"
__revive_save() { echo save >> "$SAVE_LOG"; }
ssh host
trap -p DEBUG"#,
                &[&snippet, previous],
                &[("SAVE_LOG", saves.to_str().unwrap())],
            );
            assert_eq!(
                out.trim(),
                "trap -- '__revive_debug \"$?\" \"$_\" && : \"$_\"' DEBUG",
                "{previous:?}"
            );
            assert_eq!(read_lines(&saves), vec!["save"], "{previous:?}");
        }
        drop(dir);
    }

    #[test]
    fn trap_saves_are_throttled_but_prompt_saves_are_not() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        let env = [("REVIVE_STUB_LOG", log.to_str().unwrap())];
        let out = bash(
            r#"set -eu
ssh() { :; }
eval "$1"
start=$SECONDS
while [ "$SECONDS" = "$start" ]; do :; done
for i in $(seq 50); do ssh host; done
__revive_precmd
echo "pending=${__revive_pending-}"
sleep 1"#,
            &[&snippet],
            &env,
        );
        assert_eq!(out, "pending=\n");
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let lines = read_lines(&log);
        assert_eq!(lines.len(), 3, "trap, deferred trap, prompt: {lines:?}");
        std::fs::remove_file(&log).unwrap();

        bash(
            r#"ssh() { :; }
eval "$1"
start=$SECONDS
while [ "$SECONDS" = "$start" ]; do :; done
ssh host
__revive_precmd
[ "$SECONDS" = "$((start + 1))" ] || echo "crossed a second" >&2"#,
            &[&snippet],
            &env,
        );
        assert_eq!(wait_lines(&log, 2).len(), 2);
        drop(dir);
    }

    #[test]
    fn a_trap_save_carries_the_typed_command_and_a_prompt_save_does_not() {
        let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}|\"");
        let ssh = program("ssh");
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        bash_tty(
            r#"eval "$1"
ssh host -p 22 "a b"
__revive_precmd
sleep 1"#,
            &snippet,
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                ("PATH", &path_with(&ssh)),
            ],
        );
        // Both saves are detached with the same delay, so their order is open.
        let mut lines = wait_lines(&log, 2);
        lines.sort();
        assert_eq!(lines, vec!["ssh host -p 22 \"a b\"|", "|"], "{lines:?}");
        drop(dir);
    }

    #[test]
    fn no_hint_for_a_redirected_group_or_a_function() {
        for (script, why) in [
            ("{ ssh host; } > /dev/null", "the group's redirection"),
            (
                "ssh() { command ssh \"$@\" > /dev/null; }\nssh host",
                "a function",
            ),
            (
                "ssh() { command ssh \"$@\" > /dev/null; }\n'ssh' host",
                "a quoted function name",
            ),
            (
                "ssh() { command ssh \"$@\" > /dev/null; }\n\\ssh host",
                "an escaped function name",
            ),
        ] {
            let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}|\"");
            let ssh = program("ssh");
            let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
            bash_tty(
                &format!("eval \"$1\"\n{script}\nsleep 1"),
                &snippet,
                &[
                    ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                    ("PATH", &path_with(&ssh)),
                ],
            );
            let lines = wait_lines(&log, 1);
            assert!(lines.iter().all(|line| line == "|"), "{why}: {lines:?}");
            drop(dir);
        }
    }

    #[test]
    fn a_hint_is_current_only_while_its_command_is_the_latest() {
        let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}|${REVIVE_SEQ-unset}\"");
        let ssh = program("ssh");
        let hints = dir.path().to_str().unwrap().to_string();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, &hints);
        bash_tty(
            r#"eval "$1"
ssh one && ssh two
sleep 2"#,
            &snippet,
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                ("PATH", &path_with(&ssh)),
            ],
        );
        let mut lines = wait_lines(&log, 2);
        lines.sort();
        // `ssh two` retires the first hint (2), then numbers its own (3).
        assert_eq!(lines, vec!["ssh one|1", "ssh two|3"], "{lines:?}");
        // `ssh two` retired the first hint before numbering its own.
        let pid = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .find_map(|e| {
                e.file_name()
                    .to_str()
                    .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
                    .map(str::to_string)
            })
            .expect("the shell wrote its command number");
        let hints_root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::create_dir_all(hints_dir(hints_root.path())).unwrap();
        std::fs::copy(
            dir.path().join(&pid),
            hints_dir(hints_root.path()).join(&pid),
        )
        .unwrap();
        // The `sleep` after them retired `ssh two`'s hint as well.
        assert!(hint_is_current(hints_root.path(), &pid, "4"));
        assert!(!hint_is_current(hints_root.path(), &pid, "3"));
        assert!(!hint_is_current(hints_root.path(), &pid, "1"));
        assert!(!hint_is_current(hints_root.path(), &pid, ""));
        assert!(!hint_is_current(hints_root.path(), "../x", "4"));
        drop(dir);
    }

    #[test]
    fn a_hint_is_retired_by_the_next_command_or_the_prompt() {
        let (dir, bin, log) = stub();
        let ssh = program("ssh");
        let hints = dir.path().to_str().unwrap().to_string();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, &hints);
        // `myssh` and `cat` do not match, yet each ends the hint before it.
        bash_tty(
            r#"eval "$1"
myssh() { ssh two > /dev/null; }
ssh one; myssh
read -r a < "$REVIVE_HINTS/$$"
ssh three | cat
read -r b < "$REVIVE_HINTS/$$"
ssh four
__revive_precmd
read -r c < "$REVIVE_HINTS/$$"
printf '%s %s %s\n' "$a" "$b" "$c" > "$REVIVE_HINTS/result""#,
            &snippet,
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                ("REVIVE_HINTS", &hints),
                ("PATH", &path_with(&ssh)),
            ],
        );
        let result = std::fs::read_to_string(dir.path().join("result")).unwrap();
        assert_eq!(result.trim(), "2 4 6");
        drop(dir);
    }

    #[test]
    fn prompt_entries_outside_the_hooks_own_are_not_commands() {
        let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}\"");
        let snippet = bash_snippet(&config(&["*"], false), &bin, HINTS);
        // Added by later rc lines, one on each side of __revive_precmd. A
        // prompt then runs them in that order.
        bash_tty(
            r#"__early_hook() { :; }
__later_hook() { :; }
eval "$1"
PROMPT_COMMAND="__early_hook
$PROMPT_COMMAND
__later_hook"
__early_hook
__revive_precmd
__later_hook
sleep 2"#,
            &snippet,
            &[("REVIVE_STUB_LOG", log.to_str().unwrap())],
        );
        let lines = wait_lines(&log, 1);
        assert!(lines.iter().all(|l| !l.contains("_hook")), "{lines:?}");
        drop(dir);
    }

    #[test]
    fn a_command_inside_a_prompt_entry_name_is_still_a_command() {
        // `vi` is part of `__revive_precmd`, but not one of its commands.
        let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}\"");
        let vi = program("vi");
        let snippet = bash_snippet(&config(&["vi"], false), &bin, HINTS);
        bash_tty(
            r#"eval "$1"
vi
sleep 1"#,
            &snippet,
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                ("PATH", &path_with(&vi)),
            ],
        );
        assert_eq!(wait_lines(&log, 1), vec!["vi"]);
        drop(dir);
    }

    #[test]
    fn prompt_entries_still_see_the_commands_pipestatus_and_last_argument() {
        let (dir, bin, log) = stub();
        let seen = |snippet: &str| {
            let out = dir.path().join(format!("seen-{}", snippet.len()));
            let rc = dir.path().join(format!("rc-{}", snippet.len()));
            std::fs::write(
                &rc,
                format!(
                    "PROMPT_COMMAND='printf \"%s|%s\\n\" \"${{PIPESTATUS[*]}}\" \"$_\" >> {}'\n{snippet}\n: rcdone\n",
                    out.display()
                ),
            )
            .unwrap();
            let input = dir.path().join("input");
            std::fs::write(&input, "false | true | (exit 3)\necho a lastword\nexit\n").unwrap();
            let shell = format!("bash --noprofile --rcfile {} -i", rc.display());
            let mut command = Command::new("script");
            if cfg!(target_os = "macos") {
                command.args(["-q", "/dev/null", "bash", "--noprofile", "--rcfile"]);
                command.arg(&rc).arg("-i");
            } else {
                command.args(["-qec", &shell, "/dev/null"]);
            }
            // A pipe that stays open past `exit`: BSD script stops at the
            // end of a file before the shell has read it.
            let mut feeder = Command::new("sh")
                .arg("-c")
                .arg(format!("sleep 0.5; cat '{}'; sleep 1", input.display()))
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let status = command
                .env("REVIVE_STUB_LOG", &log)
                .stdin(feeder.stdout.take().unwrap())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
            feeder.wait().unwrap();
            read_lines(&out)
        };
        let without = seen("");
        let with = seen(&bash_snippet(&config(&["*"], false), &bin, HINTS));
        // And in a pane that got a relaunch, whose entry stays for good.
        let relaunched = seen(&format!(
            "{}{}",
            bash_relaunch("true", None),
            bash_snippet(&config(&["*"], false), &bin, HINTS)
        ));
        // Both rc files end with `: rcdone`, so the first `$_` agrees.
        assert!(
            without.iter().any(|l| l.starts_with("1 0 3|")),
            "{without:?}"
        );
        assert!(without.iter().any(|l| l == "0|lastword"), "{without:?}");
        let major = bash("echo ${BASH_VERSINFO[0]}", &[], &[]);
        if major.trim().parse::<u32>().unwrap() >= 4 {
            assert_eq!(with, without);
            assert_eq!(relaunched, without);
        } else {
            // bash 3.2 resets PIPESTATUS in any DEBUG trap (README).
            let last = |lines: &[String]| -> Vec<String> {
                lines
                    .iter()
                    .map(|l| l.rsplit('|').next().unwrap_or("").to_string())
                    .collect()
            };
            assert_eq!(last(&with), last(&without));
        }
        drop(dir);
    }

    #[test]
    fn a_bash_preexec_line_is_a_hint_only_when_it_is_the_command_running() {
        for (history, expected) in [("ssh new", true), ("ssh old", false)] {
            let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}\"");
            let ssh = program("ssh");
            let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
            // A stand-in for bash-preexec: it hands every preexec function
            // its `history 1` line, which a HISTIGNOREd command leaves stale.
            bash_tty(
                r#"__bp_imported=1; preexec_functions=(); precmd_functions=()
eval "$1"
__fake_bp() { local f; for f in "${preexec_functions[@]}"; do "$f" "$REVIVE_TEST_HISTORY"; done; }
trap '__fake_bp' DEBUG
ssh new
sleep 1"#,
                &snippet,
                &[
                    ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                    ("REVIVE_TEST_HISTORY", history),
                    ("PATH", &path_with(&ssh)),
                ],
            );
            let lines = wait_lines(&log, 1);
            assert!(!lines.is_empty());
            assert_eq!(
                lines.iter().any(|l| l == history),
                expected,
                "{history}: {lines:?}"
            );
            drop(dir);
        }
    }

    #[test]
    fn a_save_runs_in_its_own_process_group_even_before_job_control() {
        // Job control is off while the rc file runs, as in this shell.
        let (dir, bin, log) = stub_logging(
            "\"$(ps -o pgid= -p $$ | tr -d ' ') $(ps -o pgid= -p $REVIVE_SHELL_PID | tr -d ' ')\"",
        );
        let ssh = program("ssh");
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        bash(
            r#"eval "$1"
ssh host
sleep 1"#,
            &[&snippet],
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                ("PATH", &path_with(&ssh)),
            ],
        );
        let lines = wait_lines(&log, 1);
        let groups: Vec<&str> = lines[0].split(' ').collect();
        assert_eq!(groups.len(), 2, "{lines:?}");
        assert_ne!(groups[0], groups[1], "{lines:?}");
        drop(dir);
    }

    #[test]
    fn xtrace_shows_the_users_commands_rather_than_the_hooks() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        let output = Command::new("bash")
            .arg("-c")
            .arg("eval \"$1\"\nset -x\necho one\necho two\nset +x")
            .arg("_")
            .arg(&snippet)
            .env("REVIVE_STUB_LOG", &log)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let trace = String::from_utf8_lossy(&output.stderr);
        // One line naming the hook per command: the trap's own call.
        let hook = trace.lines().filter(|l| l.contains("__revive")).count();
        assert!(hook <= 3, "{trace}");
        assert!(
            trace.contains("+ echo one") && trace.contains("+ echo two"),
            "{trace}"
        );
        drop(dir);
    }

    #[test]
    fn a_builtin_leaves_the_seconds_saves_to_programs() {
        let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}\"");
        let ssh = program("ssh");
        let first = program("first");
        let snippet = bash_snippet(&config(&["*"], false), &bin, HINTS);
        bash_tty(
            r#"eval "$1"
first; cd /tmp; ssh one
sleep 2"#,
            &snippet,
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                (
                    "PATH",
                    &format!("{}:{}", first.path().display(), path_with(&ssh)),
                ),
            ],
        );
        let lines = wait_lines(&log, 2);
        assert!(lines.iter().any(|l| l == "ssh one"), "{lines:?}");
        assert!(lines.iter().all(|l| !l.starts_with("cd")), "{lines:?}");
        drop(dir);
    }

    #[test]
    fn set_u_and_an_empty_prompt_command_stay_quiet() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        // `bash` fails the test on any stderr output.
        bash(
            r#"eval "$1"
PROMPT_COMMAND=
set -u
__revive_preexec "ssh host"
__revive_precmd"#,
            &[&snippet],
            &[("REVIVE_STUB_LOG", log.to_str().unwrap())],
        );
        drop(dir);
    }

    #[test]
    fn xtrace_under_bash_preexec_shows_no_hook_internals() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        let output = Command::new("bash")
            .arg("-c")
            .arg(
                "__bp_imported=1; preexec_functions=(); precmd_functions=()\neval \"$1\"\nset -x\nfor f in \"${preexec_functions[@]}\"; do \"$f\" 'ssh host'; done\nset +x",
            )
            .arg("_")
            .arg(&snippet)
            .env("REVIVE_STUB_LOG", &log)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let trace = String::from_utf8_lossy(&output.stderr);
        let hook = trace.lines().filter(|l| l.contains("__revive")).count();
        assert!(hook <= 2, "{trace}");
        drop(dir);
    }

    #[test]
    fn from_a_sourced_file_the_trap_waits_for_the_prompt_and_chains_the_shells() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        let hook = dir.path().join("hook.sh");
        std::fs::write(&hook, &snippet).unwrap();
        // As ~/.profile runs ~/.bashrc: bash hides the user's trap from the
        // sourced files; the prompt does not. `bash -i` makes the shell
        // interactive, as an rc file's is.
        let outer = dir.path().join("profile.sh");
        std::fs::write(&outer, format!(". '{}'\n", hook.display())).unwrap();
        let output = Command::new("bash")
            .args(["--norc", "-i", "-c"])
            .arg(
                r#"trap ': user' DEBUG
. "$1"
echo "during=$(trap -p DEBUG)"
eval "$PROMPT_COMMAND"
echo "after=$(trap -p DEBUG)"
echo "left=$PROMPT_COMMAND"
declare -f __revive_prev_debug | grep -c ': user'"#,
            )
            .arg("_")
            .arg(&outer)
            .env("REVIVE_STUB_LOG", &log)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&output.stdout);
        assert!(out.contains("during=trap -- ': user' DEBUG"), "{out}");
        assert!(out.contains("after=trap -- '__revive_debug"), "{out}");
        // Its entry took itself out once it had run.
        assert!(!out.contains("__revive_trap_wanted"), "{out}");
        assert!(out.trim_end().ends_with('1'), "{out}");
        drop(dir);
    }

    #[test]
    fn a_deferred_save_takes_the_hint_of_the_command_running_when_it_fires() {
        let (dir, bin, log) = stub();
        let hints = dir.path().to_str().unwrap().to_string();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, &hints);
        let ssh = program("ssh");
        bash_tty(
            r#"eval "$1"
ssh one; ssh two
cp "$REVIVE_HINTS/$$" "$REVIVE_HINTS/during""#,
            &snippet,
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                ("REVIVE_HINTS", &hints),
                ("PATH", &path_with(&ssh)),
            ],
        );
        let hints_root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::create_dir_all(hints_dir(hints_root.path())).unwrap();
        std::fs::copy(
            dir.path().join("during"),
            hints_dir(hints_root.path()).join("1"),
        )
        .unwrap();
        // `cp` retired `ssh two`'s hint: a save now finds no hint at all.
        let (seq, hint) = latest(hints_root.path(), "1").unwrap();
        assert_eq!(hint, "", "{seq}");
        std::fs::write(hints_dir(hints_root.path()).join("1"), "7\nssh two").unwrap();
        assert_eq!(
            latest(hints_root.path(), "1"),
            Some(("7".to_string(), "ssh two".to_string()))
        );
        assert!(hint_is_current(hints_root.path(), "1", "7"));
        assert_eq!(latest(hints_root.path(), "../1"), None);
        drop(dir);
    }

    #[test]
    fn an_entry_appended_after_the_hook_still_sees_the_status() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        // The usual idiom appends to the string, after the hook's entry.
        let out = bash(
            r#"eval "$1"
PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND; }echo seen=\$?"
(exit 5)
eval "$PROMPT_COMMAND""#,
            &[&snippet],
            &[("REVIVE_STUB_LOG", log.to_str().unwrap())],
        );
        assert_eq!(out, "seen=5\n");
        drop(dir);
    }

    #[test]
    fn a_command_inside_a_compound_prompt_entry_keeps_the_shutdown_guard() {
        let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}\"");
        let snippet = bash_snippet(&config(&["*"], false), &bin, HINTS);
        // A prompt runs the entry's inner commands, then the hook's.
        bash_tty(
            r#"eval "$1"
sh -c 'exit 143'
if [ -n "$X" ]; then :; fi
__revive_precmd && :
sleep 2"#,
            &snippet,
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                ("PROMPT_COMMAND", r#"if [ -n "$X" ]; then :; fi"#),
            ],
        );
        let lines = read_lines(&log);
        assert!(!lines.is_empty(), "the trap saves ran");
        assert!(
            lines
                .iter()
                .all(|line| !line.is_empty() && !line.starts_with('[')),
            "{lines:?}"
        );
        drop(dir);
    }

    #[test]
    fn a_job_brought_back_with_fg_saves() {
        let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}\"");
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        // No job control here, so `fg` fails, but the trap has run by then.
        bash_tty(
            r#"eval "$1"
fg 2>/dev/null
sleep 1"#,
            &snippet,
            &[("REVIVE_STUB_LOG", log.to_str().unwrap())],
        );
        assert_eq!(wait_lines(&log, 1), vec![""]);
        drop(dir);
    }

    #[test]
    fn prompt_entries_are_found_past_comments_case_time_and_eval() {
        let (dir, bin, _) = stub();
        let snippet = bash_snippet(&config(&["*"], false), &bin, HINTS);
        let out = bash(
            r#"eval "$1"
PROMPT_COMMAND='true a # note
# a whole comment line
case $TERM in *) true t;; esac
time -p true z
eval "true q"
echo "a #b" >/dev/null
(( n++ ))'
for c in 'true a' 'case $TERM in ' 'true t' 'true z' 'true q' 'echo "a #b" > /dev/null' '(( n++ ))' 'vi'; do
  __revive_is_prompt_entry "$c" && echo "yes $c" || echo "no $c"
done"#,
            &[&snippet],
            &[],
        );
        assert_eq!(
            out,
            "yes true a\nyes case $TERM in \nyes true t\nyes true z\nyes true q\nyes echo \"a #b\" > /dev/null\nyes (( n++ ))\nno vi\n"
        );
        drop(dir);
    }

    #[test]
    fn an_entry_the_hook_cannot_read_still_keeps_the_shutdown_guard() {
        let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}\"");
        let snippet = bash_snippet(&config(&["*"], false), &bin, HINTS);
        // `then` inside a command substitution defeats the text match.
        bash_tty(
            r#"eval "$1"
sh -c 'exit 143'
x=$(if true; then echo a; fi)
__revive_precmd && :
sleep 2"#,
            &snippet,
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                ("PROMPT_COMMAND", "x=$(if true; then echo a; fi)"),
            ],
        );
        let lines = read_lines(&log);
        assert_eq!(
            lines.first().map(String::as_str),
            Some("sh -c 'exit 143'"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .all(|line| !line.is_empty() && !line.starts_with("x=")),
            "{lines:?}"
        );
        drop(dir);
    }

    #[test]
    fn a_child_bash_with_the_exported_entries_runs_nothing() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        // `bash` fails the test on any stderr output.
        let out = bash(
            r#"export PROMPT_COMMAND='history -a'
eval "$1"
bash -c 'false; eval "$PROMPT_COMMAND"; echo "child=$?"'"#,
            &[&snippet],
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                ("HISTFILE", "/dev/null"),
            ],
        );
        assert_eq!(out, "child=0\n");
        drop(dir);
    }

    #[test]
    fn a_kill_status_does_not_stop_saves_when_the_prompt_hook_is_gone() {
        let (dir, bin, _) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        let saves = dir.path().join("saves");
        bash(
            r#"ssh() { return "$1"; }
eval "$1"
PROMPT_COMMAND='history -a'
__revive_save() { echo "save $1" >> "$SAVE_LOG"; }
SECONDS=0
ssh 143
SECONDS=5
ssh 0
SECONDS=10
ssh 0"#,
            &[&snippet],
            &[
                ("SAVE_LOG", saves.to_str().unwrap()),
                ("HISTFILE", "/dev/null"),
            ],
        );
        assert_eq!(read_lines(&saves).len(), 3);
        drop(dir);
    }

    #[test]
    fn a_chained_trap_leaves_a_set_e_shell_alive_after_a_handled_failure() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        let out = bash(
            r#"trap ': user "$?"' DEBUG
eval "$1"
set -e
false || echo survived"#,
            &[&snippet],
            &[("REVIVE_STUB_LOG", log.to_str().unwrap())],
        );
        assert_eq!(out, "survived\n");
        drop(dir);
    }

    #[test]
    fn a_guarded_user_trap_that_ends_false_trips_neither_set_e_nor_err() {
        let (dir, bin, log) = stub();
        let ssh = program("ssh");
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        // At top level `[[ ... ]] && echo` failing is exempt; the hook must
        // keep it so.
        let out = bash(
            r#"trap '[[ -n ${TRACE-} ]] && echo trace' DEBUG
eval "$1"
trap 'echo ERR' ERR
set -e
echo a
ssh host
echo b"#,
            &[&snippet],
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                ("PATH", &path_with(&ssh)),
            ],
        );
        assert_eq!(out, "a\nb\n");
        drop(dir);
    }

    #[test]
    fn no_hint_when_bash_preexec_wraps_the_hooks_trap() {
        let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}\"");
        let ssh = program("ssh");
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        // bash-preexec loaded after the hook: it runs the hook's trap as a
        // preexec function, once per line.
        bash_tty(
            r#"eval "$1"
__bp_imported=1
__wrapped() { __revive_debug 0 _; }
trap '__wrapped' DEBUG
ssh one
sleep 1"#,
            &snippet,
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                ("PATH", &path_with(&ssh)),
            ],
        );
        let lines = wait_lines(&log, 1);
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|l| l.is_empty()), "{lines:?}");
        drop(dir);
    }

    #[test]
    fn a_tool_that_wraps_the_trap_and_calls_it_bare_raises_no_error() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        // As starship's bash init does, loaded after the hook, over a trap
        // the user already had. `bash` fails the test on any stderr output.
        let out = bash(
            r#"trap 'mine=1' DEBUG
eval "$1"
dbg_trap="$(trap -p DEBUG | cut -d' ' -f3 | tr -d \')"
wrap() { local last=$1; $dbg_trap; : "$last"; }
trap 'wrap "$_"' DEBUG
echo a
false || echo b"#,
            &[&snippet],
            &[("REVIVE_STUB_LOG", log.to_str().unwrap())],
        );
        assert_eq!(out, "a\nb\n");
        drop(dir);
    }

    #[test]
    fn a_reprinted_prompt_entry_keeps_the_shutdown_guard() {
        let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}\"");
        let snippet = bash_snippet(&config(&["*"], false), &bin, HINTS);
        // bash reprints `echo   a>/dev/null` as `echo a > /dev/null`; the
        // entry is still recognised with whitespace left out.
        bash_tty(
            r#"eval "$1"
sh -c 'exit 143'
echo   a>/dev/null
__revive_precmd
sleep 2"#,
            &snippet,
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                ("PROMPT_COMMAND", "echo   a>/dev/null"),
            ],
        );
        // No prompt save (its hint is empty) and no save for the entry; the
        // `sleep` after the prompt is a new command and may save.
        let lines = read_lines(&log);
        assert_eq!(
            lines.first().map(String::as_str),
            Some("sh -c 'exit 143'"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .all(|l| !l.is_empty() && !l.starts_with("echo")),
            "{lines:?}"
        );
        drop(dir);
    }

    #[test]
    fn the_prompt_hook_passes_the_status_without_failing_and_installing_saves_nothing() {
        let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}\"");
        let snippet = bash_snippet(&config(&["*"], false), &bin, HINTS);
        // A failing PROMPT_COMMAND entry would end a `set -e` shell after a
        // status errexit lets through, and fire an ERR trap again.
        let out = bash(
            r#"eval "$1"
sleep 1
set -e
false && true
__revive_precmd && :
echo "alive=$?""#,
            &[&snippet],
            &[("REVIVE_STUB_LOG", log.to_str().unwrap())],
        );
        // Alive, and the next entry still sees the command's status.
        assert!(out.ends_with("alive=1\n"), "{out}");
        drop(dir);
        // Installing alone: nothing after the trap, so nothing saves.
        let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}\"");
        let snippet = bash_snippet(&config(&["*"], false), &bin, HINTS);
        bash(
            r#"eval "$1""#,
            &[&snippet],
            &[("REVIVE_STUB_LOG", log.to_str().unwrap())],
        );
        std::thread::sleep(std::time::Duration::from_millis(1200));
        assert!(read_lines(&log).is_empty(), "{:?}", read_lines(&log));
        drop(dir);
    }

    #[test]
    fn a_prompt_command_entry_before_the_hook_keeps_the_shutdown_guard() {
        let (dir, bin, log) = stub_logging("\"${REVIVE_COMMAND-unset}\"");
        let snippet = bash_snippet(&config(&["*"], false), &bin, HINTS);
        bash_tty(
            r#"eval "$1"
sh -c 'exit 143'
true
__revive_precmd
sleep 2"#,
            &snippet,
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                ("PROMPT_COMMAND", "true"),
            ],
        );
        // The prompt after the SIGTERM status (143) saves nothing, although
        // the `true` entry ran in between and matches `*`. Only the prompt
        // save carries an empty command; trap saves carry their line.
        let lines = read_lines(&log);
        assert!(!lines.is_empty(), "the trap saves ran");
        assert!(lines.iter().all(|line| !line.is_empty()), "{lines:?}");
        drop(dir);
    }

    #[test]
    fn a_throttled_trap_save_is_deferred_not_dropped() {
        let (dir, bin, log) = stub_logging("\"$(date +%s.%N)\"");
        let marks = dir.path().join("marks");
        let snippet = bash_snippet(&config(&["ssh"], false), &bin, HINTS);
        // The scenario needs `ssh 0.9` to end inside the second it began; a
        // loaded machine can push it over, which the script reports and the
        // test then runs again rather than judging a different scenario.
        let script = r#"ssh() { sleep "$1"; }
eval "$1"
start=$SECONDS
while [ "$SECONDS" = "$start" ]; do :; done
ssh 0.9
date +%s.%N > "$MARKS"
[ "$SECONDS" = "$((start + 1))" ] || { echo "crossed a second" >&2; exit 3; }
ssh 3
date +%s.%N >> "$MARKS""#;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let _ = std::fs::remove_file(&log);
            let output = Command::new("bash")
                .args(["-c", script, "_", &snippet])
                .env("REVIVE_STUB_LOG", log.to_str().unwrap())
                .env("MARKS", marks.to_str().unwrap())
                .stdin(Stdio::null())
                .output()
                .unwrap();
            if output.status.code() == Some(3) && attempts < 3 {
                std::thread::sleep(std::time::Duration::from_millis(2000));
                continue;
            }
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            break;
        }
        let times = |path: &std::path::Path| -> Vec<f64> {
            read_lines(path)
                .iter()
                .map(|l| l.parse().unwrap())
                .collect()
        };
        let saves = times(&log);
        let marks = times(&marks);
        assert_eq!(saves.len(), 2, "{saves:?}");
        assert!(saves[0] < marks[0], "{saves:?} {marks:?}");
        assert!(
            marks[0] < saves[1] && saves[1] < marks[1],
            "a save lands while `ssh 3` runs: {saves:?} {marks:?}"
        );
        drop(dir);
    }

    #[test]
    fn an_allow_all_loop_schedules_two_trap_saves() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["*"], false), &bin, HINTS);
        bash(
            r#"start=$SECONDS
while [ "$SECONDS" = "$start" ]; do :; done
eval "$1"
for i in $(seq 200); do :; done
sleep 1.8"#,
            &[&snippet],
            &[("REVIVE_STUB_LOG", log.to_str().unwrap())],
        );
        let lines = read_lines(&log);
        assert_eq!(lines.len(), 2, "{lines:?}");
        drop(dir);
    }
}
