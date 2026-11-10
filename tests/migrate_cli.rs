//! migrate through the binary with stand-in `hc` and `herdr-app` scripts on
//! PATH. Nothing reaches the real home, app, core or TUI.

use herdr_revive::layout::{Direction, Node, TabLayout, WorkspaceLayout};
use herdr_revive::migrate::tree::Launch;
use herdr_revive::migrate::tui::Export;
use herdr_revive::model::*;
use herdr_revive::platform::{LOGIN_JOB, LOGIN_PANE};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

const UUID: &str = "123e4567-e89b-12d3-a456-426614174000";
const END: &str = "@@END@@";

const HC: &str = r#"#!/bin/sh
state="$FAKE_STATE"
for a in "$@"; do printf '%s\n' "$a" >> "$state/hc.calls"; done
printf '%s\n' "@@END@@" >> "$state/hc.calls"
case "$1" in
machine) cat "$state/machines.json"; exit 0 ;;
exec) ;;
*) exit 2 ;;
esac
shift 9
program="$1"; shift
case "$program $*" in
"herdr-revive --version") cat "$state/revive-version" ;;
"herdr --version") echo "herdr 0.9.1" ;;
"herdr-revive migrate --export tui"*) cat "$state/export.json" ;;
"herdr-revive migrate --record "*)
    [ -f "$state/record-fails" ] && { echo "herdr-revive: another migration is running" >&2; exit 1; }
    XDG_STATE_HOME="$state/machine-state" exec "$REVIVE_BIN" "$@" ;;
*--dry-run) cat "$state/plan.json" ;;
*--yes) cat "$state/applied.json" ;;
*) echo "unexpected: $program $*" >&2; exit 9 ;;
esac
"#;

/// Answers the way the live app did: `space.new` and `tab.new` report no
/// pane, only `pane.split` does, and `ls` lists every terminal made so far
/// with its pane (call n makes `t<n>` in pane `p<n>`).
const HERDR_APP: &str = r#"#!/bin/sh
state="$FAKE_STATE"
for a in "$@"; do printf '%s\n' "$a" >> "$state/app.calls"; done
printf '%s\n' "@@END@@" >> "$state/app.calls"
n=$(grep -c '^@@END@@$' "$state/app.calls")
made() {
    printf '{"id":"%s","tabs":[{"id":"tb%s","terminals":[{"terminal":"t%s","pane":"p%s"}]}]}\n' \
        "$1" "$n" "$n" "$n" >> "$state/made"
}
case "$1" in
ls)
    [ -f "$state/app-closed" ] && { echo "no window" >&2; exit 1; }
    printf '{"spaces":['
    [ -f "$state/made" ] && paste -sd, "$state/made" | tr -d '\n'
    printf ']}\n'
    ;;
do)
    case "$2" in
    space.new) made "s$n"; printf '{"space":"s%s","tab":"tb%s","terminal":"t%s"}\n' "$n" "$n" "$n" ;;
    tab.new) made "tab$n"; printf '{"space":"s0","tab":"tb%s","terminal":"t%s"}\n' "$n" "$n" ;;
    pane.split)
        [ -f "$state/kill-at-split" ] && kill -9 "$PPID"
        printf '{"pane":"p%s","terminal":"t%s"}\n' "$n" "$n" ;;
    *) exit 2 ;;
    esac
    ;;
*) exit 2 ;;
esac
"#;

struct Fake {
    root: tempfile::TempDir,
}

