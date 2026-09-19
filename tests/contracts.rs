use anyhow::{Result, bail};
use herdr_revive::{
    app, capture,
    model::*,
    planner, platform, store,
    transport::{self, Host},
};
use serde_json::json;
use std::path::Path;

fn config() -> Config {
    Config {
        allowed_programs: vec![
            "ssh".into(),
            "minicom".into(),
            "codex".into(),
            "claude".into(),
        ],
        settle_ms: 0,
        ..Config::default()
    }
}

#[test]
fn null_output_wrapper_preserves_literal_arguments_and_checks_original_policy() {
    let args = [
        "/usr/bin/printf",
        "%s\\0",
        "",
        "space here",
        "'\"$;|",
        "$(exit 7)",
    ];
    let command = CommandSpec::ProgramNullStdio {
        argv: args.iter().map(|s| (*s).into()).collect(),
        null_stdio: [false, false, true],
    };
    let mut config = config();
    config.allowed_programs = vec!["/bin/sh".into()];
    assert!(command.allowed_argv(&config).is_err());
    config.allowed_programs = vec!["/usr/bin/printf".into()];
    let launch = command.allowed_argv(&config).unwrap();
    let output = std::process::Command::new(&launch[0])
        .args(&launch[1..])
        .output()
        .unwrap();
    assert!(output.status.success());
    let expected: Vec<u8> = args[2..]
        .iter()
        .flat_map(|s| s.bytes().chain([0]))
        .collect();
    assert_eq!(output.stdout, expected);
    let unsupported = CommandSpec::ProgramNullStdio {
        argv: vec!["sleep".into(), "1".into()],
        null_stdio: [true, false, false],
    };
    assert!(unsupported.argv().is_err());
}

#[test]
fn legacy_ssh_null_output_specs_are_rejected_before_restore() {
    for name in ["ssh", "/usr/bin/ssh"] {
        let command = CommandSpec::ProgramNullStdio {
            argv: vec![name.into(), "fixture.invalid".into()],
            null_stdio: [false, true, false],
        };
        assert!(
            command
                .argv()
                .unwrap_err()
                .to_string()
                .contains("SSH output redirection")
        );
    }
    assert!(platform::ssh_terminal_stdout(std::process::id()).is_err());
}
fn saved() -> Snapshot {
    Snapshot {
        tool: TOOL.into(),
        schema: SCHEMA,
        session: "a".repeat(64),
        created_ms: 1,
        scope: Scope::Session,
        layout: vec![],
        focused_workspace_id: None,
        panes: vec![SavedPane {
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            pane_id: "w1:p1".into(),
            cwd: "/tmp".into(),
            command: Some(CommandSpec::Program {
                argv: vec!["ssh".into(), "fixture.invalid".into()],
            }),
        }],
    }
}
fn live() -> LiveSnapshot {
    serde_json::from_value(json!({"version":"0.9.1", "protocol":22,
        "workspaces":[{"workspace_id":"w1"}], "tabs":[{"workspace_id":"w1","tab_id":"w1:t1"}],
        "panes":[{"workspace_id":"w1","tab_id":"w1:t1","pane_id":"w1:p1","terminal_id":"term_fixture","cwd":"/tmp"}]})).unwrap()
}

#[test]
fn plans_match_ids_and_ignore_array_order() {
    let saved = saved();
    let mut live = live();
    let p = planner::plan(&saved, &saved.session, &live, &config()).unwrap();
    assert_eq!(p.entries[0].decision, planner::Decision::Candidate);
    live.panes[0].pane_id = "w1:p9".into();
    assert_eq!(
        planner::plan(&saved, &saved.session, &live, &config())
            .unwrap()
            .entries[0]
            .decision,
        planner::Decision::MissingPane
    );
    live.panes[0].pane_id = "w1:p1".into();
    live.tabs[0].tab_id = "w1:t9".into();
    live.panes[0].tab_id = "w1:t9".into();
    assert_eq!(
        planner::plan(&saved, &saved.session, &live, &config())
            .unwrap()
            .entries[0]
            .decision,
        planner::Decision::IdentityMismatch
    );
}

