use crate::model::Config;

pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
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
        alternatives.push(format!(
            "{}|{}*|*{}|*{}*",
            quote(program),
            quote(&format!("{program} ")),
            quote(&format!(" {program}")),
            quote(&format!(" {program} "))
        ));
        if config.match_program_basename && !program.contains('/') {
            alternatives.push(format!(
                "*{}|*{}*",
                quote(&format!("/{program}")),
                quote(&format!("/{program} "))
            ));
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
__revive_save() {
  ( ( sleep "$1"; REVIVE_SHELL_PID=$$ "$__revive_bin" --host core autosave --force ) >/dev/null 2>&1 & )
}
__revive_save_throttled() {
  if [ "${__revive_stamp-}" != "$SECONDS" ]; then
    __revive_stamp=$SECONDS
    __revive_save 0.5
  elif [ "${__revive_stamp2-}" != "$SECONDS" ]; then
    __revive_stamp2=$SECONDS
    __revive_save 1.5
  fi
}
__revive_preexec() {
  [ "${BASH_SUBSHELL:-0}" = 0 ] || return "${2:-0}"
  if [ -n "${__revive_relaunch_cmd+x}" ]; then
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
  if [ -n "${__revive_pending-}" ] && [ -z "${__revive_rc+x}" ] && [ -n "${__revive_rc0+x}" ]; then
    __revive_rc=$__revive_rc0
  fi
  local __revive_cmd=${1//\'/}
  __revive_cmd=${__revive_cmd//\"/}
  case "$__revive_cmd" in
    __revive_*) ;;
    @PATTERN@) __revive_pending=1; unset __revive_rc; __revive_save_throttled || : ;;
  esac
  return "${2:-0}"
}
__revive_precmd() {
  local __revive_status=$?
  __revive_status=${__revive_rc-$__revive_status}
  unset __revive_rc __revive_rc0
  [ -n "${__revive_pending-}" ] || return 0
  __revive_pending=
  case "$__revive_status" in 129|137|143) return 0 ;; esac
  __revive_save 0.5
  return 0
}
if [ -n "${__bp_imported-}" ] || [ -n "${bash_preexec_imported-}" ]; then
  case " ${preexec_functions[*]-} " in
    *" __revive_preexec "*) ;;
    *) preexec_functions+=(__revive_preexec) ;;
  esac
  case " ${precmd_functions[*]-} " in
    *" __revive_precmd "*) ;;
    *) precmd_functions+=(__revive_precmd) ;;
  esac
else
  __revive_prev=$(trap -p DEBUG)
  case "$__revive_prev" in
    *__revive_debug*|*__revive_preexec*) ;;
    *)
      __revive_prev=${__revive_prev#trap -- \'}
      __revive_prev=${__revive_prev%\' DEBUG}
      __revive_prev=${__revive_prev//\'\\\'\'/\'}
      if [ -n "$__revive_prev" ] && eval "__revive_prev_debug() { $__revive_prev
}" 2>/dev/null; then
        __revive_ret() { return "$1"; }
        __revive_debug() {
          __revive_rc0=$1
          __revive_ret "$1" "$2"; __revive_prev_debug
          local __revive_st=$?
          __revive_preexec "$BASH_COMMAND" "$__revive_st"
        }
      else
        __revive_debug() {
          __revive_rc0=$1
          __revive_preexec "$BASH_COMMAND"
        }
      fi
      trap '__revive_debug "$?" "$_"' DEBUG
      ;;
  esac
  unset __revive_prev
  case "$(declare -p PROMPT_COMMAND 2>/dev/null)" in
    *__revive_precmd*) ;;
    "declare -a"*) PROMPT_COMMAND+=(__revive_precmd) ;;
    *) PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND
}__revive_precmd" ;;
  esac
fi
"#;

const RELAUNCH_HEAD: &str = r#"__revive_relaunch() {
  __revive_relaunch() { return; }
  unset __revive_relaunch_cmd
"#;

const RELAUNCH_TAIL: &str = r#"  __revive_rc=$?
  return "$__revive_rc"
}
case "$(declare -p PROMPT_COMMAND 2>/dev/null)" in
  "declare -a"*) PROMPT_COMMAND=(__revive_relaunch ${PROMPT_COMMAND[@]+"${PROMPT_COMMAND[@]}"}) ;;
  *) PROMPT_COMMAND="__revive_relaunch${PROMPT_COMMAND:+
$PROMPT_COMMAND}" ;;
esac
"#;

/// Runs the relaunched command once, from the first prompt: after the rest of
/// the rc file has run (lines below the marker block often set `PATH`), and
/// with job control on, so the command leads its own process group and
/// capture records it rather than the shell. A command started inside a
/// function never reaches the `DEBUG` trap, so it reports itself to the hook.
/// The entry stays in `PROMPT_COMMAND` as a no-op that keeps `$?`: editing
/// the user's text at run time could leave it unparsable.
pub fn bash_relaunch(command: &str) -> String {
    let quoted = quote(command);
    format!(
        "{RELAUNCH_HEAD}  ! declare -F __revive_preexec >/dev/null || __revive_preexec {quoted}\n{command}\n{RELAUNCH_TAIL}__revive_relaunch_cmd={quoted}\n"
    )
}

