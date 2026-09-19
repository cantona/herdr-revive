use super::{IDENTITY_CONNECTIONS, Process, decode_argv};
use anyhow::{Context, Result, bail, ensure};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

fn read_proc_text(path: impl AsRef<Path>) -> std::io::Result<String> {
    let mut text = String::with_capacity(4096);
    std::fs::File::open(path)?
        .take(64 * 1024 + 1)
        .read_to_string(&mut text)?;
    if text.len() > 64 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "proc metadata exceeds size limit",
        ));
    }
    Ok(text)
}

pub fn parse_stat(text: &str) -> Result<Process> {
    let (pid, _) = text.split_once(' ').context("invalid process stat")?;
    let (_, fields) = text.rsplit_once(") ").context("invalid process stat")?;
    let fields: Vec<&str> = fields.split_whitespace().collect();
    ensure!(fields.len() > 19, "truncated process stat");
    Ok(Process {
        pid: pid.parse()?,
        parent: fields[1].parse()?,
        group: fields[2].parse()?,
        start: fields[19].parse()?,
    })
}

pub fn process(pid: u32) -> Result<Process> {
    parse_stat(
        &read_proc_text(format!("/proc/{pid}/stat")).context("process identity unavailable")?,
    )
}

pub fn process_table() -> Result<HashMap<u32, Process>> {
    let mut table = HashMap::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        match read_proc_text(entry.path().join("stat")) {
            Ok(text) => {
                let p = parse_stat(&text)?;
                ensure!(p.pid == pid, "process table identity mismatch");
                table.insert(pid, p);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => bail!("process table is inaccessible"),
        }
    }
    Ok(table)
}

pub fn has_children(shell: &Process) -> Result<bool> {
    ensure!(
        process(shell.pid)? == *shell,
        "shell changed during idle check"
    );
    // Read every thread's child list: a child belongs to the thread that
    // created it, so checking only the main thread can miss background work.
    let children = (|| -> Result<bool> {
        let mut threads = 0;
        for entry in std::fs::read_dir(format!("/proc/{}/task", shell.pid))? {
            let entry = entry?;
            threads += 1;
            ensure!(threads <= 4096, "too many shell threads to inspect");
            if !read_proc_text(entry.path().join("children"))?
                .trim()
                .is_empty()
            {
                return Ok(true);
            }
        }
        ensure!(threads > 0, "shell thread list is unavailable");
        Ok(false)
    })();
    if let Ok(children) = children {
        ensure!(
            process(shell.pid)? == *shell,
            "shell changed during idle check"
        );
        return Ok(children);
    }
    // Older kernels/configurations may not expose thread child lists. Preserve
    // the original complete-table check on unsupported or racing reads.
    let table = process_table()?;
    ensure!(
        table.get(&shell.pid) == Some(shell),
        "shell changed during idle check"
    );
    Ok(table.values().any(|p| p.parent == shell.pid))
}

pub fn terminal_stdio(pid: u32, shell: u32) -> Result<[bool; 3]> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let terminal = std::fs::read_link(format!("/proc/{shell}/fd/0"))?;
    let terminal_meta = std::fs::metadata(format!("/proc/{shell}/fd/0"))?;
    ensure!(
        terminal.starts_with("/dev/pts") || terminal.starts_with("/dev/tty"),
        "shell input is not a terminal"
    );
    ensure!(
        terminal_meta.file_type().is_char_device()
            && terminal_meta.rdev() == controlling_terminal(shell)?
            && terminal_meta.rdev() == controlling_terminal(pid)?,
        "pane streams do not match the controlling terminal"
    );
    let mut null_stdio = [false; 3];
    for (fd, is_null) in null_stdio.iter_mut().enumerate() {
        let target = std::fs::read_link(format!("/proc/{pid}/fd/{fd}"))?;
        let metadata = std::fs::metadata(format!("/proc/{pid}/fd/{fd}"))?;
        *is_null = fd != 0
            && target == Path::new("/dev/null")
            && metadata.file_type().is_char_device()
            && nix::sys::stat::major(metadata.rdev()) == 1
            && nix::sys::stat::minor(metadata.rdev()) == 3;
        ensure!(
            (target == terminal
                && metadata.file_type().is_char_device()
                && (metadata.dev(), metadata.ino(), metadata.rdev())
                    == (
                        terminal_meta.dev(),
                        terminal_meta.ino(),
                        terminal_meta.rdev()
                    ))
                || *is_null,
            "redirected standard streams are not representable as argv"
        );
    }
    Ok(null_stdio)
}