#[test]
fn current_policy_and_session_are_required() {
    let saved = saved();
    assert!(planner::plan(&saved, &"b".repeat(64), &live(), &config()).is_err());
    assert_eq!(
        planner::plan(&saved, &saved.session, &live(), &Config::default())
            .unwrap()
            .entries[0]
            .decision,
        planner::Decision::DeniedByPolicy
    );
}

#[test]
fn malformed_native_snapshots_are_rejected() {
    let mut value = serde_json::to_value(saved()).unwrap();
    for key in ["restorable", "unknown"] {
        value[key] = json!(true);
        assert!(serde_json::from_value::<Snapshot>(value.clone()).is_err());
        value.as_object_mut().unwrap().remove(key);
    }
    for schema in [0, 2, u32::MAX] {
        let mut s = saved();
        s.schema = schema;
        assert!(s.validate().is_err());
    }
    let mut s = saved();
    s.panes.push(s.panes[0].clone());
    assert!(s.validate().is_err());
    let mut s = saved();
    s.panes[0].tab_id = "w2:t1".into();
    assert!(s.validate().is_err());
    let mut s = saved();
    s.panes[0].cwd = "relative".into();
    assert!(s.validate().is_err());
}

#[test]
fn config_fails_closed_on_unknown_fields_and_invalid_limits() {
    assert!(toml::from_str::<Config>("autoRestore = true").is_err());
    let mut cfg = config();
    cfg.retention = 0;
    assert!(cfg.validate().is_err());
    cfg = config();
    cfg.timeout_ms = 0;
    assert!(cfg.validate().is_err());
    cfg = config();
    cfg.allowed_programs.push("../ssh".into());
    assert!(cfg.validate().is_err());
}

#[test]
fn exact_agent_references_only() {
    let id = "01234567-89ab-cdef-0123-456789abcdef";
    for invalid in [
        "",
        "latest",
        "../sessions/123",
        "--continue",
        "01234567-89ab-cdef-0123-456789abcdeg",
    ] {
        assert!(validate_session_id(invalid).is_err());
    }
    for (agent, executable, flag) in [
        ("claude", "claude", "--resume"),
        ("codex", "codex", "resume"),
        ("gemini", "gemini", "--resume"),
        ("copilot", "copilot", "--resume"),
        ("cursor", "cursor-agent", "--resume"),
    ] {
        let argv = vec![executable.into(), flag.into(), id.into()];
        assert_eq!(
            capture::command_for(&argv, None)
                .unwrap()
                .unwrap()
                .argv()
                .unwrap(),
            argv
        );
        assert!(capture::command_for(&[executable.into(), "--continue".into()], None).is_err());
        let native = AgentSession {
            source: format!("herdr:{agent}"),
            agent: agent.into(),
            kind: "id".into(),
            value: id.into(),
        };
        assert_eq!(
            capture::command_for(&[executable.into()], Some(&native))
                .unwrap()
                .unwrap()
                .argv()
                .unwrap(),
            argv
        );
        let mut bad = native;
        bad.source = "user:guess".into();
        assert!(capture::command_for(&[executable.into()], Some(&bad)).is_err());
        assert!(CommandSpec::Program { argv }.argv().is_err());
    }
}

#[test]
fn node_agent_wrappers_require_exact_ids_and_cannot_be_plain_programs() {
    for (agent, script) in [
        ("codex", "/fixture/node_modules/@openai/codex/bin/codex.js"),
        (
            "claude",
            "/fixture/node_modules/@anthropic-ai/claude-code/cli.js",
        ),
    ] {
        let argv = vec!["node".into(), script.into(), "--continue".into()];
        assert!(capture::command_for(&argv, None).is_err());
        assert!(CommandSpec::Program { argv: argv.clone() }.argv().is_err());
        let native = AgentSession {
            source: format!("herdr:{agent}"),
            agent: agent.into(),
            kind: "id".into(),
            value: "01234567-89ab-cdef-0123-456789abcdef".into(),
        };
        let command = capture::command_for(&argv, Some(&native)).unwrap().unwrap();
        let restored = command.allowed_argv(&config()).unwrap();
        assert_eq!(restored[0], agent);
        assert_eq!(restored[2], native.value);
        assert!(!restored.contains(&"--continue".into()));
    }
    let mut cfg = config();
    cfg.allowed_programs.push("node".into());
    assert!(cfg.validate().is_ok());
    assert!(capture::command_for(&["node".into(), "server.js".into()], None).is_ok());
}

