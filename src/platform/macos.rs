//! Darwin process inspection. Unsafe code is confined to checked libproc/sysctl
//! calls here; the rest of the crate continues to deny it.
use super::{Process, decode_argv};
use anyhow::{Context, Result, ensure};
use nix::libc;
use std::collections::HashMap;
use std::mem::{MaybeUninit, size_of};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

fn pid(pid: u32) -> Result<i32> {
    ensure!(pid > 0, "invalid process PID");
    pid.try_into().context("invalid process PID")
}

fn bsd_info(id: u32) -> Result<libc::proc_bsdinfo> {
    let id = pid(id)?;
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    // SAFETY: libproc receives an aligned buffer of exactly the flavor's ABI
    // type. It is read only after a complete result, and all fields are POD.
    #[allow(unsafe_code)]
    let count = unsafe {
        libc::proc_pidinfo(
            id,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    if count <= 0 {
        return Err(std::io::Error::last_os_error()).context("process identity unavailable");
    }
    ensure!(
        count as usize == size_of::<libc::proc_bsdinfo>(),
        "truncated process identity"
    );
    // SAFETY: the preceding call filled the entire POD struct.
    #[allow(unsafe_code)]
    let info = unsafe { info.assume_init() };
    ensure!(info.pbi_pid == id as u32, "process identity mismatch");
    Ok(info)
}

pub fn process(id: u32) -> Result<Process> {
    let info = bsd_info(id)?;
    Ok(Process {
        pid: info.pbi_pid,
        parent: info.pbi_ppid,
        group: info.pbi_pgid,
        start: info
            .pbi_start_tvsec
            .checked_mul(1_000_000)
            .and_then(|s| s.checked_add(info.pbi_start_tvusec))
            .context("invalid process start time")?,
    })
}

pub fn process_table() -> Result<HashMap<u32, Process>> {
    let mut capacity = 1024;
    let pids = loop {
        ensure!(capacity <= 1_048_576, "process table exceeds size limit");
        let mut pids = vec![0i32; capacity];
        // SAFETY: the initialized buffer has the stated byte capacity. This
        // API returns a PID count, unlike proc_listpids (which returns bytes).
        #[allow(unsafe_code)]
        let count = unsafe {
            libc::proc_listallpids(
                pids.as_mut_ptr().cast(),
                (pids.len() * size_of::<i32>()) as i32,
            )
        };
        ensure!(count > 0, "process enumeration failed");
        if (count as usize) < pids.len() {
            pids.truncate(count as usize);
            break pids;
        }
        capacity *= 2;
    };
    let mut table = HashMap::new();
    for id in pids.into_iter().filter(|id| *id > 0) {
        let identity = process(id as u32).or_else(|error| {
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.raw_os_error() == Some(libc::EPERM))
            {
                ancestry_only(id)
            } else {
                Err(error)
            }
        });
        match identity {
            Ok(info) => {
                table.insert(info.pid, info);
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.raw_os_error() == Some(libc::ESRCH)) => {}
            Err(error) => return Err(error).context("process table is inaccessible"),
        }
    }
    Ok(table)
}

pub fn has_children(shell: &Process) -> Result<bool> {
    ensure!(
        process(shell.pid)? == *shell,
        "shell changed during idle check"
    );
    let mut child = 0i32;
    nix::errno::Errno::clear();
    // SAFETY: proc_listchildpids writes at most the supplied buffer size and
    // returns a PID count. One entry suffices to prove that the shell is busy.
    #[allow(unsafe_code)]
    let count = unsafe {
        libc::proc_listchildpids(
            pid(shell.pid)?,
            (&mut child as *mut i32).cast(),
            size_of::<i32>() as i32,
        )
    };
    ensure!(
        count >= 0 && (count != 0 || nix::errno::Errno::last_raw() == 0),
        "shell child enumeration failed"
    );
    ensure!(
        process(shell.pid)? == *shell,
        "shell changed during idle check"
    );
    Ok(count > 0)
}

fn ancestry_only(id: i32) -> Result<Process> {
    // Full BSD info is restricted to the same user. Keep other users' parent
    // and group IDs so privileged children/pipeline members cannot disappear
    // from safety checks. A zero start is never accepted as a capture identity:
    // capture/idle checks must independently succeed through process().
    let mut info = MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    // SAFETY: the buffer matches the flavor's POD ABI type and exact size.
    #[allow(unsafe_code)]
    let count = unsafe {
        libc::proc_pidinfo(
            id,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<libc::proc_bsdshortinfo>() as i32,
        )
    };
    if count <= 0 {
        return Err(std::io::Error::last_os_error()).context("process ancestry unavailable");
    }
    ensure!(
        count as usize == size_of::<libc::proc_bsdshortinfo>(),
        "truncated process ancestry"
    );
    // SAFETY: complete POD result, checked above.
    #[allow(unsafe_code)]
    let info = unsafe { info.assume_init() };
    ensure!(
        info.pbsi_pid == id as u32,
        "process ancestry identity mismatch"
    );
    Ok(Process {
        pid: info.pbsi_pid,
        parent: info.pbsi_ppid,
        group: info.pbsi_pgid,
        start: 0,
    })
}

fn c_path(bytes: &[u8]) -> Result<PathBuf> {
    let end = bytes
        .iter()
        .position(|b| *b == 0)
        .context("unterminated process path")?;
    let path = std::str::from_utf8(&bytes[..end]).context("non-UTF-8 process path refused")?;
    ensure!(
        Path::new(path).is_absolute(),
        "process path is not absolute"
    );
    Ok(path.into())
}

pub fn executable(id: u32) -> Result<PathBuf> {
    let id = pid(id)?;
    let mut bytes = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is writable for the supplied byte length.
    #[allow(unsafe_code)]
    let count = unsafe { libc::proc_pidpath(id, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    ensure!(
        count > 0 && (count as usize) < bytes.len(),
        "process executable unavailable"
    );
    c_path(&bytes)
}

fn procargs(id: u32) -> Result<Vec<u8>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid(id)?];
    let mut argmax = 0i32;
    let mut size = size_of::<i32>();
    // SAFETY: kern.argmax writes one integer; the output size is exact.
    #[allow(unsafe_code)]
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.argmax".as_ptr(),
            (&mut argmax as *mut i32).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    ensure!(
        result == 0
            && size == size_of::<i32>()
            && argmax > 0
            && argmax as usize <= crate::model::MAX_BYTES,
        "invalid kernel argument limit"
    );
    // KERN_PROCARGS2 rejects a capacity larger than the kernel's ARG_MAX.
    let mut bytes = vec![0u8; argmax as usize];
    let mut length = bytes.len();
    // SAFETY: both MIB and output are valid for their supplied lengths; null
    // newp makes this a read-only sysctl.
    #[allow(unsafe_code)]
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            bytes.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("process arguments unavailable");
    }
    ensure!(length <= bytes.len(), "process arguments exceed size limit");
    bytes.truncate(length);
    Ok(bytes)
}

