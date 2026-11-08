use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn hello() -> Value {
    json!({
        "generation": 1, "version": "0.1.18", "core": {"id": "c0ffee", "name": "fixture"},
        "framed": false, "methods": ["hello", "terminal.list", "terminal.get", "terminal.send"],
        "features": []
    })
}

pub fn record(address: &str, space: &str, tab: &str, pid: u32, cwd: &str) -> Value {
    json!({
        "address": address, "state": "live", "pid": pid, "cwd": cwd, "keep": true,
        "labels": {"herdr.space": space, "herdr.tab": tab, "herdr.seen": "0"},
        "argv": ["bash"], "status": null, "foreground": null
    })
}

/// A scripted reply: the lines to send for one request (notifications carry no
/// id and are sent as given); `None` keeps the connection open and silent.
pub type Reply = Option<Vec<Value>>;

pub struct FakeCore {
    pub socket: PathBuf,
    pub requests: Arc<Mutex<Vec<Value>>>,
    pub sent: Arc<Mutex<Vec<Value>>>,
    /// When set to `n`, every `terminal.get` after the first `n` gets no
    /// reply, as from a core that stopped answering.
    pub answered_gets: Arc<Mutex<Option<usize>>>,
    records: Arc<Mutex<Vec<Value>>>,
    _dir: tempfile::TempDir,
}

impl FakeCore {
    /// Answers every method from `records`. Connections are served one at a
    /// time, sequentially: each is read to EOF before the next is accepted.
    pub fn start(records: Vec<Value>) -> Self {
        Self::launch(records, None)
    }

    /// Answers one connection with exactly `replies`, one per request, in order.
    pub fn scripted(replies: Vec<Reply>) -> Self {
        Self::launch(Vec::new(), Some(replies))
    }

    pub fn records(&self) -> Arc<Mutex<Vec<Value>>> {
        self.records.clone()
    }

    fn launch(records: Vec<Value>, script: Option<Vec<Reply>>) -> Self {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("core.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let records = Arc::new(Mutex::new(records));
        let answered_gets = Arc::new(Mutex::new(None));
        let (seen, typed, table) = (requests.clone(), sent.clone(), records.clone());
        let limit = answered_gets.clone();
        std::thread::spawn(move || {
            let mut script = script;
            let mut gets = 0;
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    line.clear();
                    seen.lock().unwrap().push(request.clone());
                    if script.is_none() && request["method"] == "terminal.get" {
                        gets += 1;
                        if limit.lock().unwrap().is_some_and(|n| gets > n) {
                            std::thread::sleep(Duration::from_secs(3));
                            break;
                        }
                    }
                    let lines = match script.as_mut() {
                        Some(replies) if !replies.is_empty() => match replies.remove(0) {
                            None => {
                                std::thread::sleep(Duration::from_secs(3));
                                break;
                            }
                            Some(lines) => lines,
                        },
                        Some(_) => break,
                        None => vec![answer(&request, &table, &typed)],
                    };
                    for mut reply in lines {
                        if reply.get("result").is_some() || reply.get("error").is_some() {
                            if reply.get("id").is_none() {
                                reply["id"] = request["id"].clone();
                            }
                            reply["jsonrpc"] = json!("2.0");
                        }
                        if writeln!(writer, "{reply}").is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            socket,
            requests,
            sent,
            answered_gets,
            records,
            _dir: dir,
        }
    }
}

fn answer(request: &Value, table: &Arc<Mutex<Vec<Value>>>, sent: &Arc<Mutex<Vec<Value>>>) -> Value {
    let error = |kind: &str| json!({"error": {"code": -32000, "message": "SECRET", "data": {"kind": kind, "hint": "SECRET"}}});
    let records = table.lock().unwrap();
    match request["method"].as_str().unwrap_or("") {
        "hello" => json!({"result": hello()}),
        "terminal.list" => {
            json!({"result": {"terminals": *records, "hidden_finished": 0, "machines": []}})
        }
        "terminal.get" => {
            let wanted = request["params"]["terminal"].as_str().unwrap_or("");
            match records.iter().find(|r| r["address"] == wanted) {
                Some(record) => json!({"result": record}),
                None => error("not_found"),
            }
        }
        "terminal.send" => {
            sent.lock().unwrap().push(request["params"].clone());
            json!({"result": {"turn": 1, "status": null, "started": null, "wait": null}})
        }
        _ => error("unknown_method"),
    }
}