#[test]
fn pipelines_are_rejected_but_leader_descendants_are_allowed() {
    use platform::Process;
    let leader = Process {
        pid: 10,
        parent: 1,
        group: 10,
        start: 1,
    };
    let descendant = Process {
        pid: 11,
        parent: 10,
        group: 10,
        start: 2,
    };
    let mut table = std::collections::HashMap::from([(10, leader.clone()), (11, descendant)]);
    platform::validate_foreground_tree(&table, &leader).unwrap();
    table.insert(
        12,
        Process {
            pid: 12,
            parent: 1,
            group: 10,
            start: 3,
        },
    );
    assert!(platform::validate_foreground_tree(&table, &leader).is_err());
}

#[test]
fn arbitrary_argv_roundtrips_in_real_posix_shells() {
    let mut args = vec![
        "printf".into(),
        "%s\\0".into(),
        "".into(),
        "two words".into(),
        "'\"$;|&(){}*?`\\".into(),
        "中文測試".into(),
        "$(touch NEVER_CREATED)".into(),
    ];
    let alphabet = ["a", "'", " ", "$", "\\", "\"", ";", "|", "漢"];
    for i in 0..81 {
        args.push(format!(
            "{}{}{}",
            alphabet[i % 9],
            alphabet[(i / 9) % 9],
            alphabet[(i * 7) % 9]
        ));
    }
    let cwd = tempfile::tempdir().unwrap();
    for shell in ["bash", "dash", "zsh"] {
        let executable = Path::new("/bin").join(shell);
        if !executable.exists() {
            continue;
        }
        let text = platform::shell_command(shell, cwd.path().to_str().unwrap(), &args).unwrap();
        let output = std::process::Command::new(&executable)
            .args(["-c", &text])
            .output()
            .unwrap();
        assert!(output.status.success(), "{shell}");
        let expected: Vec<u8> = args[2..]
            .iter()
            .flat_map(|s| s.bytes().chain([0]))
            .collect();
        assert_eq!(output.stdout, expected, "{shell}");
        assert!(!cwd.path().join("NEVER_CREATED").exists());
    }
}

#[test]
fn terminal_controls_and_unsupported_shells_fail_closed() {
    for arg in ["hello\nworld", "\r", "\x1b[201~", "\0", "\t", "\u{7f}"] {
        assert!(platform::shell_command("bash", "/tmp", &["ssh".into(), arg.into()]).is_err());
    }
    for shell in ["fish", "pwsh", "cmd", "unknown"] {
        assert!(platform::shell_command(shell, "/tmp", &["ssh".into()]).is_err());
    }
    assert!(platform::decode_argv(b"ssh\0\xff\0").is_err());
    assert!(platform::decode_argv(b"ssh\0truncated").is_err());
    assert_eq!(
        platform::decode_argv(b"ssh\0\0x\0").unwrap(),
        ["ssh", "", "x"]
    );
}

#[test]
fn deterministic_planning_for_fixture_sizes() {
    for count in [1, 10, 50, 100] {
        let mut s = saved();
        let mut l = live();
        let template = s.panes[0].clone();
        let live_template = l.panes[0].clone();
        s.panes = (1..=count)
            .map(|i| {
                let mut p = template.clone();
                p.pane_id = format!("w1:p{i}");
                p
            })
            .collect();
        l.panes = (1..=count)
            .rev()
            .map(|i| {
                let mut p = live_template.clone();
                p.pane_id = format!("w1:p{i}");
                p
            })
            .collect();
        let first =
            serde_json::to_value(planner::plan(&s, &s.session, &l, &config()).unwrap()).unwrap();
        s.panes.reverse();
        l.panes.reverse();
        assert_eq!(
            first,
            serde_json::to_value(planner::plan(&s, &s.session, &l, &config()).unwrap()).unwrap()
        );
    }
}