pub fn bash_snippet(config: &Config, binary: &str) -> String {
    SNIPPET
        .replace("@BIN@", &quote(binary))
        .replace("@PATTERN@", &bash_pattern(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Config;
    use std::process::{Command, Stdio};

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
        assert_eq!(
            bash_pattern(&config(&["ssh"], false)),
            "'ssh'|'ssh '*|*' ssh'|*' ssh '*"
        );
        assert_eq!(
            bash_pattern(&config(&["ssh"], true)),
            "'ssh'|'ssh '*|*' ssh'|*' ssh '*|*'/ssh'|*'/ssh '*"
        );
        assert_eq!(
            bash_pattern(&config(&["/usr/bin/ssh"], true)),
            "'/usr/bin/ssh'|'/usr/bin/ssh '*|*' /usr/bin/ssh'|*' /usr/bin/ssh '*"
        );
        assert_eq!(quote("it's"), "'it'\\''s'");
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
        let snippet = bash_snippet(&config(&["ssh"], false), &bin);
        assert!(syntax_ok(&snippet));
        assert!(syntax_ok(&bash_snippet(&config(&["*"], true), &bin)));
        assert!(syntax_ok(&bash_snippet(&config(&[], true), &bin)));
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
        let snippet = bash_snippet(&config(&["ssh"], false), &bin);
        let out = bash(
            r#"trap 'echo prev >> "$REVIVE_STUB_LOG"' DEBUG
PROMPT_COMMAND="echo pc"
eval "$1"
: marker
echo "$PROMPT_COMMAND""#,
            &[&snippet],
            &[("REVIVE_STUB_LOG", log.to_str().unwrap())],
        );
        assert_eq!(out.trim(), "echo pc\n__revive_precmd");
        assert!(std::fs::read_to_string(&log).unwrap().contains("prev"));
        let out = bash(
            r#"PROMPT_COMMAND=(one two)
eval "$1"
printf '%s|' "${PROMPT_COMMAND[@]}""#,
            &[&snippet],
            &[],
        );
        assert_eq!(out, "one|two|__revive_precmd|");
        let out = bash(
            r#"__bp_imported=1; preexec_functions=(); precmd_functions=(a)
eval "$1"
echo "${preexec_functions[0]} ${precmd_functions[1]} $(trap -p DEBUG)""#,
            &[&snippet],
            &[],
        );
        assert_eq!(out.trim(), "__revive_preexec __revive_precmd");
        drop(dir);
    }

    #[test]
    fn prompt_commands_ending_in_a_terminator_still_run() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin);
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
        let snippet = bash_snippet(&config(&["ssh"], false), &bin);
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
                "echo pc\n__revive_precmd\ntrap -- '__revive_debug \"$?\" \"$_\"' DEBUG",
            ),
            (
                "PROMPT_COMMAND='echo pc'\ntrap 'printf \"%s\\n\" \"$BASH_COMMAND\" >> \"$USER_LOG\"' DEBUG",
                "echo pc\n__revive_precmd\ntrap -- '__revive_debug \"$?\" \"$_\"' DEBUG",
            ),
            (
                "PROMPT_COMMAND=(one)",
                "one __revive_precmd\ntrap -- '__revive_debug \"$?\" \"$_\"' DEBUG",
            ),
            (
                "__bp_imported=1; preexec_functions=(); precmd_functions=()",
                "__revive_preexec __revive_precmd",
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
        let snippet = bash_snippet(&config(&["ssh"], false), &bin);
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
    fn the_last_matching_command_of_a_line_decides_the_prompt_save() {
        let (dir, bin, _) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin);
        let saves = dir.path().join("saves");
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
        let snippet = bash_snippet(&config(&["ssh"], false), &bin);
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
        let snippet = bash_snippet(&config(&["ssh"], false), &bin);
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
            "allowed\nremote\ntrap -- '__revive_debug \"$?\" \"$_\"' DEBUG\n"
        );
        // extdebug also runs the trap as `ssh` is entered, still naming `ssh host`.
        assert!(!read_lines(&saves).is_empty());
        drop(dir);
    }

    #[test]
    fn the_relaunch_runs_once_at_the_first_prompt_after_the_whole_rc_file() {
        let (dir, bin, _) = stub();
        let hook = bash_snippet(&config(&["probe"], false), &bin);
        let relaunch = bash_relaunch(" cd -- '/' && command 'probe' 'host'");
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
                "__revive_relaunch\necho pc\n__revive_precmd|",
                "pc\n",
                2,
            ),
            (
                "PROMPT_COMMAND=(one)",
                hook.as_str(),
                "__revive_relaunch|one|__revive_precmd|",
                "",
                2,
            ),
            ("", "", "__revive_relaunch|", "", 0),
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
                format!("{before}\nprobe host in / with set\n{user}cmd=unset\n{before}\n{user}"),
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
        let snippet = bash_snippet(&config(&["ssh"], false), &bin);
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
                "trap -- '__revive_debug \"$?\" \"$_\"' DEBUG",
                "{previous:?}"
            );
            assert_eq!(read_lines(&saves), vec!["save"], "{previous:?}");
        }
        drop(dir);
    }

    #[test]
    fn trap_saves_are_throttled_but_prompt_saves_are_not() {
        let (dir, bin, log) = stub();
        let snippet = bash_snippet(&config(&["ssh"], false), &bin);
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
    fn a_throttled_trap_save_is_deferred_not_dropped() {
        let (dir, bin, log) = stub_logging("\"$(date +%s.%N)\"");
        let marks = dir.path().join("marks");
        let snippet = bash_snippet(&config(&["ssh"], false), &bin);
        bash(
            r#"ssh() { sleep "$1"; }
eval "$1"
start=$SECONDS
while [ "$SECONDS" = "$start" ]; do :; done
ssh 0.9
date +%s.%N > "$MARKS"
[ "$SECONDS" = "$((start + 1))" ] || echo "crossed a second" >&2
ssh 3
date +%s.%N >> "$MARKS""#,
            &[&snippet],
            &[
                ("REVIVE_STUB_LOG", log.to_str().unwrap()),
                ("MARKS", marks.to_str().unwrap()),
            ],
        );
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
        let snippet = bash_snippet(&config(&["*"], false), &bin);
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