fn split_procargs(bytes: &[u8]) -> Result<(Vec<String>, Option<Vec<u8>>)> {
    let argc = i32::from_ne_bytes(bytes.get(..4).context("missing process argc")?.try_into()?);
    ensure!((1..=4096).contains(&argc), "invalid process argument count");
    let mut offset = 4;
    offset += bytes[offset..]
        .iter()
        .position(|b| *b == 0)
        .context("missing executable path")?
        + 1;
    while bytes.get(offset) == Some(&0) {
        offset += 1;
    }
    let start = offset;
    for _ in 0..argc {
        offset += bytes
            .get(offset..)
            .context("truncated process argv")?
            .iter()
            .position(|b| *b == 0)
            .context("truncated process argv")?
            + 1;
    }
    let argv = decode_argv(&bytes[start..offset])?;
    // SIP-protected executables may expose argv but omit the environment
    // entirely. Do not treat that as a known-empty environment and silently
    // select the wrong agent profile.
    if offset == bytes.len() {
        return Ok((argv, None));
    }
    let start = offset;
    while let Some(byte) = bytes.get(offset) {
        if *byte == 0 {
            break;
        }
        offset += bytes[offset..]
            .iter()
            .position(|b| *b == 0)
            .context("truncated process environment")?
            + 1;
    }
    Ok((argv, Some(bytes[start..offset].to_vec())))
}