#[test]
fn duplicate_live_ids_and_orphans_are_errors() {
    let mut l = live();
    l.panes.push(l.panes[0].clone());
    assert!(planner::index_live(&l).is_err());
    l = live();
    l.tabs.clear();
    assert!(planner::index_live(&l).is_err());
    l = live();
    l.workspaces.clear();
    assert!(planner::index_live(&l).is_err());
}

#[test]
fn response_envelopes_are_checked_and_redacted() {
    assert!(
        transport::decode_response(br#"{"id":"wrong","result":{"type":"ok"}}"#, "right", "ok")
            .is_err()
    );
    let error = transport::decode_response(
        br#"{"id":"right","error":{"message":"SECRET"}}"#,
        "right",
        "ok",
    )
    .unwrap_err();
    assert!(!format!("{error:#}").contains("SECRET"));
    assert!(
        transport::decode_response(br#"{"id":"right","result":{"type":"pong"}}"#, "right", "ok")
            .is_err()
    );
    assert!(validate_protocol("0.9.1", 23).is_err());
    assert!(validate_protocol("9.0.0", 22).is_err());
}

#[test]
fn private_atomic_storage_and_retention() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let s = store::Store::new(temp.path(), saved().session).unwrap();
    let _lock = s.try_lock().unwrap().unwrap();
    assert!(s.try_lock().unwrap().is_none());
    for time in 1..=5 {
        let mut snapshot = saved();
        snapshot.created_ms = time;
        s.save(&snapshot, None, 2).unwrap();
    }
    assert_eq!(s.list("snapshots").unwrap().len(), 2);
    assert_eq!(
        s.read_snapshot(&s.snapshot_path(None).unwrap())
            .unwrap()
            .created_ms,
        5
    );
    assert_eq!(
        std::fs::metadata(s.snapshot_path(None).unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let mut bad = saved();
    bad.schema = 99;
    assert!(s.save(&bad, None, 2).is_err());
    assert_eq!(
        s.read_snapshot(&s.snapshot_path(None).unwrap())
            .unwrap()
            .created_ms,
        5
    );
    std::fs::write(s.root.join(".interrupted-write.tmp"), b"partial").unwrap();
    assert_eq!(
        s.read_snapshot(&s.snapshot_path(None).unwrap())
            .unwrap()
            .created_ms,
        5
    );
}

#[test]
fn symlinks_and_oversized_state_are_rejected() {
    use std::os::unix::fs::symlink;
    let t = tempfile::tempdir().unwrap();
    let target = t.path().join("target");
    std::fs::write(&target, b"safe").unwrap();
    let link = t.path().join("link");
    symlink(&target, &link).unwrap();
    assert!(store::read_bytes(&link).is_err());
    assert!(store::atomic_write(&link, b"unsafe").is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"safe");
    let big = t.path().join("large");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(MAX_BYTES as u64 + 1)
        .unwrap();
    assert!(store::read_bytes(&big).is_err());
    for name in ["..", "../escape", "x/y", "", "a.b"] {
        assert!(store::validate_name(name).is_err());
    }
}

#[test]
fn permission_failure_preserves_previous_file() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.json");
    store::atomic_write(&path, b"previous").unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = store::atomic_write(&path, b"replacement");
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"previous");
}

#[test]
fn session_paths_normalize_parent_aliases_but_separate_sessions() {
    use std::os::unix::fs::symlink;
    let t = tempfile::tempdir().unwrap();
    let actual = t.path().join("actual");
    std::fs::create_dir(&actual).unwrap();
    symlink(&actual, t.path().join("alias")).unwrap();
    assert_eq!(
        store::session_identity(&actual.join("socket")).unwrap(),
        store::session_identity(&t.path().join("alias/socket")).unwrap()
    );
    assert_ne!(
        store::session_identity(&actual.join("socket")).unwrap().1,
        store::session_identity(&actual.join("other")).unwrap().1
    );
    assert!(store::session_identity(Path::new("relative.sock")).is_err());
}

struct Fake {
    runs: usize,
    fail: bool,
}
impl Host for Fake {
    fn snapshot(&mut self) -> Result<LiveSnapshot> {
        Ok(live())
    }
    fn process_info(&mut self, _: &str) -> Result<ProcessInfo> {
        bail!("unused")
    }
    fn pane(&mut self, _: &str) -> Result<LivePane> {
        Ok(live().panes.remove(0))
    }
    fn run(&mut self, _: &str, _: &str) -> Result<()> {
        self.runs += 1;
        if self.fail {
            bail!("ambiguous timeout");
        }
        Ok(())
    }
}

#[test]
fn one_claim_per_boot_and_no_retry_after_ambiguous_failure() {
    for fail in [false, true] {
        let t = tempfile::tempdir().unwrap();
        let s = saved();
        let store = store::Store::new(t.path(), s.session.clone()).unwrap();
        let _lock = store.try_lock().unwrap().unwrap();
        let plan = planner::plan(&s, &s.session, &live(), &config()).unwrap();
        let mut host = Fake { runs: 0, fail };
        let generation = "b".repeat(64);
        let result = app::execute(&mut host, &store, &s, &plan, &generation, |_, _, _, _| {
            Ok(Some("safe fixture".into()))
        });
        assert_eq!(result.is_err(), fail);
        assert_eq!(host.runs, 1);
        assert!(
            app::execute(&mut host, &store, &s, &plan, &generation, |_, _, _, _| Ok(
                Some("duplicate".into())
            ))
            .is_err()
        );
        assert_eq!(host.runs, 1);
        if fail {
            let mut evidence = app::pending(&store).unwrap().unwrap();
            assert_eq!(evidence.state, app::BootState::Failed);
            assert!(app::require_clear(&store).is_err());
            evidence.state = app::BootState::Done;
            evidence.acknowledged = true;
            app::finish(&store, &evidence).unwrap();
            assert!(app::pending(&store).unwrap().is_none());
            assert!(
                app::boot(&store, &generation)
                    .unwrap()
                    .unwrap()
                    .acknowledged
            );
        }
    }
}

#[test]
fn crash_before_run_preserves_sending_evidence() {
    let t = tempfile::tempdir().unwrap();
    let s = saved();
    let store = store::Store::new(t.path(), s.session.clone()).unwrap();
    let plan = planner::plan(&s, &s.session, &live(), &config()).unwrap();
    let mut host = Fake {
        runs: 0,
        fail: false,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = app::execute(
            &mut host,
            &store,
            &s,
            &plan,
            &"b".repeat(64),
            |_, _, _, _| panic!("simulated crash"),
        );
    }));
    assert!(result.is_err());
    assert_eq!(host.runs, 0);
    assert_eq!(
        app::pending(&store).unwrap().unwrap().entries[0].outcome,
        app::Outcome::Sending
    );
    assert!(app::require_clear(&store).is_err());
}