fn controlling_terminal(pid: u32) -> Result<u64> {
    let text = read_proc_text(format!("/proc/{pid}/stat"))?;
    let (_, fields) = text.rsplit_once(") ").context("invalid process stat")?;
    let encoded = fields
        .split_whitespace()
        .nth(4)
        .context("missing controlling terminal")?
        .parse::<i32>()? as u32;
    ensure!(encoded != 0, "process has no controlling terminal");
    let major = u64::from((encoded >> 8) & 0xfff);
    let minor = u64::from((encoded & 0xff) | ((encoded >> 12) & 0xfff00));
    Ok(nix::sys::stat::makedev(major, minor))
}

pub fn ssh_terminal_stdout(pid: u32) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let native = std::fs::metadata("/usr/bin/ssh")?;
    let running = std::fs::metadata(format!("/proc/{pid}/exe"))?;
    ensure!(
        (native.dev(), native.ino()) == (running.dev(), running.ino()),
        "SSH runtime stream recovery requires the system OpenSSH executable"
    );
    let descriptor = |fd: u32| -> Result<(u64, u64, u64, u32)> {
        let meta = std::fs::metadata(format!("/proc/{pid}/fd/{fd}"))?;
        let info = read_proc_text(format!("/proc/{pid}/fdinfo/{fd}"))?;
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .context("missing descriptor flags")?;
        let access = u32::from_str_radix(flags.trim(), 8)? & 3;
        Ok((meta.dev(), meta.ino(), meta.rdev(), access))
    };
    let input = descriptor(0)?;
    let error = descriptor(2)?;
    let terminal = std::fs::metadata(format!("/proc/{pid}/fd/0"))?;
    ensure!(
        terminal.file_type().is_char_device() && terminal.rdev() == controlling_terminal(pid)?,
        "SSH input is not its controlling terminal"
    );
    let mut descriptors = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd"))? {
        let fd: u32 = entry?
            .file_name()
            .to_str()
            .context("invalid descriptor name")?
            .parse()?;
        descriptors.insert(fd);
        ensure!(
            descriptors.len() <= 4096,
            "too many SSH descriptors to inspect"
        );
    }
    let mut outputs = Vec::new();
    // OpenSSH duplicates the session streams before replacing fd 1 with /dev/null.
    // Select by input/error first: a redirected output must not hide ambiguity.
    for fd in descriptors
        .iter()
        .copied()
        .filter(|fd| *fd >= 3 && *fd <= u32::MAX - 2)
    {
        if descriptors.contains(&(fd + 1))
            && descriptors.contains(&(fd + 2))
            && descriptor(fd)? == input
            && descriptor(fd + 2)? == error
        {
            outputs.push(descriptor(fd + 1)?);
        }
    }
    ensure!(
        outputs.len() == 1,
        "SSH session output descriptors are ambiguous; capture refused"
    );
    ensure!(
        outputs[0] == input,
        "SSH session output is redirected; reconnect with terminal output before saving"
    );
    Ok(())
}

pub fn executable(pid: u32) -> Result<std::path::PathBuf> {
    Ok(std::fs::read_link(format!("/proc/{pid}/exe"))?)
}

