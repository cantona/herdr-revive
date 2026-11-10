mod common;

use common::fake_core::{FakeCore, hello};
use herdr_revive::core::rpc::{Client, RpcError};
use herdr_revive::model::MAX_BYTES;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Serves one connection with `serve`, for replies the scripted fake cannot
/// produce (raw bytes, endless streams).
fn raw_core(
    serve: impl FnOnce(BufReader<UnixStream>, UnixStream) + Send + 'static,
) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let socket = dir.path().join("core.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        serve(BufReader::new(stream.try_clone().unwrap()), stream);
    });
    (dir, socket)
}

fn read_request(reader: &mut BufReader<UnixStream>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn answer_hello(reader: &mut BufReader<UnixStream>, writer: &mut UnixStream) {
    let request = read_request(reader);
    let reply = json!({"jsonrpc": "2.0", "id": request["id"], "result": hello()});
    writeln!(writer, "{reply}").unwrap();
}

fn error_text(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[test]
fn calls_round_trip_after_one_hello() {
    let core = FakeCore::start(vec![]);
    let mut client = Client::connect(&core.socket, Duration::from_millis(1000)).unwrap();
    assert_eq!(client.version, "0.1.18");
    let result = client.call("terminal.list", json!({})).unwrap();
    assert_eq!(result["terminals"], json!([]));
    assert_eq!(client.requests, 2);
    let seen = core.requests.lock().unwrap();
    assert_eq!(seen[0]["method"], "hello");
    assert_eq!(seen[0]["jsonrpc"], "2.0");
    assert_eq!(seen[1]["method"], "terminal.list");
    assert_ne!(seen[0]["id"], seen[1]["id"]);
}

#[test]
fn errors_keep_the_kind_and_drop_the_message() {
    let core = FakeCore::start(vec![]);
    let mut client = Client::connect(&core.socket, Duration::from_millis(1000)).unwrap();
    let error = client
        .call("terminal.get", json!({"terminal": "t9"}))
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("not_found"), "{text}");
    assert!(!text.contains("SECRET"), "{text}");
    assert_eq!(error.downcast_ref::<RpcError>().unwrap().kind, "not_found");
    let result = client.call("terminal.list", json!({})).unwrap();
    assert_eq!(result["terminals"], json!([]));
}

#[test]
fn a_null_id_error_reply_keeps_the_kind() {
    let core = FakeCore::scripted(vec![
        Some(vec![json!({"result": hello()})]),
        Some(vec![json!({
            "jsonrpc": "2.0", "id": null,
            "error": {"code": -32700, "message": "SECRET", "data": {"kind": "parse_error", "hint": "SECRET"}}
        })]),
    ]);
    let mut client = Client::connect(&core.socket, Duration::from_millis(1000)).unwrap();
    let error = client.call("terminal.list", json!({})).unwrap_err();
    assert_eq!(
        error.downcast_ref::<RpcError>().unwrap().kind,
        "parse_error"
    );
    let text = error_text(error);
    assert!(!text.contains("SECRET"), "{text}");
}

#[test]
fn a_silent_server_times_out_within_the_budget() {
    let core = FakeCore::scripted(vec![Some(vec![json!({"result": hello()})]), None]);
    let mut client = Client::connect(&core.socket, Duration::from_millis(200)).unwrap();
    let started = Instant::now();
    let text = error_text(client.call("terminal.list", json!({})).unwrap_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        text.contains("terminal.list timed out after 200 ms"),
        "{text}"
    );
    let text = error_text(client.call("terminal.list", json!({})).unwrap_err());
    assert!(text.contains("unusable after an earlier error"), "{text}");
}

#[test]
fn endless_notifications_hit_the_call_deadline() {
    let (_dir, socket) = raw_core(|mut reader, mut writer| {
        answer_hello(&mut reader, &mut writer);
        read_request(&mut reader);
        let notification = json!({"jsonrpc": "2.0", "method": "event", "params": {"seq": 1}});
        while writeln!(writer, "{notification}").is_ok() {
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let mut client = Client::connect(&socket, Duration::from_millis(300)).unwrap();
    let started = Instant::now();
    let text = error_text(client.call("terminal.list", json!({})).unwrap_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        text.contains("terminal.list timed out after 300 ms"),
        "{text}"
    );
}

#[test]
fn a_trickled_reply_hits_the_call_deadline() {
    let (_dir, socket) = raw_core(|mut reader, mut writer| {
        answer_hello(&mut reader, &mut writer);
        read_request(&mut reader);
        while writer.write_all(b" ").is_ok() {
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let mut client = Client::connect(&socket, Duration::from_millis(300)).unwrap();
    let started = Instant::now();
    let text = error_text(client.call("terminal.list", json!({})).unwrap_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        text.contains("terminal.list timed out after 300 ms"),
        "{text}"
    );
}

#[test]
fn a_silent_hello_times_out_within_the_budget() {
    let (_dir, socket) = raw_core(|mut reader, _writer| {
        read_request(&mut reader);
        std::thread::sleep(Duration::from_secs(2));
    });
    let started = Instant::now();
    let text = error_text(
        Client::connect(&socket, Duration::from_millis(300))
            .err()
            .unwrap(),
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(text.contains("hello timed out after 300 ms"), "{text}");
}

#[test]
fn a_connection_closed_mid_call_is_reported() {
    let core = FakeCore::scripted(vec![Some(vec![json!({"result": hello()})])]);
    let mut client = Client::connect(&core.socket, Duration::from_millis(1000)).unwrap();
    let text = error_text(client.call("terminal.list", json!({})).unwrap_err());
    assert!(
        text.contains("closed the connection during terminal.list"),
        "{text}"
    );
}

#[test]
fn a_malformed_json_line_is_refused() {
    let (_dir, socket) = raw_core(|mut reader, mut writer| {
        answer_hello(&mut reader, &mut writer);
        read_request(&mut reader);
        writer.write_all(b"{not json\n").unwrap();
        std::thread::sleep(Duration::from_secs(1));
    });
    let mut client = Client::connect(&socket, Duration::from_millis(1000)).unwrap();
    let text = error_text(client.call("terminal.list", json!({})).unwrap_err());
    assert!(
        text.contains("invalid herdr-core JSON reply to terminal.list"),
        "{text}"
    );
}

#[test]
fn an_oversized_line_is_refused() {
    let (_dir, socket) = raw_core(|mut reader, mut writer| {
        answer_hello(&mut reader, &mut writer);
        read_request(&mut reader);
        let mut line = vec![b'a'; MAX_BYTES + 1];
        line.push(b'\n');
        let _ = writer.write_all(&line);
    });
    let mut client = Client::connect(&socket, Duration::from_millis(5000)).unwrap();
    let text = error_text(client.call("terminal.list", json!({})).unwrap_err());
    assert!(
        text.contains("terminal.list reply exceeds size limit"),
        "{text}"
    );
}

#[test]
fn framed_mode_and_bad_replies_are_refused() {
    let mut framed = hello();
    framed["framed"] = json!(true);
    let core = FakeCore::scripted(vec![Some(vec![json!({"result": framed})])]);
    let text = error_text(
        Client::connect(&core.socket, Duration::from_millis(1000))
            .err()
            .unwrap(),
    );
    assert!(text.contains("framed"), "{text}");

    let core = FakeCore::scripted(vec![
        Some(vec![json!({"result": hello()})]),
        Some(vec![json!({"jsonrpc": "2.0", "id": 999, "result": {}})]),
    ]);
    let mut client = Client::connect(&core.socket, Duration::from_millis(1000)).unwrap();
    let error = client.call("terminal.list", json!({})).unwrap_err();
    assert!(format!("{error:#}").contains("ID mismatch"));
}

#[test]
fn notifications_between_replies_are_skipped() {
    let core = FakeCore::scripted(vec![
        Some(vec![json!({"result": hello()})]),
        Some(vec![
            json!({"jsonrpc": "2.0", "method": "event", "params": {"seq": 1, "type": "status"}}),
            json!({"result": {"terminals": []}}),
        ]),
    ]);
    let mut client = Client::connect(&core.socket, Duration::from_millis(1000)).unwrap();
    assert_eq!(
        client.call("terminal.list", json!({})).unwrap()["terminals"],
        json!([])
    );
}

#[test]
fn a_missing_socket_fails_fast() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let started = Instant::now();
    let text = error_text(
        Client::connect(&dir.path().join("none.sock"), Duration::from_millis(500))
            .err()
            .unwrap(),
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(text.contains("none.sock"), "{text}");
    assert!(text.contains("ENOENT"), "{text}");
}

/// Binds a listener whose backlog holds one pending connection, then fills it.
#[cfg(target_os = "linux")]
fn full_backlog() -> (tempfile::TempDir, PathBuf, UnixListener, UnixStream) {
    use nix::sys::socket::{
        AddressFamily, Backlog, SockFlag, SockType, UnixAddr, bind, listen, socket,
    };
    use std::os::fd::AsRawFd;
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let path = dir.path().join("core.sock");
    let fd = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .unwrap();
    bind(fd.as_raw_fd(), &UnixAddr::new(&path).unwrap()).unwrap();
    listen(&fd, Backlog::new(0).unwrap()).unwrap();
    let filler = UnixStream::connect(&path).unwrap();
    (dir, path, UnixListener::from(fd), filler)
}

#[cfg(target_os = "linux")]
#[test]
fn a_full_backlog_waits_for_room_until_the_deadline() {
    let (_dir, socket, listener, _filler) = full_backlog();
    let started = Instant::now();
    let text = error_text(
        Client::connect(&socket, Duration::from_millis(300))
            .err()
            .unwrap(),
    );
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(text.contains("connect timed out after 300 ms"), "{text}");

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        let (queued, _) = listener.accept().unwrap();
        drop(queued);
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream.try_clone().unwrap();
        answer_hello(&mut BufReader::new(stream), &mut writer);
        std::thread::sleep(Duration::from_secs(1));
    });
    let client = Client::connect(&socket, Duration::from_millis(1000)).unwrap();
    assert_eq!(client.version, "0.1.18");
}

use common::fake_core::record;
use common::shell::Shell;
use herdr_revive::core::host::{self, CoreHost, RunMode};
use herdr_revive::model::{Config, SCHEMA, SavedPane, Scope, Snapshot, TOOL};
use herdr_revive::planner;
use herdr_revive::transport::Host;

#[test]
fn records_become_tui_shaped_panes() {
    let mut rec = record("t7", "s1", "tb1", 4242, "/work");
    rec["status"] = json!({"state": "idle", "app": "claude-code"});
    let pane = host::live_pane(&rec).unwrap().unwrap();
    assert_eq!(pane.workspace_id, "s1");
    assert_eq!(pane.tab_id, "s1:ttb1");
    assert_eq!(pane.pane_id, "s1:pt7");
    assert_eq!(pane.terminal_id, "t7");
    assert_eq!(pane.cwd.as_deref(), Some("/work"));
    assert_eq!(pane.agent.as_deref(), Some("claude-code"));
    assert!(pane.agent_session.is_none());
    assert_eq!(host::address_of(&pane.pane_id).unwrap(), "t7");

    // A shell that has moved is seen through its foreground process.
    let root = tempfile::tempdir().unwrap();
    let sub = root.path().join("sub dir");
    std::fs::create_dir(&sub).unwrap();
    let sub = sub.to_str().unwrap();
    let moved_to = |cwd: &str| {
        let mut moved = rec.clone();
        moved["foreground"] = json!({"argv": ["bash", "-l"], "cwd": cwd, "pid": 4243});
        host::live_pane(&moved).unwrap().unwrap().cwd
    };
    assert_eq!(moved_to(sub).as_deref(), Some(sub));
    // Linux marks a removed folder; one made again at that path is there.
    assert_eq!(moved_to(&format!("{sub} (deleted)")).as_deref(), Some(sub));
    // A folder that is gone cannot be entered: the terminal's own cwd stands.
    let gone = root.path().join("gone");
    let gone = gone.to_str().unwrap();
    assert_eq!(
        moved_to(&format!("{gone} (deleted)")).as_deref(),
        Some("/work")
    );
    assert_eq!(moved_to(gone).as_deref(), Some("/work"));
}

#[test]
fn finished_parked_unlabeled_and_odd_labeled_terminals_are_not_panes() {
    let mut finished = record("t1", "s1", "tb1", 1, "/");
    finished["state"] = json!("finished");
    assert!(host::live_pane(&finished).unwrap().is_none());
    let mut parked = record("t2", "", "", 1, "/");
    parked["labels"] = json!({"herdr.space": "", "herdr.tab": ""});
    assert!(host::live_pane(&parked).unwrap().is_none());
    let mut bare = record("t3", "s1", "tb1", 1, "/");
    bare["labels"] = json!({});
    assert!(host::live_pane(&bare).unwrap().is_none());
    let mut odd = record("t4", "s-1", "tb1", 1, "/");
    odd["labels"]["herdr.space"] = json!("s-1");
    assert!(host::live_pane(&odd).unwrap().is_none());
    let remote = record("vps/t7", "s1", "tb1", 1, "/");
    assert!(host::live_pane(&remote).unwrap().is_none());
    let colon = record("a:b", "s1", "tb1", 1, "/");
    assert!(host::live_pane(&colon).unwrap().is_none());
    for bad in ["foo-bar", "a:b"] {
        let mut space = record("t5", "s1", "tb1", 1, "/");
        space["labels"]["herdr.space"] = json!(bad);
        assert!(host::live_pane(&space).unwrap().is_none(), "space {bad:?}");
        let mut tab = record("t6", "s1", "tb1", 1, "/");
        tab["labels"]["herdr.tab"] = json!(bad);
        assert!(host::live_pane(&tab).unwrap().is_none(), "tab {bad:?}");
    }
}

#[test]
fn a_terminal_with_an_odd_label_does_not_hide_the_others() {
    let records = vec![
        record("t1", "my-space", "tb1", 1, "/a"),
        record("t2", "s1", "tb1", 2, "/b"),
    ];
    let live = host::live_snapshot(&records).unwrap();
    let panes: Vec<_> = live.panes.iter().map(|p| p.pane_id.as_str()).collect();
    assert_eq!(panes, ["s1:pt2"]);
    assert_eq!(live.workspaces.len(), 1);
    planner::index_live(&live).unwrap();
    let listed = host::listed_pane_ids(&records).unwrap();
    assert_eq!(listed.into_iter().collect::<Vec<_>>(), ["s1:pt2"]);
}

#[test]
fn snapshots_group_panes_by_space_and_tab_and_validate() {
    let records = vec![
        record("t1", "s1", "tb1", 1, "/a"),
        record("t2", "s1", "tb1", 2, "/b"),
        record("t3", "s2", "tb2", 3, "/c"),
        {
            let mut r = record("t4", "s3", "tb3", 4, "/d");
            r["state"] = json!("finished");
            r
        },
    ];
    let live = host::live_snapshot(&records).unwrap();
    assert_eq!(live.workspaces.len(), 2);
    assert_eq!(live.tabs.len(), 2);
    assert_eq!(live.panes.len(), 3);
    assert!(live.focused_pane_id.is_none());
    planner::index_live(&live).unwrap();
    let saved = Snapshot {
        tool: TOOL.into(),
        schema: SCHEMA,
        session: "a".repeat(64),
        created_ms: 1,
        scope: Scope::Session,
        layout: vec![],
        focused_workspace_id: None,
        panes: live
            .panes
            .iter()
            .map(|p| SavedPane {
                workspace_id: p.workspace_id.clone(),
                tab_id: p.tab_id.clone(),
                pane_id: p.pane_id.clone(),
                cwd: p.cwd.clone().unwrap(),
                command: None,
            })
            .collect(),
    };
    saved.validate().unwrap();
}

#[test]
fn pane_ids_must_name_a_terminal() {
    assert_eq!(host::address_of("s1:pt7").unwrap(), "t7");
    assert_eq!(host::pane_id("s1", "t7"), "s1:pt7");
    assert_eq!(host::tab_id("s1", "tb1"), "s1:ttb1");
    for bad in ["s1:t7", "s1:p", "s1:pt7:p", "a-b:pt7", ""] {
        assert!(host::address_of(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn core_host_reads_lists_sends_or_emits_and_refuses_layout_apis() {
    let core = FakeCore::start(vec![record("t7", "s1", "tb1", 4242, "/work")]);
    let config = Config::default();
    let mut sending = CoreHost::connect(&core.socket, &config, RunMode::Send).unwrap();
    assert!(!sending.supports_layout());
    assert_eq!(sending.snapshot().unwrap().panes[0].pane_id, "s1:pt7");
    assert_eq!(sending.pane("s1:pt7").unwrap().terminal_id, "t7");
    assert!(format!("{:#}", sending.pane("s1:pt9").unwrap_err()).contains("not_found"));
    sending
        .run("s1:pt7", " cd -- '/work' && command 'sleep' '600'")
        .unwrap();
    assert_eq!(
        core.sent.lock().unwrap()[0],
        json!({"terminal": "t7", "text": " cd -- '/work' && command 'sleep' '600'"})
    );
    assert!(
        format!(
            "{:#}",
            sending.api("layout.export", json!({}), "x").unwrap_err()
        )
        .contains("not supported by the herdr-core host")
    );
    assert_eq!(sending.requests(), 5);
    drop(sending);

    let mut emitting = CoreHost::connect(&core.socket, &config, RunMode::Emit).unwrap();
    emitting.run("s1:pt7", "text one").unwrap();
    emitting.run("s1:pt7", "text two").unwrap();
    assert_eq!(emitting.emitted, ["text one", "text two"]);
    assert_eq!(core.sent.lock().unwrap().len(), 1);
    assert_eq!(emitting.terminal("t7").unwrap()["pid"], 4242);
}

#[test]
fn process_info_refuses_missing_pid_and_non_live_records() {
    let mut no_pid = record("t1", "s1", "tb1", 1, "/");
    no_pid["pid"] = json!(null);
    let mut finished = record("t2", "s1", "tb1", 4242, "/");
    finished["state"] = json!("finished");
    let core = FakeCore::start(vec![no_pid, finished]);
    let mut host = CoreHost::connect(&core.socket, &Config::default(), RunMode::Send).unwrap();
    let err = format!("{:#}", host.process_info("s1:pt1").unwrap_err());
    assert!(err.contains("terminal has no shell process"), "{err}");
    let err = format!("{:#}", host.process_info("s1:pt2").unwrap_err());
    assert!(err.contains("terminal t2 is not live"), "{err}");
}

#[test]
fn emit_mode_rejects_malformed_pane_ids_without_a_socket_call() {
    let core = FakeCore::start(vec![]);
    let mut host = CoreHost::connect(&core.socket, &Config::default(), RunMode::Emit).unwrap();
    let before = host.requests();
    assert!(host.run("s1:t7", "text").is_err());
    assert!(host.emitted.is_empty());
    assert_eq!(host.requests(), before);
}

#[test]
fn one_process_table_read_serves_every_pane_of_a_snapshot() {
    let shell = Shell::start();
    let records: Vec<Value> = (0..20)
        .map(|n| record(&format!("t{n}"), "s1", "tb1", shell.pid, "/"))
        .collect();
    let core = FakeCore::start(records);
    let mut host = CoreHost::connect(&core.socket, &Config::default(), RunMode::Send).unwrap();
    assert_eq!(host.table_reads(), 0);
    for round in 1..=2 {
        let live = host.snapshot().unwrap();
        assert_eq!(live.panes.len(), 20);
        for pane in &live.panes {
            let info = host.process_info(&pane.pane_id).unwrap();
            assert_eq!(info.foreground_process_group_id, Some(shell.pid));
            assert_eq!(info.foreground_processes[0].pid, shell.pid);
        }
        assert_eq!(host.table_reads(), round);
    }

    let late = Shell::start();
    core.records()
        .lock()
        .unwrap()
        .push(record("t99", "s1", "tb1", late.pid, "/"));
    let info = host.process_info("s1:pt99").unwrap();
    assert_eq!(host.table_reads(), 3);
    assert_eq!(info.foreground_process_group_id, Some(late.pid));
    assert_eq!(info.foreground_processes.len(), 1);
    assert_eq!(info.foreground_processes[0].pid, late.pid);

    host.pane("s1:pt0").unwrap();
    host.process_info("s1:pt0").unwrap();
    assert_eq!(host.table_reads(), 4);
}