#[test]
fn busy_pane_is_skipped_and_never_replayed_in_same_boot() {
    let t = tempfile::tempdir().unwrap();
    let s = saved();
    let store = store::Store::new(t.path(), s.session.clone()).unwrap();
    let plan = planner::plan(&s, &s.session, &live(), &config()).unwrap();
    let mut host = Fake {
        runs: 0,
        fail: false,
    };
    let record = app::execute(
        &mut host,
        &store,
        &s,
        &plan,
        &"b".repeat(64),
        |_, _, _, _| Ok(None),
    )
    .unwrap();
    assert_eq!(record.entries[0].outcome, app::Outcome::Skipped);
    assert_eq!(host.runs, 0);
}

#[test]
fn partial_restore_stops_at_failure_and_preserves_all_outcomes() {
    let temp = tempfile::tempdir().unwrap();
    let mut saved = saved();
    let mut live = live();
    let pane = saved.panes[0].clone();
    let current = live.panes[0].clone();
    for i in 2..=3 {
        let mut next = pane.clone();
        next.pane_id = format!("w1:p{i}");
        saved.panes.push(next);
        let mut next = current.clone();
        next.pane_id = format!("w1:p{i}");
        live.panes.push(next);
    }
    let store = store::Store::new(temp.path(), saved.session.clone()).unwrap();
    let plan = planner::plan(&saved, &saved.session, &live, &config()).unwrap();
    let mut host = Fake {
        runs: 0,
        fail: false,
    };
    let result = app::execute(
        &mut host,
        &store,
        &saved,
        &plan,
        &"b".repeat(64),
        |_, pane, _, _| {
            if pane.pane_id == "w1:p2" {
                bail!("pane disappeared before execution");
            }
            Ok(Some("safe fixture".into()))
        },
    );
    assert!(result.is_err());
    assert_eq!(host.runs, 1);
    let pending = app::pending(&store).unwrap().unwrap();
    assert_eq!(
        pending
            .entries
            .iter()
            .map(|e| e.outcome.clone())
            .collect::<Vec<_>>(),
        vec![
            app::Outcome::Applied,
            app::Outcome::Failed,
            app::Outcome::Pending
        ]
    );
}