pub fn argv_and_cwd(expected: &Process) -> Result<(Vec<String>, String)> {
    ensure!(
        process(expected.pid)? == *expected,
        "process changed during capture"
    );
    let (argv, _) = split_procargs(&procargs(expected.pid)?)?;
    let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    // SAFETY: the buffer matches the requested flavor and is read only after
    // libproc reports a complete POD result.
    #[allow(unsafe_code)]
    let count = unsafe {
        libc::proc_pidinfo(
            pid(expected.pid)?,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<libc::proc_vnodepathinfo>() as i32,
        )
    };
    ensure!(
        count as usize == size_of::<libc::proc_vnodepathinfo>(),
        "process cwd unavailable"
    );
    // SAFETY: the entire POD struct was initialized above.
    #[allow(unsafe_code)]
    let info = unsafe { info.assume_init() };
    let bytes: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .map(|b| *b as u8)
        .collect();
    let cwd = c_path(&bytes)?
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
    let (_, environment) = split_procargs(&procargs(expected.pid)?)?;
    ensure!(
        process(expected.pid)? == *expected,
        "process changed during environment capture"
    );
    environment
        .context("process environment unavailable (macOS may hide it for a protected executable)")
}

// These public Darwin ABI types are absent from libc. Layouts are from
// <sys/proc_info.h>; native tests also check their sizes against the SDK.
#[repr(C)]
struct FileInfo {
    openflags: u32,
    status: u32,
    offset: i64,
    kind: i32,
    guardflags: u32,
}

#[repr(C)]
struct VnodeFdInfo {
    file: FileInfo,
    vnode: libc::vnode_info,
}

#[repr(C)]
struct PipeInfo {
    stat: libc::vinfo_stat,
    handle: u64,
    peerhandle: u64,
    status: i32,
    reserved: i32,
}

#[repr(C)]
struct PipeFdInfo {
    file: FileInfo,
    pipe: PipeInfo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Descriptor {
    dev: u32,
    ino: u64,
    rdev: u32,
    mode: u16,
    access: u32,
}

impl Descriptor {
    fn same_file(self, other: Self) -> bool {
        (self.dev, self.ino, self.rdev, self.mode) == (other.dev, other.ino, other.rdev, other.mode)
    }
    fn matches(self, meta: &std::fs::Metadata) -> bool {
        // Darwin dev_t is signed; MetadataExt sign-extends it to u64 while
        // libproc's vinfo_stat stores the same 32 bits in an unsigned field.
        (self.dev, self.ino, self.rdev) == (meta.dev() as u32, meta.ino(), meta.rdev() as u32)
    }
    fn is_char(self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFCHR
    }
}

fn descriptor(id: u32, fd: i32) -> Result<Descriptor> {
    let id = pid(id)?;
    let mut info = MaybeUninit::<VnodeFdInfo>::zeroed();
    // SAFETY: PROC_PIDFDVNODEINFO (1) writes vnode_fdinfo, whose repr(C)
    // layout above matches the SDK. No uninitialized data is read.
    #[allow(unsafe_code)]
    let count = unsafe {
        libc::proc_pidfdinfo(
            id,
            fd,
            1,
            info.as_mut_ptr().cast(),
            size_of::<VnodeFdInfo>() as i32,
        )
    };
    if count <= 0 {
        return Err(std::io::Error::last_os_error())
            .context("descriptor is not an accessible vnode");
    }
    ensure!(
        count as usize == size_of::<VnodeFdInfo>(),
        "truncated descriptor metadata"
    );
    // SAFETY: complete POD result, checked above.
    #[allow(unsafe_code)]
    let info = unsafe { info.assume_init() };
    let stat = info.vnode.vi_stat;
    Ok(Descriptor {
        dev: stat.vst_dev,
        ino: stat.vst_ino,
        rdev: stat.vst_rdev,
        mode: stat.vst_mode,
        access: info.file.openflags & 3,
    })
}

fn descriptors(id: u32) -> Result<Vec<libc::proc_fdinfo>> {
    let id = pid(id)?;
    // One extra entry detects overflow without silently ignoring descriptors.
    let mut entries = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        4097
    ];
    let length = entries.len() * size_of::<libc::proc_fdinfo>();
    // SAFETY: a correctly aligned, initialized array with its exact byte size.
    #[allow(unsafe_code)]
    let count = unsafe {
        libc::proc_pidinfo(
            id,
            libc::PROC_PIDLISTFDS,
            0,
            entries.as_mut_ptr().cast(),
            length as i32,
        )
    };
    ensure!(
        count > 0
            && (count as usize) < length
            && (count as usize).is_multiple_of(size_of::<libc::proc_fdinfo>()),
        "descriptor enumeration failed or exceeded limit"
    );
    entries.truncate(count as usize / size_of::<libc::proc_fdinfo>());
    Ok(entries)
}

