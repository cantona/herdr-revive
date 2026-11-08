use crate::model::MAX_BYTES;
use anyhow::{Context, Result, bail, ensure};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, poll};
use serde_json::{Value, json};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

const CHUNK: usize = 64 * 1024;
const CONNECT_RETRY: Duration = Duration::from_millis(10);

/// A refusal from herdr-core. Only the stable `kind` survives; the message and
/// hint can quote terminal content, so they are never reported.
#[derive(Debug)]
pub struct RpcError {
    pub kind: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "herdr-core refused the request (kind: {}; details redacted)",
            self.kind
        )
    }
}

impl std::error::Error for RpcError {}

/// A blocking JSON-RPC client. Every call, and `connect` including its
/// `hello`, is bounded by one deadline of `timeout`; after any transport or
/// protocol failure the connection may be out of step, so it refuses further
/// calls.
pub struct Client {
    stream: UnixStream,
    pending: Vec<u8>,
    timeout: Duration,
    broken: bool,
    next_id: u64,
    pub requests: usize,
    pub version: String,
}

impl Client {
    pub fn connect(socket: &Path, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        let stream = connect_until(socket, timeout, deadline)?;
        let mut client = Self {
            stream,
            pending: Vec::new(),
            timeout,
            broken: false,
            next_id: 0,
            requests: 0,
            version: String::new(),
        };
        let hello = client.request("hello", json!({}), deadline)?;
        ensure!(
            hello["framed"] == Value::Bool(false),
            "herdr-core switched the connection to framed mode; refused"
        );
        client.version = hello["version"]
            .as_str()
            .context("herdr-core hello reply without version")?
            .to_string();
        Ok(client)
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let deadline = Instant::now() + self.timeout;
        self.request(method, params, deadline)
    }