pub fn git_pager_stdio(table: &HashMap<u32, Process>, git: &Process, shell: u32) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let same_file = |a: &std::fs::Metadata, b: &std::fs::Metadata| {
        (a.dev(), a.ino(), a.rdev()) == (b.dev(), b.ino(), b.rdev())
    };
    let descriptor = |pid, fd| std::fs::metadata(format!("/proc/{pid}/fd/{fd}"));
    let executable = std::fs::metadata(format!("/proc/{}/exe", git.pid))?;
    ensure!(
        same_file(&executable, &std::fs::metadata("/usr/bin/git")?),
        "pager recovery requires system Git"
    );
    let terminal = descriptor(shell, 0)?;
    ensure!(
        terminal.file_type().is_char_device()
            && terminal.rdev() == controlling_terminal(shell)?
            && terminal.rdev() == controlling_terminal(git.pid)?
            && same_file(&descriptor(git.pid, 0)?, &terminal),
        "Git input is not the pane terminal"
    );
    let children: Vec<_> = table
        .values()
        .filter(|child| child.parent == git.pid)
        .collect();
    ensure!(children.len() == 1, "Git pager child is ambiguous");
    let pager = children[0];
    ensure!(
        pager.group == git.group && process(pager.pid)? == *pager,
        "Git pager identity changed"
    );
    let executable = std::fs::metadata(format!("/proc/{}/exe", pager.pid))?;
    let mut supported = false;
    for path in ["/usr/bin/less", "/usr/bin/more"] {
        match std::fs::metadata(path) {
            Ok(native) => supported |= same_file(&executable, &native),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(supported, "Git pager is not system less or more");
    let pipe = descriptor(pager.pid, 0)?;
    ensure!(pipe.file_type().is_fifo(), "Git pager input is not a pipe");
    ensure!(
        terminal.rdev() == controlling_terminal(pager.pid)?
            && same_file(&terminal, &descriptor(pager.pid, 1)?)
            && same_file(&terminal, &descriptor(pager.pid, 2)?),
        "Git pager output is redirected"
    );
    match descriptor(git.pid, 1) {
        Ok(output) => {
            ensure!(
                same_file(&output, &pipe),
                "Git output does not feed its pager"
            );
            let error = descriptor(git.pid, 2)?;
            ensure!(
                same_file(&error, &terminal) || same_file(&error, &pipe),
                "Git stderr is redirected"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Git closes both pipe writers before waiting for the pager at exit.
            ensure!(
                descriptor(git.pid, 2)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "Git pager output closure is ambiguous"
            );
            let mut terminal_copies = 0;
            for entry in std::fs::read_dir(format!("/proc/{}/fd", git.pid))? {
                let entry = entry?;
                let fd: u32 = entry
                    .file_name()
                    .to_str()
                    .context("invalid descriptor name")?
                    .parse()?;
                if fd >= 3 && same_file(&std::fs::metadata(entry.path())?, &terminal) {
                    terminal_copies += 1;
                }
            }
            ensure!(
                terminal_copies >= 2,
                "Git original terminal outputs are unavailable"
            );
        }
        Err(error) => return Err(error.into()),
    }
    ensure!(
        process(git.pid)? == *git && process(pager.pid)? == *pager,
        "Git pager process changed during capture"
    );
    Ok(())
}

pub fn argv_and_cwd(expected: &Process) -> Result<(Vec<String>, String)> {
    ensure!(
        process(expected.pid)? == *expected,
        "process changed during capture"
    );
    let bytes = crate::store::read_bytes(Path::new(&format!("/proc/{}/cmdline", expected.pid)))?;
    let argv = decode_argv(&bytes)?;
    let cwd = std::fs::read_link(format!("/proc/{}/cwd", expected.pid))?
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("non-UTF-8 cwd refused"))?;
    ensure!(
        process(expected.pid)? == *expected,
        "PID reused during capture"
    );
    Ok((argv, cwd))
}

pub fn environment(expected: &Process) -> Result<Vec<u8>> {
    ensure!(
        process(expected.pid)? == *expected,
        "process changed before environment capture"
    );
    let bytes = crate::store::read_bytes(Path::new(&format!("/proc/{}/environ", expected.pid)))?;
    ensure!(
        process(expected.pid)? == *expected,
        "process changed during environment capture"
    );
    ensure!(
        bytes.is_empty() || bytes.last() == Some(&0),
        "truncated process environment"
    );
    Ok(bytes)
}

pub fn generation(socket: &Path) -> Result<String> {
    use nix::sys::socket::{
        AddressFamily, SockFlag, SockType, UnixAddr, connect, getsockopt, socket as new_socket,
        sockopt::PeerCredentials,
    };
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileTypeExt;
    let meta = std::fs::symlink_metadata(socket).context("target socket is unavailable")?;
    ensure!(meta.file_type().is_socket(), "target is not a Unix socket");
    let fd = new_socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    connect(fd.as_raw_fd(), &UnixAddr::new(socket)?).context("cannot establish server identity")?;
    IDENTITY_CONNECTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let peer = getsockopt(&fd, PeerCredentials)?;
    ensure!(
        peer.uid() == nix::unistd::geteuid().as_raw(),
        "Herdr peer belongs to another user"
    );
    let pid: u32 = peer.pid().try_into().context("invalid Herdr peer PID")?;
    let identity = process(pid)?;
    let boot = read_proc_text("/proc/sys/kernel/random/boot_id")?;
    Ok(crate::store::digest(
        format!("linux-server-v1:{}:{}:{}", boot.trim(), pid, identity.start).as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_of_another_thread_is_not_mistaken_for_an_idle_process() {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut child = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap();
            ready_tx.send(()).unwrap();
            let _ = stop_rx.recv();
            child.kill().unwrap();
            child.wait().unwrap();
        });
        ready_rx.recv().unwrap();
        let result = has_children(&process(std::process::id()).unwrap());
        stop_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(result.unwrap());
    }
}