fn controlling_terminal(id: u32) -> Result<u32> {
    let info = bsd_info(id)?;
    ensure!(
        info.e_tdev != u32::MAX,
        "process has no controlling terminal"
    );
    Ok(info.e_tdev)
}

pub fn terminal_stdio(id: u32, shell: u32) -> Result<[bool; 3]> {
    let terminal = descriptor(shell, 0)?;
    ensure!(
        terminal.is_char()
            && terminal.rdev == controlling_terminal(shell)?
            && terminal.rdev == controlling_terminal(id)?,
        "pane streams do not match the controlling terminal"
    );
    let null = std::fs::metadata("/dev/null")?;
    let mut null_stdio = [false; 3];
    for (fd, is_null) in null_stdio.iter_mut().enumerate() {
        let stream = descriptor(id, fd as i32)?;
        *is_null = fd != 0 && stream.is_char() && stream.matches(&null);
        ensure!(
            stream.same_file(terminal) || *is_null,
            "redirected standard streams are not representable as argv"
        );
    }
    Ok(null_stdio)
}

fn system_executable(id: u32, path: &str) -> Result<bool> {
    let running = std::fs::metadata(executable(id)?)?;
    let native = std::fs::metadata(path)?;
    Ok((running.dev(), running.ino()) == (native.dev(), native.ino()))
}

pub fn ssh_terminal_stdout(id: u32) -> Result<()> {
    ensure!(
        system_executable(id, "/usr/bin/ssh")?,
        "SSH runtime stream recovery requires the system OpenSSH executable"
    );
    let input = descriptor(id, 0)?;
    let error = descriptor(id, 2)?;
    ensure!(
        input.is_char() && input.rdev == controlling_terminal(id)?,
        "SSH input is not its controlling terminal"
    );
    let fds = descriptors(id)?;
    let mut outputs = Vec::new();
    for fd in fds
        .iter()
        .filter(|fd| fd.proc_fd >= 3 && fd.proc_fd <= i32::MAX - 2)
    {
        let n = fd.proc_fd;
        if fd.proc_fdtype == 1
            && fds.iter().any(|f| f.proc_fd == n + 1)
            && fds.iter().any(|f| f.proc_fd == n + 2 && f.proc_fdtype == 1)
            && descriptor(id, n)? == input
            && descriptor(id, n + 2)? == error
        {
            // A pipe or socket in the output position is a redirection too.
            outputs.push(descriptor(id, n + 1).ok());
        }
    }
    ensure!(
        outputs.len() == 1,
        "SSH session output descriptors are ambiguous; capture refused"
    );
    ensure!(
        outputs[0] == Some(input),
        "SSH session output is redirected; reconnect with terminal output before saving"
    );
    Ok(())
}