    fn request(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value> {
        ensure!(
            !self.broken,
            "herdr-core connection unusable after an earlier error; {method} not sent"
        );
        let outcome = self.exchange(method, params, deadline);
        if let Err(error) = &outcome
            && error.downcast_ref::<RpcError>().is_none()
        {
            self.broken = true;
        }
        outcome
    }

    fn exchange(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value> {
        self.next_id += 1;
        self.requests += 1;
        let id = self.next_id;
        let mut bytes = serde_json::to_vec(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )?;
        bytes.push(b'\n');
        self.write_all(method, &bytes, deadline)?;
        loop {
            let line = self.read_line(method, deadline)?;
            let reply: Value = serde_json::from_slice(&line)
                .with_context(|| format!("invalid herdr-core JSON reply to {method}"))?;
            ensure!(
                reply.is_object(),
                "invalid herdr-core JSON reply to {method}: not an object"
            );
            let Some(reply_id) = reply.get("id") else {
                continue;
            };
            if let Some(error) = reply.get("error")
                && (reply_id.is_null() || *reply_id == json!(id))
            {
                let kind = error
                    .pointer("/data/kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                return Err(RpcError { kind }.into());
            }
            ensure!(
                *reply_id == json!(id),
                "herdr-core {method} reply ID mismatch"
            );
            return reply
                .get("result")
                .cloned()
                .with_context(|| format!("herdr-core {method} reply without result"));
        }
    }

    fn write_all(&mut self, method: &str, bytes: &[u8], deadline: Instant) -> Result<()> {
        let mut remaining = bytes;
        while !remaining.is_empty() {
            self.check_deadline(method, deadline)?;
            match self.stream.write(remaining) {
                Ok(0) => bail!("herdr-core closed the connection during {method}"),
                Ok(n) => remaining = &remaining[n..],
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    self.wait(method, PollFlags::POLLOUT, deadline)?
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(
                        anyhow::Error::new(e).context(format!("herdr-core {method} write failed"))
                    );
                }
            }
        }
        Ok(())
    }

    fn read_line(&mut self, method: &str, deadline: Instant) -> Result<Vec<u8>> {
        let mut scanned = 0;
        let mut chunk = vec![0u8; CHUNK];
        loop {
            if let Some(offset) = self.pending[scanned..].iter().position(|b| *b == b'\n') {
                let end = scanned + offset + 1;
                ensure!(
                    end <= MAX_BYTES,
                    "herdr-core {method} reply exceeds size limit"
                );
                return Ok(self.pending.drain(..end).collect());
            }
            scanned = self.pending.len();
            ensure!(
                scanned < MAX_BYTES,
                "herdr-core {method} reply exceeds size limit"
            );
            self.check_deadline(method, deadline)?;
            match self.stream.read(&mut chunk) {
                Ok(0) => bail!("herdr-core closed the connection during {method}"),
                Ok(n) => self.pending.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    self.wait(method, PollFlags::POLLIN, deadline)?
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) if e.kind() == ErrorKind::TimedOut => bail!(self.timed_out(method)),
                Err(e) => {
                    return Err(
                        anyhow::Error::new(e).context(format!("herdr-core {method} read failed"))
                    );
                }
            }
        }
    }

    fn wait(&self, method: &str, flags: PollFlags, deadline: Instant) -> Result<()> {
        let left = self.check_deadline(method, deadline)?;
        let mut fds = [PollFd::new(self.stream.as_fd(), flags)];
        match poll(&mut fds, poll_millis(left)) {
            Ok(_) | Err(Errno::EINTR) => Ok(()),
            Err(e) => {
                Err(anyhow::Error::new(e).context(format!("herdr-core {method} poll failed")))
            }
        }
    }

    fn check_deadline(&self, method: &str, deadline: Instant) -> Result<Duration> {
        match deadline.checked_duration_since(Instant::now()) {
            Some(left) if !left.is_zero() => Ok(left),
            _ => bail!(self.timed_out(method)),
        }
    }

    fn timed_out(&self, method: &str) -> String {
        timeout_message(method, self.timeout)
    }
}

fn timeout_message(method: &str, timeout: Duration) -> String {
    format!(
        "herdr-core {method} timed out after {} ms",
        timeout.as_millis()
    )
}

fn poll_millis(left: Duration) -> u16 {
    left.as_millis().clamp(1, u16::MAX as u128) as u16
}

fn connect_until(socket: &Path, timeout: Duration, deadline: Instant) -> Result<UnixStream> {
    use nix::sys::socket::{UnixAddr, connect, getsockopt, sockopt::SocketError};
    use std::os::fd::AsRawFd;
    let fd = crate::transport::nonblocking_socket()?;
    let address = UnixAddr::new(socket)
        .with_context(|| format!("invalid herdr-core socket path {}", socket.display()))?;
    let refused = |errno: Errno| {
        anyhow::anyhow!(
            "cannot connect to herdr-core socket {}: {errno}",
            socket.display()
        )
    };
    let timed_out = || anyhow::anyhow!(timeout_message("connect", timeout));
    loop {
        match connect(fd.as_raw_fd(), &address) {
            Ok(()) => break,
            Err(Errno::EAGAIN) => {
                let left = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|left| !left.is_zero())
                    .ok_or_else(timed_out)?;
                std::thread::sleep(left.min(CONNECT_RETRY));
            }
            Err(Errno::EINPROGRESS | Errno::EINTR) => {
                loop {
                    let left = deadline
                        .checked_duration_since(Instant::now())
                        .filter(|left| !left.is_zero())
                        .ok_or_else(timed_out)?;
                    let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLOUT)];
                    match poll(&mut fds, poll_millis(left)) {
                        Ok(0) | Err(Errno::EINTR) => {}
                        Ok(_) => break,
                        Err(errno) => return Err(refused(errno)),
                    }
                }
                let status = getsockopt(&fd, SocketError).map_err(refused)?;
                if status != 0 {
                    return Err(refused(Errno::from_raw(status)));
                }
                break;
            }
            Err(errno) => return Err(refused(errno)),
        }
    }
    Ok(UnixStream::from(fd))
}
