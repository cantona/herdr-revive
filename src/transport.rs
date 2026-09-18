use crate::model::*;
use anyhow::{Context, Result, bail, ensure};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub trait Host {
    fn snapshot(&mut self) -> Result<LiveSnapshot>;
    fn process_info(&mut self, pane: &str) -> Result<ProcessInfo>;
    fn pane(&mut self, pane: &str) -> Result<LivePane>;
    fn run(&mut self, pane: &str, text: &str) -> Result<()>;
    fn api(&mut self, _method: &str, _params: Value, _result_type: &str) -> Result<Value> {
        bail!("host does not implement layout APIs")
    }
}

pub struct Transport {
    pub socket: PathBuf,
    pub binary: Option<PathBuf>,
    pub kind: TransportKind,
    pub timeout: Duration,
    pub requests: usize,
    pub children: usize,
    checked: bool,
}

impl Transport {
    pub fn new(socket: PathBuf, binary: Option<PathBuf>, config: &Config) -> Self {
        Self {
            socket,
            binary,
            kind: config.transport,
            timeout: Duration::from_millis(config.timeout_ms),
            requests: 0,
            children: 0,
            checked: false,
        }
    }
    fn request(
        &mut self,
        method: &str,
        params: Value,
        args: &[&str],
        cli_id: &str,
        result_type: &str,
    ) -> Result<Value> {
        self.requests += 1;
        let id = format!("herdr-revive:{}:{}", std::process::id(), self.requests);
        let kind = if args.is_empty() {
            TransportKind::Direct
        } else {
            self.kind
        };
        let result = match kind {
            TransportKind::Direct => {
                let request = json!({"id": id, "method": method, "params": params});
                let mut bytes = serde_json::to_vec(&request)?;
                bytes.push(b'\n');
                let response =
                    direct_request(&self.socket, &bytes, self.timeout).with_context(|| {
                        format!("Herdr {method} transport failed; mutations are never retried")
                    })?;
                decode_response(&response, &id, result_type)?
            }
            TransportKind::Cli => {
                self.children += 1;
                let bin = self
                    .binary
                    .as_ref()
                    .context("HERDR_BIN_PATH is required for CLI transport")?;
                let response =
                    cli_request(bin, &self.socket, args, self.timeout).with_context(|| {
                        format!("Herdr {method} CLI failed; mutations are never retried")
                    })?;
                // Herdr's pane run CLI reports success by exit status, without JSON.
                if method == "pane.send_input" {
                    ensure!(response.is_empty(), "unexpected output from Herdr pane run");
                    json!({"type": "ok"})
                } else {
                    decode_response(&response, cli_id, result_type)?
                }
            }
        };
        Ok(result)
    }
    fn check(&mut self) -> Result<()> {
        if self.checked {
            return Ok(());
        }
        if self.kind == TransportKind::Direct {
            let pong = self.request("ping", json!({}), &[], "", "pong")?;
            let version = pong["version"].as_str().context("missing host version")?;
            let protocol: u32 = pong["protocol"]
                .as_u64()
                .context("missing host protocol")?
                .try_into()?;
            validate_protocol(version, protocol)?;
            self.checked = true;
        } else {
            // The CLI has no raw ping command; snapshot is its versioned handshake.
            self.snapshot()?;
        }
        Ok(())
    }
}

fn field<T: DeserializeOwned>(value: Value, key: &str) -> Result<T> {
    serde_json::from_value(value.get(key).context("missing response data")?.clone())
        .map_err(|_| anyhow::anyhow!("invalid Herdr response data"))
}

impl Host for Transport {
    fn api(&mut self, method: &str, params: Value, result_type: &str) -> Result<Value> {
        self.check()?;
        self.request(method, params, &[], "", result_type)
    }
    fn snapshot(&mut self) -> Result<LiveSnapshot> {
        if self.kind == TransportKind::Direct {
            self.check()?;
        }
        let value = self.request(
            "session.snapshot",
            json!({}),
            &["api", "snapshot"],
            "cli:api:snapshot",
            "session_snapshot",
        )?;
        let snapshot: LiveSnapshot = field(value, "snapshot")?;
        validate_protocol(&snapshot.version, snapshot.protocol)?;
        self.checked = true;
        Ok(snapshot)
    }
    fn process_info(&mut self, pane: &str) -> Result<ProcessInfo> {
        self.check()?;
        let value = self.request(
            "pane.process_info",
            json!({"pane_id": pane}),
            &["pane", "process-info", "--pane", pane],
            "cli:pane:process_info",
            "pane_process_info",
        )?;
        let info: ProcessInfo = field(value, "process_info")?;
        ensure!(info.pane_id == pane, "process response pane ID mismatch");
        Ok(info)
    }
    fn pane(&mut self, pane: &str) -> Result<LivePane> {
        self.check()?;
        let value = self.request(
            "pane.get",
            json!({"pane_id": pane}),
            &["pane", "get", pane],
            "cli:pane:get",
            "pane_info",
        )?;
        let info: LivePane = field(value, "pane")?;
        ensure!(info.pane_id == pane, "response pane ID mismatch");
        Ok(info)
    }
    fn run(&mut self, pane: &str, text: &str) -> Result<()> {
        self.check()?;
        self.request(
            "pane.send_input",
            json!({"pane_id": pane, "text": text, "keys": ["Enter"]}),
            &["pane", "run", pane, text],
            "cli:request",
            "ok",
        )?;
        Ok(())
    }
}