#[test]
fn process_identity_detects_pid_reuse() {
    let current = platform::process(std::process::id()).unwrap();
    let mut reused = current.clone();
    reused.start += 1;
    assert!(platform::argv_and_cwd(&reused).is_err());
    assert!(platform::argv_and_cwd(&current).is_ok());
}

#[test]
#[cfg(target_os = "linux")]
fn stat_parser_handles_spaces_and_parentheses_in_process_names() {
    let stat = format!(
        "42 (name ) ( with spaces) S 12 42 {} 9876 0",
        ["0"; 16].join(" ")
    );
    let p = platform::parse_stat(&stat).unwrap();
    assert_eq!((p.pid, p.parent, p.group, p.start), (42, 12, 42, 9876));
}

#[test]
fn config_and_boot_corruption_are_visible() {
    let t = tempfile::tempdir().unwrap();
    std::fs::write(t.path().join("config.toml"), "auto_restore = 'secret'").unwrap();
    let error = store::load_config(t.path()).unwrap_err();
    assert!(!format!("{error:#}").contains("secret"));
    let store = store::Store::new(t.path(), saved().session).unwrap();
    std::fs::write(store.root.join("pending.json"), "{").unwrap();
    assert!(app::require_clear(&store).is_err());
}

#[test]
fn read_protocol_deadline_covers_dribbling_and_partial_replies() {
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};
    for response in [b"{partial".as_slice(), b"".as_slice()] {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let response = response.to_vec();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            stream.write_all(&response).unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        let start = Instant::now();
        assert!(transport::direct_request(&path, b"{}\n", Duration::from_millis(30)).is_err());
        assert!(start.elapsed() < Duration::from_millis(90));
        server.join().unwrap();
    }
}

#[test]
fn configurable_program_matching_and_safe_agent_options() {
    let mut cfg = Config {
        allowed_programs: vec!["node".into()],
        match_program_basename: true,
        ..Config::default()
    };
    assert!(cfg.allows("/usr/bin/NODE"));
    assert!(!cfg.allows("/usr/bin/python"));
    cfg.allowed_programs = vec!["*".into()];
    assert!(cfg.allows("/usr/bin/python"));
    for arg in [
        "-c",
        "-r01234567-89ab-cdef-0123-456789abcdef",
        "--continue",
        "--session-id",
        "--fork-session",
        "--resume=latest",
        "prompt",
    ] {
        cfg.agent_extra_args
            .insert("claude".into(), vec![arg.into()]);
        assert!(cfg.validate().is_err(), "{arg}");
    }
    cfg.agent_extra_args
        .insert("claude".into(), vec!["--model".into(), "fixture".into()]);
    cfg.validate().unwrap();
    let command = CommandSpec::Agent {
        executable: "claude".into(),
        agent: AgentKind::Claude,
        session_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
    };
    assert_eq!(
        &command.allowed_argv(&cfg).unwrap()[3..],
        &["--model", "fixture"]
    );
    cfg.resume_agents = false;
    assert!(command.allowed_argv(&cfg).is_err());
}