fn pipe(id: u32, fd: i32) -> Result<PipeInfo> {
    let id = pid(id)?;
    let mut info = MaybeUninit::<PipeFdInfo>::zeroed();
    // SAFETY: PROC_PIDFDPIPEINFO (6) uses the repr(C) pipe_fdinfo layout above.
    #[allow(unsafe_code)]
    let count = unsafe {
        libc::proc_pidfdinfo(
            id,
            fd,
            6,
            info.as_mut_ptr().cast(),
            size_of::<PipeFdInfo>() as i32,
        )
    };
    ensure!(
        count as usize == size_of::<PipeFdInfo>(),
        "pipe metadata unavailable"
    );
    // SAFETY: complete POD result, checked above.
    #[allow(unsafe_code)]
    let info = unsafe { info.assume_init() };
    Ok(info.pipe)
}

pub fn git_pager_stdio(table: &HashMap<u32, Process>, git: &Process, shell: u32) -> Result<()> {
    // /usr/bin/git is a developer-tool shim on macOS. Resolve the installed
    // tool using filesystem metadata only, including custom xcode-select paths.
    let running = std::fs::metadata(executable(git.pid)?)?;
    let mut system_git = false;
    for path in [
        "/usr/bin/git",
        "/var/db/xcode_select_link/usr/bin/git",
        "/Library/Developer/CommandLineTools/usr/bin/git",
        "/Applications/Xcode.app/Contents/Developer/usr/bin/git",
    ] {
        match std::fs::metadata(path) {
            Ok(native) => {
                system_git |= (running.dev(), running.ino()) == (native.dev(), native.ino())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(system_git, "pager recovery requires system Git");
    let terminal = descriptor(shell, 0)?;
    ensure!(
        terminal.is_char()
            && terminal.rdev == controlling_terminal(shell)?
            && terminal.rdev == controlling_terminal(git.pid)?
            && descriptor(git.pid, 0)?.same_file(terminal),
        "Git input is not the pane terminal"
    );
    let children: Vec<_> = table.values().filter(|p| p.parent == git.pid).collect();
    ensure!(children.len() == 1, "Git pager child is ambiguous");
    let pager = children[0];
    ensure!(
        pager.group == git.group && process(pager.pid)? == *pager,
        "Git pager identity changed"
    );
    ensure!(
        system_executable(pager.pid, "/usr/bin/less")?
            || system_executable(pager.pid, "/usr/bin/more")?,
        "Git pager is not system less or more"
    );
    let input = pipe(pager.pid, 0)?;
    ensure!(
        terminal.rdev == controlling_terminal(pager.pid)?
            && descriptor(pager.pid, 1)?.same_file(terminal)
            && descriptor(pager.pid, 2)?.same_file(terminal),
        "Git pager output is redirected"
    );
    let fds = descriptors(git.pid)?;
    if fds.iter().any(|fd| fd.proc_fd == 1) {
        let output = pipe(git.pid, 1)?;
        ensure!(
            output.handle != 0
                && output.peerhandle == input.handle
                && input.peerhandle == output.handle,
            "Git output does not feed its pager"
        );
        if !descriptor(git.pid, 2).is_ok_and(|d| d.same_file(terminal)) {
            ensure!(
                pipe(git.pid, 2)?.handle == output.handle,
                "Git stderr is redirected"
            );
        }
    } else {
        ensure!(
            !fds.iter().any(|fd| fd.proc_fd == 2),
            "Git pager output closure is ambiguous"
        );
        let mut copies = 0;
        for fd in fds
            .iter()
            .filter(|fd| fd.proc_fd >= 3 && fd.proc_fdtype == 1)
        {
            if descriptor(git.pid, fd.proc_fd)?.same_file(terminal) {
                copies += 1;
            }
        }
        ensure!(copies >= 2, "Git original terminal outputs are unavailable");
    }
    ensure!(
        process(git.pid)? == *git && process(pager.pid)? == *pager,
        "Git pager process changed during capture"
    );
    Ok(())
}

pub fn generation(socket: &Path) -> Result<String> {
    use nix::sys::socket::{
        UnixAddr, connect, getsockopt,
        sockopt::{LocalPeerCred, LocalPeerPid},
    };
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileTypeExt;
    ensure!(
        std::fs::symlink_metadata(socket)?.file_type().is_socket(),
        "target is not a Unix socket"
    );
    let fd = crate::transport::nonblocking_socket()?;
    connect(fd.as_raw_fd(), &UnixAddr::new(socket)?).context("cannot establish server identity")?;
    super::IDENTITY_CONNECTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let peer = getsockopt(&fd, LocalPeerCred)?;
    ensure!(
        peer.uid() == nix::unistd::geteuid().as_raw(),
        "Herdr peer belongs to another user"
    );
    let id: u32 = getsockopt(&fd, LocalPeerPid)?
        .try_into()
        .context("invalid Herdr peer PID")?;
    let identity = process(id)?;
    let mut boot = [0u8; 128];
    let mut length = boot.len();
    // SAFETY: the literal is NUL-terminated; output has the supplied size and
    // null newp requests a read. kern.bootsessionuuid is stable across sleep.
    #[allow(unsafe_code)]
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            boot.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    ensure!(
        result == 0 && length > 1 && length <= boot.len(),
        "boot identity unavailable"
    );
    let boot = std::str::from_utf8(&boot[..length])?.trim_end_matches('\0');
    Ok(crate::store::digest(
        format!("macos-server-v1:{boot}:{id}:{}", identity.start).as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn procargs_preserves_empty_and_literal_arguments_and_separates_environment() {
        let mut bytes = 4i32.to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/bin/program\0\0\0program\0\0space ' $ here\0\xe4\xb8\xad\0PROFILE=local\0SECRET=private\0\0\0");
        let (argv, env) = split_procargs(&bytes).unwrap();
        assert_eq!(argv, ["program", "", "space ' $ here", "中"]);
        assert_eq!(env.unwrap(), b"PROFILE=local\0SECRET=private\0");
        assert!(split_procargs(&bytes[..20]).is_err());
        assert!(split_procargs(&0i32.to_ne_bytes()).is_err());
    }

    #[test]
    fn native_process_metadata_and_abi() {
        assert_eq!(size_of::<FileInfo>(), 24);
        assert_eq!(size_of::<VnodeFdInfo>(), 176);
        assert_eq!(size_of::<PipeFdInfo>(), 184);
        let current = process(std::process::id()).unwrap();
        assert_eq!(process_table().unwrap().get(&current.pid), Some(&current));
        let (argv, cwd) = argv_and_cwd(&current).unwrap();
        assert!(!argv.is_empty());
        assert_eq!(Path::new(&cwd), std::env::current_dir().unwrap());
        assert_eq!(
            executable(current.pid).unwrap(),
            std::env::current_exe().unwrap()
        );
    }

    #[test]
    fn null_device_identity_matches_open_descriptor() {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open("/dev/null").unwrap();
        let descriptor = descriptor(std::process::id(), file.as_raw_fd()).unwrap();
        let meta = file.metadata().unwrap();
        assert!(
            descriptor.matches(&meta),
            "{descriptor:?} vs dev={} ino={} rdev={}",
            meta.dev(),
            meta.ino(),
            meta.rdev()
        );
    }

    #[test]
    fn child_environment_is_available() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::macos::tests::environment_child",
                "--ignored",
            ])
            .stdout(std::process::Stdio::null())
            .env("REVIVE_FIXTURE_PROFILE", "local")
            .spawn()
            .unwrap();
        let identity = process(child.id()).unwrap();
        let result = environment(&identity);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            result
                .unwrap()
                .split(|b| *b == 0)
                .any(|entry| entry == b"REVIVE_FIXTURE_PROFILE=local")
        );
    }

    #[test]
    #[ignore = "subprocess fixture for child_environment_is_available"]
    fn environment_child() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}