impl Fake {
    fn new() -> Self {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        for dir in ["bin", "state", "home", "xdg-state"] {
            std::fs::create_dir(root.path().join(dir)).unwrap();
        }
        let fake = Self { root };
        fake.script("hc", HC);
        fake.script("herdr-app", HERDR_APP);
        fake.write(
            "machines.json",
            &json!({"machines": [
                {"name": "canix", "state": "connected", "temporary": false, "host": "canix"}
            ]})
            .to_string(),
        );
        fake.write("revive-version", "herdr-revive 0.3.0\n");
        fake
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
    fn script(&self, name: &str, text: &str) {
        let path = self.path("bin").join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
    }
    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.path("state").join(name), text).unwrap();
    }
    fn revive(&self, args: &[&str]) -> (i32, String, String) {
        self.revive_with(args, &[])
    }
    fn revive_with(&self, args: &[&str], envs: &[(&str, &str)]) -> (i32, String, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_herdr-revive"))
            .args(args)
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.path("bin").display()),
            )
            .env("HOME", self.path("home"))
            .env("XDG_STATE_HOME", self.path("xdg-state"))
            .env("FAKE_STATE", self.path("state"))
            .env("REVIVE_BIN", env!("CARGO_BIN_EXE_herdr-revive"))
            .envs(envs.iter().copied())
            .output()
            .unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }
    fn calls(&self, name: &str) -> Vec<Vec<String>> {
        let Ok(text) = std::fs::read_to_string(self.path("state").join(name)) else {
            return Vec::new();
        };
        let mut calls = vec![Vec::new()];
        for line in text.lines() {
            if line == END {
                calls.push(Vec::new());
            } else if let Some(last) = calls.last_mut() {
                last.push(line.to_string());
            }
        }
        calls.pop();
        calls
    }
    fn app_calls(&self) -> Vec<(String, Value)> {
        self.calls("app.calls")
            .into_iter()
            .map(|call| match call.as_slice() {
                [ls] => (ls.clone(), Value::Null),
                [_, command, args] => (command.clone(), serde_json::from_str(args).unwrap()),
                other => panic!("unexpected herdr-app call {other:?}"),
            })
            .collect()
    }
    fn exec_calls(&self) -> Vec<Vec<String>> {
        self.calls("hc.calls")
            .into_iter()
            .filter(|call| call[0] == "exec")
            .map(|call| call[9..].to_vec())
            .collect()
    }
    fn ledger(&self) -> Value {
        let path = self.path("xdg-state").join("herdr-revive/migrated.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }
}

fn leaf(id: &str) -> Node {
    Node::Pane {
        pane_id: id.into(),
        label: None,
    }
}

/// `[1] work`: tab `build` = p1 | p2 (claude), tab `2` = p3 (ssh).
fn export() -> Export {
    let pane = |tab: &str, id: &str, cwd: &str, command: Option<CommandSpec>| SavedPane {
        workspace_id: "w1".into(),
        tab_id: tab.into(),
        pane_id: id.into(),
        cwd: cwd.into(),
        command,
    };
    let claude = CommandSpec::Agent {
        executable: "claude".into(),
        agent: AgentKind::Claude,
        session_id: UUID.into(),
        session_mode: AgentSessionMode::Resume,
        claude_config_dir: None,
        claude_home: None,
        null_stdio: [false; 3],
    };
    let ssh = CommandSpec::Program {
        argv: vec!["ssh".into(), "host".into()],
    };
    let launch = |cwd: &str, argv: Option<Vec<&str>>| Launch {
        cwd: cwd.into(),
        argv: argv.map(|a| a.into_iter().map(String::from).collect()),
    };
    Export {
        tool: TOOL.into(),
        schema: SCHEMA,
        session: "a".repeat(64),
        snapshot: Snapshot {
            tool: TOOL.into(),
            schema: SCHEMA,
            session: "a".repeat(64),
            created_ms: 1,
            scope: Scope::Session,
            panes: vec![
                pane("w1:t1", "w1:p1", "/srv/w", None),
                pane("w1:t1", "w1:p2", "/srv/w/b c", Some(claude)),
                pane("w1:t2", "w1:p3", "/srv/w", Some(ssh)),
            ],
            layout: vec![WorkspaceLayout {
                workspace_id: "w1".into(),
                label: "[1] work".into(),
                active_tab_id: None,
                tabs: vec![
                    TabLayout {
                        tab_id: "w1:t1".into(),
                        label: "build".into(),
                        focused_pane_id: "w1:p1".into(),
                        root: Node::Split {
                            direction: Direction::Right,
                            ratio: 0.6,
                            first: Box::new(leaf("w1:p1")),
                            second: Box::new(leaf("w1:p2")),
                        },
                    },
                    TabLayout {
                        tab_id: "w1:t2".into(),
                        label: "2".into(),
                        focused_pane_id: "w1:p3".into(),
                        root: leaf("w1:p3"),
                    },
                ],
            }],
            focused_workspace_id: None,
        },
        launches: BTreeMap::from([
            ("w1:p1".to_string(), launch("/srv/w", None)),
            (
                "w1:p2".to_string(),
                launch("/srv/w/b c", Some(vec!["claude", "--resume", UUID])),
            ),
            (
                "w1:p3".to_string(),
                launch("/srv/w", Some(vec!["ssh", "host"])),
            ),
        ]),
        live_agents: vec!["w1:p2".into()],
        denied: vec![],
        tui_copies: vec![],
        terminal_ids: [("w1".to_string(), vec!["term_1".to_string()])]
            .into_iter()
            .collect(),
        missing_folders: BTreeMap::new(),
        app_copies: vec![],
        partial_copies: vec![],
        uncaptured: vec![],
        warnings: vec![],
        live_workspaces: vec![],
    }
}

#[test]
fn migrate_refuses_conflicting_flags() {
    let fake = Fake::new();
    let (code, _, stderr) = fake.revive(&["--host", "core", "migrate", "--to", "app"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("takes no --host"), "{stderr}");
    let (code, _, stderr) = fake.revive(&["migrate", "--to", "app", "--json"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("--json needs --dry-run or --yes"),
        "{stderr}"
    );
    let (code, _, stderr) = fake.revive(&[
        "--socket",
        "/tmp/x.sock",
        "migrate",
        "--to",
        "tui",
        "--session",
        "a",
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("pass one"), "{stderr}");
    let (code, _, stderr) = fake.revive(&[
        "--socket",
        "/tmp/x.sock",
        "migrate",
        "--to",
        "app",
        "--machine",
        "canix",
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("with --machine"), "{stderr}");
    let (code, _, stderr) = fake.revive(&["migrate", "--export", "tui", "--machine", "canix"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("takes no --machine"), "{stderr}");
    let (code, _, stderr) = fake.revive(&["migrate", "--export", "app"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("--export app is not supported"), "{stderr}");
    let (code, _, _) = fake.revive(&["migrate"]);
    assert_eq!(code, 2, "clap usage error without --to");
    let (code, _, _) = fake.revive(&["migrate", "--to", "app", "--export", "tui"]);
    assert_eq!(code, 2, "--to and --export conflict");
    assert!(fake.calls("hc.calls").is_empty());
    assert!(fake.calls("app.calls").is_empty());
}

#[test]
fn a_closed_app_stops_the_plan_before_anything_else() {
    let fake = Fake::new();
    fake.write("app-closed", "");
    let args = ["migrate", "--to", "app", "--machine", "canix", "--dry-run"];
    let (code, _, stderr) = fake.revive(&args);
    assert_eq!(code, 1);
    assert!(stderr.contains("open the Herdr app first"), "{stderr}");
    // migrate uses both hosts, so the variable that picks one is ignored.
    let (code, _, stderr) = fake.revive_with(&args, &[("HERDR_REVIVE_HOST", "neither")]);
    assert_eq!(code, 1);
    assert!(stderr.contains("open the Herdr app first"), "{stderr}");
    assert!(fake.calls("hc.calls").is_empty());
}

#[test]
fn export_reads_the_tui_without_creating_its_state() {
    let fake = Fake::new();
    let state = fake.path("tui-state");
    let socket = fake.path("home").join("none.sock");
    let (code, stdout, stderr) = fake.revive(&[
        "--socket",
        socket.to_str().unwrap(),
        "--state-dir",
        state.to_str().unwrap(),
        "--config-dir",
        fake.path("tui-config").to_str().unwrap(),
        "migrate",
        "--export",
        "tui",
    ]);
    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("cannot read the TUI session"), "{stderr}");
    assert!(!state.exists());
    assert!(!fake.path("tui-config").exists());
    assert!(!fake.path("xdg-state").join("herdr-revive").exists());
    assert!(fake.calls("app.calls").is_empty());
    assert!(fake.calls("hc.calls").is_empty());
}

#[test]
fn a_machine_that_is_not_ready_is_named_at_plan_time() {
    let fake = Fake::new();
    fake.write("revive-version", "herdr-revive 0.2.1\n");
    let (code, _, stderr) =
        fake.revive(&["migrate", "--to", "app", "--machine", "canix", "--dry-run"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("install herdr-revive 0.3.0 or newer on canix"),
        "{stderr}"
    );
    fake.write(
        "machines.json",
        r#"{"machines": [{"name": "canix", "state": "disconnected", "temporary": false}]}"#,
    );
    let (code, _, stderr) =
        fake.revive(&["migrate", "--to", "tui", "--machine", "canix", "--dry-run"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("not connected"), "{stderr}");
    assert!(fake.app_calls().iter().all(|(command, _)| command == "ls"));
}

#[test]
fn a_tui_on_another_machine_is_copied_into_a_space_there() {
    let fake = Fake::new();
    fake.write("export.json", &serde_json::to_string(&export()).unwrap());
    let (code, stdout, stderr) =
        fake.revive(&["migrate", "--to", "app", "--machine", "canix", "--dry-run"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("create     canix: w1 -> \"work\", 2 tabs, 3 panes, 1 agent"),
        "{stdout}"
    );
    assert!(stdout.contains("still live in the TUI: w1:p2"), "{stdout}");
    assert!(stdout.contains("split sizes are not copied"), "{stdout}");
    assert!(fake.app_calls().iter().all(|(command, _)| command == "ls"));

    let (code, stdout, stderr) =
        fake.revive(&["migrate", "--to", "app", "--machine", "canix", "--yes"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.ends_with("done\n"), "{stdout}");
    let execs = fake.exec_calls();
    assert_eq!(execs[0], ["herdr-revive", "--version"]);
    assert_eq!(execs[1], ["herdr", "--version"]);
    assert_eq!(execs[2], ["herdr-revive", "migrate", "--export", "tui"]);
    let call = &fake.calls("hc.calls")[1];
    assert_eq!(
        call[..6],
        ["exec", "--on", "canix", "--timeout", "120", "--"]
    );
    assert_eq!(call[6..8], ["sh", "-c"]);

    // The --yes run: ls (the open window), ls again under the ledger lock
    // (the plan's), then each `space.new`/`tab.new` is followed by an ls
    // that finds the pane of the terminal it made.
    let all = fake.app_calls();
    let second_run = &all[2..];
    assert_eq!(
        second_run,
        [
            ("ls".to_string(), Value::Null),
            ("ls".to_string(), Value::Null),
            (
                "space.new".to_string(),
                json!({"folder": "/srv/w", "name": "work", "tab": "build", "machine": "canix"})
            ),
            ("ls".to_string(), Value::Null),
            (
                "pane.split".to_string(),
                json!({"direction": "right", "pane": "p5",
                       "argv": ["sh", "-c", LOGIN_PANE, "/srv/w/b c", "claude", "--resume", UUID]})
            ),
            (
                "tab.new".to_string(),
                json!({"folder": "/srv/w", "argv": ["sh", "-c", LOGIN_JOB, "/srv/w", "ssh", "host"], "space": "s5"})
            ),
            ("ls".to_string(), Value::Null),
        ]
    );
    let ledger = fake.ledger();
    assert_eq!(ledger["entries"][0]["direction"], "to_app");
    assert_eq!(ledger["entries"][0]["source"]["machine"], "canix");
    assert_eq!(ledger["entries"][0]["created"]["space"], "s5");
    assert_eq!(
        ledger["entries"][0]["source"]["terminal_ids"],
        json!(["term_1"])
    );
    assert_eq!(ledger["entries"][0]["source"]["label"], "work");
    // canix records the copy too, as if it had made it, so a migrate started
    // there (or from a third machine) knows it: first, as soon as space.new
    // made it, as a partial copy, then whole.
    let records: Vec<Vec<String>> = fake
        .exec_calls()
        .into_iter()
        .filter(|call| {
            call.starts_with(&["herdr-revive".into(), "migrate".into(), "--record".into()])
        })
        .collect();
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0].get(4).map(String::as_str), Some("--partial"));
    let pending: Value = serde_json::from_str(&records[0][3]).unwrap();
    assert_eq!(pending["created"]["space"], "s5");
    assert_eq!(pending["created"]["unknown"], true);
    assert_eq!(records[1].len(), 4, "the whole copy");
    let theirs: Value = serde_json::from_str(
        &std::fs::read_to_string(fake.path("state/machine-state/herdr-revive/migrated.json"))
            .unwrap(),
    )
    .unwrap();
    let entry = &theirs["entries"][0];
    assert_eq!(entry["direction"], "to_app");
    assert_eq!(entry["source"]["machine"], Value::Null);
    assert_eq!(entry["source"]["session"], "a".repeat(64));
    assert_eq!(entry["source"]["workspace_id"], "w1");
    assert_eq!(entry["created"], ledger["entries"][0]["created"]);
    assert_eq!(entry["at"], ledger["entries"][0]["at"]);
    assert_eq!(theirs["partial"], json!([]), "the whole copy replaced it");

    let (code, stdout, _) = fake.revive(&["migrate", "--to", "app", "--machine", "canix", "--yes"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("unchanged  canix: w1"), "{stdout}");
    assert!(stdout.ends_with("nothing to copy\n"), "{stdout}");
}

#[test]
fn a_run_killed_part_way_leaves_the_machine_its_partial_copy() {
    let fake = Fake::new();
    fake.write("export.json", &serde_json::to_string(&export()).unwrap());
    fake.write("kill-at-split", "");
    let (code, _, _) = fake.revive(&["migrate", "--to", "app", "--machine", "canix", "--yes"]);
    assert_eq!(code, -1, "killed");
    // Killed at the first pane.split after space.new made s3: canix was told
    // of s3 before that, so its own plans and a third machine's know it.
    let record = fake.exec_calls().into_iter().last().unwrap();
    assert_eq!(record[..3], ["herdr-revive", "migrate", "--record"]);
    assert_eq!(record.get(4).map(String::as_str), Some("--partial"));
    let theirs: Value = serde_json::from_str(
        &std::fs::read_to_string(fake.path("state/machine-state/herdr-revive/migrated.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(theirs["entries"], json!([]));
    let partial = &theirs["partial"][0];
    assert_eq!(partial["source"]["machine"], Value::Null);
    assert_eq!(partial["source"]["workspace_id"], "w1");
    assert_eq!(partial["created"]["space"], "s3");
    assert_eq!(partial["created"]["unknown"], true);
    let mine = &fake.ledger()["partial"][0];
    assert_eq!(
        mine["created"]["space"], "s3",
        "as this stand-in app names it"
    );
    assert_eq!(mine["at"], partial["at"]);
}

#[test]
fn a_copy_the_machine_cannot_record_stands_with_a_warning() {
    let fake = Fake::new();
    fake.write("export.json", &serde_json::to_string(&export()).unwrap());
    fake.write("record-fails", "");
    let (code, stdout, stderr) =
        fake.revive(&["migrate", "--to", "app", "--machine", "canix", "--yes"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.ends_with("done\n"), "{stdout}");
    assert!(
        stderr.contains(
            "herdr-revive: warning: copied w1 into space s3 on canix, but canix did not record \
             it (another migration is running); a migrate started on canix will not know that \
             s3 is a copy of w1; to record it, run on canix: herdr-revive migrate --record \
             '{\"direction\":\"to_app\",\"source\":{\"machine\":null,"
        ),
        "{stderr}"
    );
    assert_eq!(fake.ledger()["entries"][0]["created"]["space"], "s3");
}

#[test]
fn the_json_report_carries_the_warning_the_dialog_shows() {
    let fake = Fake::new();
    fake.write("export.json", &serde_json::to_string(&export()).unwrap());
    fake.write("record-fails", "");
    let (code, stdout, stderr) = fake.revive(&[
        "migrate",
        "--to",
        "app",
        "--machine",
        "canix",
        "--yes",
        "--json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    let warnings = report["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 2, "{report}");
    assert!(
        warnings[0]
            .as_str()
            .unwrap()
            .starts_with("canix did not record space s3, the copy of w1 being built"),
        "{report}"
    );
    assert!(
        warnings[1]
            .as_str()
            .unwrap()
            .starts_with("copied w1 into space s3 on canix, but canix did not record it"),
        "{report}"
    );
    assert!(!stderr.contains("did not record"), "{stderr}");
}

#[test]
fn record_refuses_every_other_migrate_flag_and_writes_nothing() {
    let fake = Fake::new();
    let entry = json!({"direction": "to_app",
        "source": {"machine": null, "session": "a".repeat(64), "workspace_id": "w1"},
        "created": {"space": "s3", "terminals": []}, "at": 7})
    .to_string();
    for extra in [
        &["--dry-run"][..],
        &["--yes"],
        &["--json"],
        &["--only", "zz"],
        &["--session", "nope"],
        &["--to", "app"],
        &["--export", "tui"],
    ] {
        let mut args = vec!["migrate", "--record", &entry];
        args.extend_from_slice(extra);
        let (code, _, stderr) = fake.revive(&args);
        assert_ne!(code, 0, "{extra:?}");
        assert!(
            stderr.contains("cannot be used with"),
            "{extra:?}: {stderr}"
        );
        assert!(
            !fake.path("xdg-state/herdr-revive/migrated.json").exists(),
            "{extra:?} wrote the ledger"
        );
    }
    let (code, _, stderr) = fake.revive(&["migrate", "--to", "app", "--partial"]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("--partial goes with --record only"),
        "{stderr}"
    );
    let (code, _, stderr) = fake.revive(&["migrate", "--record", &entry, "--partial"]);
    assert_eq!(code, 0, "{stderr}");
    let ledger = fake.ledger();
    assert_eq!(ledger["entries"], json!([]));
    assert_eq!(ledger["partial"][0]["created"]["space"], "s3");
}

#[test]
fn record_stores_a_copy_into_this_machines_app_and_refuses_anything_else() {
    let fake = Fake::new();
    let entry = json!({"direction": "to_app",
        "source": {"machine": null, "session": "a".repeat(64), "workspace_id": "w1",
                   "terminal_ids": ["term_1"], "label": "work"},
        "created": {"space": "s3", "terminals": ["t1"]}, "at": 7});
    let (code, stdout, stderr) = fake.revive(&["migrate", "--record", &entry.to_string()]);
    assert_eq!((code, stdout.as_str()), (0, ""), "{stderr}");
    assert_eq!(fake.ledger()["entries"][0], entry);
    let mut far = entry.clone();
    far["source"]["machine"] = json!("canix");
    for (bad, why) in [
        (far.to_string(), "a copy into this machine's app"),
        ("{".to_string(), "takes a ledger entry"),
    ] {
        let (code, _, stderr) = fake.revive(&["migrate", "--record", &bad]);
        assert_eq!(code, 1, "{stderr}");
        assert!(stderr.contains(why), "{stderr}");
    }
    let (code, _, stderr) = fake.revive(&[
        "migrate",
        "--record",
        &entry.to_string(),
        "--machine",
        "canix",
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("takes no --machine"), "{stderr}");
    assert_eq!(fake.ledger()["entries"].as_array().unwrap().len(), 1);
}

#[test]
fn app_spaces_on_another_machine_are_copied_by_its_own_herdr_revive() {
    let fake = Fake::new();
    let line = json!({"machine": null, "source": "s1", "status": "create", "reason": null,
        "name": "api", "tabs": 2, "panes": 3, "agents": 1, "denied": 0, "mod_blocks": 1,
        "live_agents": [], "copy": null, "partial": null});
    let summary = json!({"create": 1, "skip": 0, "unchanged": 0, "ratios_lost": false});
    fake.write(
        "plan.json",
        &json!({"to": "tui", "machine": null, "lines": [line],
        "summary": summary, "applied": []})
        .to_string(),
    );
    fake.write(
        "applied.json",
        &json!({"to": "tui", "machine": null, "lines": [line],
        "summary": summary, "applied": [{"source": "s1",
        "created": {"session": "b", "workspace_id": "w4"}}]})
        .to_string(),
    );
    let (code, stdout, stderr) =
        fake.revive(&["migrate", "--to", "tui", "--machine", "canix", "--yes"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("create     canix: s1 -> \"api\""),
        "{stdout}"
    );
    assert!(
        stdout.contains("copied     s1 -> TUI workspace w4"),
        "{stdout}"
    );
    let execs = fake.exec_calls();
    assert_eq!(
        execs[2],
        [
            "herdr-revive",
            "migrate",
            "--to",
            "tui",
            "--json",
            "--dry-run"
        ]
    );
    assert_eq!(
        execs[3],
        [
            "herdr-revive",
            "migrate",
            "--to",
            "tui",
            "--json",
            "--only",
            "s1",
            "--yes"
        ]
    );
    assert!(fake.calls("app.calls").is_empty());
}