#[test]
fn rename_preserves_snapshot_and_boot_identity() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().canonicalize().unwrap().join("host.sock");
    let (_, session) = store::session_identity(&socket).unwrap();
    assert_eq!(
        session,
        store::digest(format!("cantona.herde-revive\0session-v1\0{}", socket.display()).as_bytes())
    );
    let store = store::Store::new(&dir.path().join("state"), session.clone()).unwrap();
    let mut snapshot = saved();
    snapshot.session = session.clone();
    snapshot.tool = "herde-revive".into();
    store.save(&snapshot, None, 10).unwrap();
    assert_eq!(
        store
            .read_snapshot(&store.snapshot_path(None).unwrap())
            .unwrap(),
        snapshot
    );
    let generation = "b".repeat(64);
    let mut journal = app::Journal::new(&session, &generation);
    journal.tool = "herde-revive".into();
    journal.state = app::BootState::Done;
    app::finish(&store, &journal).unwrap();
    assert!(app::boot(&store, &generation).unwrap().is_some());
    store::atomic_json(&store.root.join("pending.json"), &journal).unwrap();
    assert!(app::require_clear(&store).is_err());
    snapshot.tool = "herdr-resurrect".into();
    assert!(snapshot.validate().is_err());
}

#[test]
fn custom_agent_launcher_is_selected_by_environment_and_current_policy() {
    let mut cfg = config();
    let launcher = AgentLauncher {
        agent: AgentKind::Claude,
        executable: "claude-local".into(),
        match_env: std::collections::BTreeMap::from([(
            "CLAUDE_CONFIG_DIR".into(),
            "/fixture/local".into(),
        )]),
    };
    cfg.agent_launchers.push(launcher.clone());
    cfg.allowed_programs.push("claude-local".into());
    cfg.validate().unwrap();
    let rules = [&launcher];
    assert!(
        capture::matching_launcher(&rules, b"CLAUDE_CONFIG_DIR=/fixture/ordinary\0")
            .unwrap()
            .is_none()
    );
    let selected = capture::matching_launcher(
        &rules,
        b"SECRET=never-save\0CLAUDE_CONFIG_DIR=/fixture/local\0",
    )
    .unwrap()
    .unwrap();
    assert_eq!(selected.executable, "claude-local");
    assert!(
        capture::matching_launcher(
            &[&launcher, &launcher],
            b"CLAUDE_CONFIG_DIR=/fixture/local\0"
        )
        .is_err()
    );
    let cmd = CommandSpec::Agent {
        executable: selected.executable.clone(),
        agent: AgentKind::Claude,
        session_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
    };
    assert_eq!(cmd.allowed_argv(&cfg).unwrap()[0], "claude-local");
    let plain = CommandSpec::Program {
        argv: vec!["claude-local".into()],
    };
    assert!(plain.allowed_argv(&cfg).is_err());
    let alternative = CommandSpec::Program {
        argv: vec!["/fixture/bin/claude-local".into(), "--continue".into()],
    };
    cfg.match_program_basename = true;
    assert!(alternative.allowed_argv(&cfg).is_err());
    assert!(
        capture::matching_launcher(
            &rules,
            b"CLAUDE_CONFIG_DIR=/fixture/ordinary\0CLAUDE_CONFIG_DIR=/fixture/local\0"
        )
        .is_err()
    );
    cfg.agent_launchers.clear();
    assert!(cmd.allowed_argv(&cfg).is_err());
}