pub fn decode_response(bytes: &[u8], id: &str, result_type: &str) -> Result<Value> {
    ensure!(
        bytes.len() <= MAX_BYTES,
        "Herdr response exceeds size limit"
    );
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("invalid Herdr JSON response"))?;
    ensure!(
        value["id"].as_str() == Some(id),
        "Herdr response ID mismatch"
    );
    if value.get("error").is_some() {
        bail!("Herdr rejected request (server details redacted)");
    }
    let result = value
        .get("result")
        .context("missing Herdr result envelope")?;
    ensure!(
        result["type"].as_str() == Some(result_type),
        "unexpected Herdr result type"
    );
    Ok(result.clone())
}

#[cfg(unix)]
pub fn direct_request(
    socket: &std::path::Path,
    bytes: &[u8],
    timeout: Duration,
) -> Result<Vec<u8>> {
    use nix::sys::socket::{
        AddressFamily, SockFlag, SockType, UnixAddr, connect, socket as new_socket,
    };
    use std::os::fd::AsRawFd;
    let deadline = Instant::now() + timeout;
    let fd = new_socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    match connect(fd.as_raw_fd(), &UnixAddr::new(socket)?) {
        Ok(()) => {}
        Err(nix::errno::Errno::EINPROGRESS) => {}
        Err(_) => bail!("cannot connect to target socket"),
    }
    let mut stream = std::os::unix::net::UnixStream::from(fd);
    let mut remaining = bytes;
    while !remaining.is_empty() {
        ensure!(Instant::now() < deadline, "Herdr write deadline exceeded");
        match stream.write(remaining) {
            Ok(0) => bail!("Herdr connection closed during write"),
            Ok(n) => remaining = &remaining[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                wait_ready(&stream, true, deadline)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => bail!("Herdr socket write failed"),
        }
    }
    let mut response = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        ensure!(
            Instant::now() < deadline,
            "Herdr response deadline exceeded"
        );
        match stream.read(&mut buffer) {
            Ok(0) => bail!("Herdr closed connection before complete response"),
            Ok(n) => {
                response.extend_from_slice(&buffer[..n]);
                ensure!(
                    response.len() <= MAX_BYTES,
                    "Herdr response exceeds size limit"
                );
                if let Some(end) = response.iter().position(|b| *b == b'\n') {
                    ensure!(
                        response[end + 1..].iter().all(u8::is_ascii_whitespace),
                        "multiple Herdr responses refused"
                    );
                    response.truncate(end);
                    return Ok(response);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                wait_ready(&stream, false, deadline)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => bail!("Herdr socket read failed"),
        }
    }
}

#[cfg(unix)]
fn wait_ready(fd: &impl std::os::fd::AsFd, write: bool, deadline: Instant) -> Result<()> {
    use nix::poll::{PollFd, PollFlags, poll};
    let left = deadline
        .checked_duration_since(Instant::now())
        .context("Herdr request deadline exceeded")?;
    let mut fds = [PollFd::new(
        fd.as_fd(),
        if write {
            PollFlags::POLLOUT
        } else {
            PollFlags::POLLIN
        },
    )];
    poll(&mut fds, left.as_millis().clamp(1, u16::MAX as u128) as u16)?;
    Ok(())
}

#[cfg(not(unix))]
pub fn direct_request(_: &std::path::Path, _: &[u8], _: Duration) -> Result<Vec<u8>> {
    bail!("direct transport is not validated for Windows")
}

#[cfg(unix)]
fn cli_request(
    binary: &std::path::Path,
    socket: &std::path::Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Vec<u8>> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use std::os::fd::AsRawFd;
    use std::process::{Command, Stdio};
    let mut child = Command::new(binary)
        .args(args)
        .env("HERDR_SOCKET_PATH", socket)
        .env_remove("HERDR_SESSION")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot start HERDR_BIN_PATH")?;
    let result = (|| {
        let mut stdout = child.stdout.take().context("CLI stdout missing")?;
        fcntl(stdout.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
        let deadline = Instant::now() + timeout;
        let mut response = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            ensure!(Instant::now() < deadline, "Herdr CLI deadline exceeded");
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    if let Some(status) = child.try_wait()? {
                        ensure!(
                            status.success(),
                            "Herdr CLI failed (diagnostic payload redacted)"
                        );
                        return Ok(response);
                    }
                }
                Ok(n) => {
                    response.extend_from_slice(&buffer[..n]);
                    ensure!(
                        response.len() <= MAX_BYTES,
                        "Herdr CLI response exceeds size limit"
                    );
                    continue;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e.into()),
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    child.wait().context("cannot reap Herdr CLI")?;
    result
}

#[cfg(not(unix))]
fn cli_request(
    _: &std::path::Path,
    _: &std::path::Path,
    _: &[&str],
    _: Duration,
) -> Result<Vec<u8>> {
    bail!("bounded CLI transport has not been validated for this platform")
}
