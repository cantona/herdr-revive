use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::ops::Range;
use std::path::{Path, PathBuf};

pub const BEGIN: &str = "# >>> herdr-revive >>>";
pub const END: &str = "# <<< herdr-revive <<<";
/// Finds herdr-revive on PATH, else in the two places installs put it: a
/// login shell can run this from ~/.bashrc before ~/.profile has added
/// ~/.local/bin, and a desktop-started core hands its shells a bare PATH.
pub const RC_LINE: &str = r#"case $- in *i*) [ -n "${BASH_VERSION-}" ] && for __revive_found in "$(command -v herdr-revive 2>/dev/null)" "$HOME/.local/bin/herdr-revive" "$HOME/.cargo/bin/herdr-revive"; do [ -x "$__revive_found" ] && { eval "$(exec "$__revive_found" --host core init bash 2>/dev/null)"; break; }; done; unset __revive_found ;; esac"#;

pub fn rc_file(home: &Path) -> PathBuf {
    let macos = cfg!(target_os = "macos");
    // Core starts `bash -l` on macOS and an interactive bash elsewhere,
    // unless its config sets `shell`.
    let login = crate::core::paths::core_shell_is_login(home).unwrap_or(macos);
    rc_file_for(home, macos, login)
}

/// An interactive non-login bash (core's default on Linux) reads
/// ~/.bashrc. A login bash (`bash -l`, core's default on macOS or set by
/// `shell` in herdr-core's config) reads the first of ~/.bash_profile,
/// ~/.bash_login and ~/.profile; a fresh account gets ~/.bash_profile. On
/// Linux a login file that already sources ~/.bashrc keeps the hook there,
/// where Linux installs have always put it.
pub fn rc_file_for(home: &Path, macos: bool, login: bool) -> PathBuf {
    if !login {
        return home.join(".bashrc");
    }
    let profile = [".bash_profile", ".bash_login", ".profile"]
        .iter()
        .map(|name| home.join(name))
        .find(|path| std::fs::symlink_metadata(path).is_ok())
        .unwrap_or_else(|| home.join(".bash_profile"));
    if !macos
        && let Ok(text) = std::fs::read_to_string(&profile)
        && sources_bashrc(&text)
    {
        // Both files are read then. A block already in the login file stays
        // there: a dotfiles login file shared with a Mac, where it is the
        // only rc file, must not lose it.
        if blocks(&text).is_ok_and(|found| !found.is_empty()) {
            return profile;
        }
        return home.join(".bashrc");
    }
    profile
}

/// Refuses when `target` no longer holds `read`: an editor saved it while
/// the block was being worked out, and writing now would drop that edit.
fn unchanged_since(target: &Path, read: &str) -> Result<()> {
    ensure!(
        read_rc(target)? == read,
        "{} changed while install was writing it; run install again",
        target.display()
    );
    Ok(())
}

/// A command, not a comment, an alias's text or a function body, that runs
/// ~/.bashrc itself with `.` or `source`: first on its line or after `;`,
/// `&&`, `||`, `then`, `else` or `do`, naming it as `~/.bashrc`,
/// `$HOME/.bashrc` or `${HOME}/.bashrc`. Inside `{ }` it may never run, so
/// it does not count; the login file is then chosen, which bash does read.
fn sources_bashrc(text: &str) -> bool {
    let bashrc = |path: &str| {
        let path = path.trim_matches(|c| c == '"' || c == '\'');
        ["~/.bashrc", "$HOME/.bashrc", "${HOME}/.bashrc"].contains(&path)
    };
    let mut depth = 0i32;
    for line in text.lines() {
        let spaced = line.replace(';', " ; ");
        let tokens: Vec<&str> = spaced.split_whitespace().collect();
        let mut command_start = true;
        for (at, token) in tokens.iter().enumerate() {
            if token.starts_with('#') {
                break;
            }
            if token.starts_with('}') {
                depth -= 1;
                command_start = false;
                continue;
            }
            if token.ends_with('{') {
                depth += 1;
                command_start = true;
                continue;
            }
            if command_start
                && depth == 0
                && (*token == "." || *token == "source")
                && tokens.get(at + 1).is_some_and(|path| bashrc(path))
            {
                return true;
            }
            command_start = matches!(*token, ";" | "&&" | "||" | "then" | "else" | "do" | "!");
        }
    }
    false
}

/// The other bash startup files that exist, which may still hold a block
/// from when core's shell read them (its `shell` setting changed since).
fn other_rc_files(home: &Path) -> Vec<PathBuf> {
    let chosen = rc_file(home);
    let chosen_target = resolve(&chosen).ok();
    [".bashrc", ".bash_profile", ".bash_login", ".profile"]
        .iter()
        .map(|name| home.join(name))
        .filter(|path| *path != chosen && std::fs::symlink_metadata(path).is_ok())
        .filter(|path| resolve(path).ok() != chosen_target)
        .collect()
}

/// The other startup files that hold a block.
fn blocks_elsewhere(home: &Path) -> Vec<PathBuf> {
    other_rc_files(home)
        .into_iter()
        .filter(|link| inspect_rc(link).is_ok_and(|(installed, _)| installed))
        .collect()
}

pub fn block() -> String {
    format!("{BEGIN}\n{RC_LINE}\n{END}\n")
}

struct Block {
    range: Range<usize>,
    canonical: bool,
}

fn unmatched(line: usize) -> anyhow::Error {
    anyhow!("rc file has an unmatched herdr-revive marker at line {line}")
}

fn blocks(text: &str) -> Result<Vec<Block>> {
    let mut found = Vec::new();
    let mut open: Option<(usize, usize, usize)> = None;
    let mut offset = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let end = offset + line.len();
        let marker = line.trim_end();
        if marker == BEGIN {
            if let Some((_, begun, _)) = open {
                return Err(unmatched(begun));
            }
            open = Some((offset, index + 1, end));
        } else if marker == END {
            let (start, _, inner) = open.take().ok_or_else(|| unmatched(index + 1))?;
            found.push(Block {
                range: start..end,
                canonical: text[inner..offset] == format!("{RC_LINE}\n"),
            });
        }
        offset = end;
    }
    match open {
        Some((_, begun, _)) => Err(unmatched(begun)),
        None => Ok(found),
    }
}

fn splice(text: &str, found: &[Block], replacement: Option<&str>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut replacement = replacement;
    let mut kept = 0;
    for block in found {
        out.push_str(&text[kept..block.range.start]);
        if let Some(new) = replacement.take() {
            out.push_str(new);
        }
        kept = block.range.end;
    }
    out.push_str(&text[kept..]);
    out
}

fn add(text: &str, found: &[Block]) -> String {
    match found {
        [] => {
            let mut out = text.to_owned();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&block());
            out
        }
        [only] if only.canonical => text.to_owned(),
        _ => splice(text, found, Some(&block())),
    }
}

pub fn has_block(text: &str) -> Result<bool> {
    Ok(!blocks(text)?.is_empty())
}

pub fn without_block(text: &str) -> Result<String> {
    Ok(splice(text, &blocks(text)?, None))
}

pub fn with_block(text: &str) -> Result<String> {
    Ok(add(text, &blocks(text)?))
}

pub fn run(home: &Path, write: bool, remove: bool) -> Result<Value> {
    let link = rc_file(home);
    let target = resolve(&link)?;
    let current = read_rc(&target)?;
    let found = blocks(&current).with_context(|| target.display().to_string())?;
    let mut installed = !found.is_empty();
    let mut up_to_date = matches!(found.as_slice(), [only] if only.canonical);
    let wanted = if remove {
        Some(splice(&current, &found, None))
    } else if write {
        Some(add(&current, &found))
    } else {
        None
    };
    let mut backup = None;
    let status = match wanted {
        None => "dry_run",
        Some(wanted) => {
            let status = if wanted == current {
                "unchanged"
            } else {
                let mode = writable_mode(&target)?;
                if !current.is_empty() {
                    backup = Some(back_up(&target, current.as_bytes(), mode)?);
                }
                unchanged_since(&target, &current)?;
                replace(&target, wanted.as_bytes(), mode)?;
                if remove { "removed" } else { "written" }
            };
            installed = !remove;
            up_to_date = !remove;
            status
        }
    };
    // A block left in a file core's shell no longer reads goes with any
    // write or removal, so the hook never runs twice or from the wrong file;
    // only after the chosen file is done, so a failed write leaves it.
    let mut removed_elsewhere = Vec::new();
    if write || remove {
        for link in blocks_elsewhere(home) {
            let target = resolve(&link)?;
            let current = read_rc(&target)?;
            let found = blocks(&current).with_context(|| target.display().to_string())?;
            let mode = writable_mode(&target)?;
            back_up(&target, current.as_bytes(), mode)?;
            unchanged_since(&target, &current)?;
            replace(&target, splice(&current, &found, None).as_bytes(), mode)?;
            removed_elsewhere.push(link);
        }
    }
    Ok(json!({
        "status": status,
        "rc_file": link,
        "target": target,
        "rc_line": RC_LINE,
        "installed": installed,
        "up_to_date": up_to_date,
        "backup": backup,
        "removed_elsewhere": removed_elsewhere,
        "elsewhere": if write || remove { Vec::new() } else { blocks_elsewhere(home) },
    }))
}

fn read_rc(target: &Path) -> Result<String> {
    match std::fs::read(target) {
        Ok(bytes) => {
            String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", target.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", target.display())),
    }
}

fn inspect_rc(link: &Path) -> Result<(bool, bool)> {
    let target = resolve(link)?;
    let current = read_rc(&target)?;
    let found = blocks(&current).with_context(|| target.display().to_string())?;
    Ok((
        !found.is_empty(),
        matches!(found.as_slice(), [only] if only.canonical),
    ))
}

/// Read-only report for `status`: a missing rc file is "not installed", any
/// other failure is reported in `rc_error` instead of failing the caller.
pub fn inspect(home: &Path) -> Value {
    let link = rc_file(home);
    let (installed, up_to_date, mut error) = match inspect_rc(&link) {
        Ok((installed, up_to_date)) => (json!(installed), json!(up_to_date), Value::Null),
        Err(e) => (Value::Null, Value::Null, json!(format!("{e:#}"))),
    };
    let elsewhere = blocks_elsewhere(home);
    if error.is_null()
        && let Some(stray) = elsewhere.first()
    {
        error = json!(format!(
            "the hook is also in {}, but herdr-core's shell is set up to read {}: run install --write to keep only that one",
            stray.display(),
            link.display()
        ));
    }
    json!({
        "binary": std::env::current_exe().ok(),
        "rc_file": link,
        "rc_line": RC_LINE,
        "rc_installed": installed,
        "rc_up_to_date": up_to_date,
        "rc_error": error,
    })
}

fn resolve(link: &Path) -> Result<PathBuf> {
    match std::fs::canonicalize(link) {
        Ok(path) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(link).is_ok() {
                bail!("rc file {} is a dangling symlink", link.display());
            }
            let parent = link
                .parent()
                .with_context(|| format!("rc file {} has no directory", link.display()))?;
            let name = link
                .file_name()
                .with_context(|| format!("rc file {} has no name", link.display()))?;
            Ok(std::fs::canonicalize(parent)
                .with_context(|| format!("home directory {} is unavailable", parent.display()))?
                .join(name))
        }
        Err(e) => Err(e).with_context(|| format!("cannot resolve rc file {}", link.display())),
    }
}

/// The mode to give the edited file and its backup; a new rc file gets 0644.
/// Refuses a file owned by someone else, so `sudo` with an inherited HOME
/// cannot leave a root-owned rc file behind.
fn writable_mode(path: &Path) -> Result<u32> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let euid = nix::unistd::geteuid().as_raw();
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().context("rc file has no parent directory")?;
            let dir = std::fs::metadata(parent)
                .with_context(|| format!("cannot inspect {}", parent.display()))?;
            ensure!(
                dir.uid() == euid,
                "{} is owned by uid {}, not the current user (uid {euid}); run install as its owner",
                parent.display(),
                dir.uid()
            );
            return Ok(0o644);
        }
        Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", path.display())),
    };
    ensure!(
        meta.uid() == euid,
        "{} is owned by uid {}, not the current user (uid {euid}); run install as its owner",
        path.display(),
        meta.uid()
    );
    Ok(meta.permissions().mode() & 0o7777)
}

fn backup_path(target: &Path) -> Result<PathBuf> {
    let mut name = target
        .file_name()
        .with_context(|| format!("rc file {} has no name", target.display()))?
        .to_os_string();
    name.push(".revive-backup");
    Ok(target.with_file_name(name))
}

/// An existing backup holds the content from before the first edit, so it is never replaced.
fn back_up(target: &Path, bytes: &[u8], mode: u32) -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let path = backup_path(target)?;
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::metadata(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("backup path {} is a dangling symlink", path.display())
            }
            Err(e) => {
                return Err(e).with_context(|| format!("cannot inspect backup {}", path.display()));
            }
        },
        Ok(_) => return Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("cannot inspect backup {}", path.display()));
        }
    }
    let context = || format!("cannot write backup {}", path.display());
    let parent = path.parent().with_context(context)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).with_context(context)?;
    temp.write_all(bytes).with_context(context)?;
    temp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))
        .with_context(context)?;
    temp.as_file().sync_all().with_context(context)?;
    match temp.persist_noclobber(&path) {
        Ok(_) => {}
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(path),
        Err(e) => return Err(e.error).with_context(context),
    }
    std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .with_context(context)?;
    Ok(path)
}

fn replace(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let context = || format!("cannot write {}", path.display());
    // A hard-linked rc file (a dotfiles checkout) is rewritten in place: a
    // rename would give this path a new inode and leave the other link with
    // the old text. That gives up atomicity, which only a link can justify.
    if std::fs::metadata(path).is_ok_and(|meta| meta.nlink() > 1) {
        // The text as it is now is kept beside it until the rewrite is on
        // disk: a write that fails half way (a full disk) must not leave the
        // rc file truncated with no copy of what it held.
        let mut keep = path.as_os_str().to_owned();
        keep.push(".revive-old");
        let keep = PathBuf::from(keep);
        // One left by a rewrite that failed holds the only full copy.
        ensure!(
            !keep.exists(),
            "{} exists from an earlier rewrite that failed; restore {} from it or remove it",
            keep.display(),
            path.display()
        );
        let mut copy = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&keep)
            .with_context(context)?;
        // A copy cut short (a full disk) is no copy: the next run would tell
        // the user to restore the rc file from it.
        if let Err(error) = std::fs::read(path)
            .and_then(|text| copy.write_all(&text))
            .and_then(|()| copy.sync_all())
        {
            let _ = std::fs::remove_file(&keep);
            return Err(error).with_context(context);
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .with_context(context)?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| {
                format!(
                    "cannot write {}; its previous text is in {}",
                    path.display(),
                    keep.display()
                )
            })?;
        return std::fs::remove_file(&keep).with_context(|| {
            format!(
                "{} was rewritten, but {} could not be removed: remove it",
                path.display(),
                keep.display()
            )
        });
    }
    let parent = path.parent().with_context(context)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).with_context(context)?;
    temp.write_all(bytes).with_context(context)?;
    temp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))
        .with_context(context)?;
    temp.as_file().sync_all().with_context(context)?;
    temp.persist(path)
        .map_err(|e| e.error)
        .with_context(context)?;
    std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .with_context(context)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn writing_keeps_a_hard_linked_rc_file_linked() {
        use std::os::unix::fs::MetadataExt;
        let home = tempfile::tempdir().unwrap();
        let rc = rc_file(home.path());
        let other = home.path().join("dotfiles-copy");
        std::fs::write(&rc, "PS1='> '\n").unwrap();
        std::fs::hard_link(&rc, &other).unwrap();
        run(home.path(), true, false).unwrap();
        assert_eq!(
            std::fs::metadata(&rc).unwrap().ino(),
            std::fs::metadata(&other).unwrap().ino()
        );
        assert!(std::fs::read_to_string(&other).unwrap().contains(RC_LINE));
        run(home.path(), false, true).unwrap();
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "PS1='> '\n");
    }

    #[test]
    fn the_rc_line_finds_the_binary_off_path_and_keeps_the_shell_clean() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("herdr-revive");
        std::fs::write(&fake, "#!/bin/sh\necho \"echo hooked-from-$0\"\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = format!("{RC_LINE}\necho \"leftover=[${{__revive_found-unset}}]\"\n");
        let out = std::process::Command::new("bash")
            .args(["--norc", "--noprofile", "-i", "-c", &script])
            .env("HOME", home.path())
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(&format!("hooked-from-{}", fake.display())),
            "{stdout}"
        );
        assert!(stdout.contains("leftover=[unset]"), "{stdout}");
    }

    #[test]
    fn rc_file_follows_the_login_shell_rule_per_os() {
        let home = tempfile::tempdir().unwrap();
        let rc = |macos, login| rc_file_for(home.path(), macos, login);
        let at = |name: &str| home.path().join(name);
        assert_eq!(rc(false, false), at(".bashrc"));
        assert_eq!(rc(true, true), at(".bash_profile"));
        assert_eq!(rc(false, true), at(".bash_profile"));
        std::fs::write(at(".profile"), "").unwrap();
        assert_eq!(rc(true, true), at(".profile"));
        std::fs::write(at(".bash_login"), "").unwrap();
        assert_eq!(rc(true, true), at(".bash_login"));
        assert_eq!(rc(false, true), at(".bash_login"));
        // A Linux login file that sources ~/.bashrc keeps the hook there,
        // unless the login file already holds the block.
        std::fs::write(at(".bash_login"), ". ~/.bashrc\n").unwrap();
        assert_eq!(rc(false, true), at(".bashrc"));
        std::fs::write(at(".bash_login"), format!(". ~/.bashrc\n{}", block())).unwrap();
        assert_eq!(rc(false, true), at(".bash_login"));
        std::fs::write(at(".bash_login"), ". ~/.bashrc\n").unwrap();
        assert_eq!(rc(true, true), at(".bash_login"));
        assert_eq!(rc(false, false), at(".bashrc"));
    }

    #[test]
    fn a_failed_write_leaves_the_block_in_the_other_file() {
        let home = tempfile::tempdir().unwrap();
        if std::env::var_os("XDG_CONFIG_HOME").is_some() {
            return;
        }
        let chosen = rc_file(home.path());
        let stray = [".bashrc", ".bash_profile"]
            .iter()
            .map(|name| home.path().join(name))
            .find(|path| *path != chosen)
            .unwrap();
        // A hard-linked rc file with a leftover copy refuses the write.
        std::fs::write(&chosen, "# mine\n").unwrap();
        std::fs::hard_link(&chosen, home.path().join("link")).unwrap();
        let mut keep = chosen.as_os_str().to_owned();
        keep.push(".revive-old");
        std::fs::write(&keep, "# old\n").unwrap();
        std::fs::write(&stray, format!("# other\n{}", block())).unwrap();
        assert!(run(home.path(), true, false).is_err());
        assert!(std::fs::read_to_string(&stray).unwrap().contains(BEGIN));
    }

    #[test]
    fn only_a_line_that_runs_bashrc_counts_as_sourcing_it() {
        assert!(sources_bashrc(". \"$HOME/.bashrc\"\n"));
        assert!(sources_bashrc(
            "if [ -f ~/.bashrc ]; then\n    . ~/.bashrc\nfi\n"
        ));
        assert!(sources_bashrc("[ -f ~/.bashrc ] && source ~/.bashrc\n"));
        assert!(!sources_bashrc("# like ~/.bashrc, but for login shells\n"));
        assert!(!sources_bashrc("export PATH=~/bin:$PATH\n"));
        assert!(!sources_bashrc("alias rl='source ~/.bashrc'\n"));
        assert!(!sources_bashrc(
            "[ -f ~/.bashrc.local ] && . ~/.bashrc.local\n"
        ));
        assert!(!sources_bashrc("source ~/.bashrc_aliases\n"));
        assert!(!sources_bashrc(". /etc/bash.bashrc\n"));
        assert!(sources_bashrc("    . \"${HOME}/.bashrc\"\n"));
        assert!(!sources_bashrc("reload() {\n  . ~/.bashrc\n}\n"));
        assert!(!sources_bashrc("reload() { . ~/.bashrc; }\n"));
        assert!(sources_bashrc("reload() {\n  :\n}\n. ~/.bashrc\n"));
        assert!(!sources_bashrc("echo . ~/.bashrc\n"));
        assert!(sources_bashrc(
            "if [ -f ~/.bashrc ]; then . ~/.bashrc; fi\n"
        ));
    }

    #[test]
    fn a_block_left_in_another_startup_file_is_reported_and_cleared() {
        let home = tempfile::tempdir().unwrap();
        // Only meaningful when no herdr-core config is ambient in the run.
        if std::env::var_os("XDG_CONFIG_HOME").is_some() {
            return;
        }
        let chosen = rc_file(home.path());
        let stray = [".bashrc", ".bash_profile"]
            .iter()
            .map(|name| home.path().join(name))
            .find(|path| *path != chosen)
            .unwrap();
        std::fs::write(&chosen, "# mine\n").unwrap();
        std::fs::write(&stray, format!("# other\n{}", block())).unwrap();
        let status = inspect(home.path());
        assert!(
            status["rc_error"]
                .as_str()
                .is_some_and(|e| e.contains("also in")),
            "{status}"
        );
        let dry = run(home.path(), false, false).unwrap();
        assert_eq!(dry["elsewhere"], json!([stray]));
        let done = run(home.path(), true, false).unwrap();
        assert_eq!(done["removed_elsewhere"], json!([stray]));
        assert_eq!(std::fs::read_to_string(&stray).unwrap(), "# other\n");
        assert!(std::fs::read_to_string(&chosen).unwrap().contains(BEGIN));
        assert!(inspect(home.path())["rc_error"].is_null());
    }

    #[test]
    fn block_edits_are_idempotent_and_reversible() {
        let original = "export A=1\n";
        let once = with_block(original).unwrap();
        assert!(once.starts_with(original));
        assert!(once.ends_with(&block()));
        assert_eq!(with_block(&once).unwrap(), once);
        assert_eq!(once.matches(BEGIN).count(), 1);
        assert_eq!(without_block(&once).unwrap(), original);
        assert_eq!(without_block(original).unwrap(), original);
        assert_eq!(with_block("").unwrap(), block());
        assert_eq!(
            with_block("no newline").unwrap(),
            format!("no newline\n{}", block())
        );
        assert!(has_block(&once).unwrap());
        assert!(!has_block(original).unwrap());
    }

    #[test]
    fn unmatched_markers_are_errors_naming_the_line() {
        let cases = [
            (format!("a\n{BEGIN}\nexport KEEP=1\n"), 2),
            (format!("a\nb\n{END}\nc\n"), 3),
            (format!("{END}\n{RC_LINE}\n{BEGIN}\n"), 1),
            (format!("{BEGIN}\n{BEGIN}\n{RC_LINE}\n{END}\n"), 1),
            (format!("{}x\n{BEGIN}\n", block()), 5),
        ];
        for (text, line) in cases {
            let wanted = format!("rc file has an unmatched herdr-revive marker at line {line}");
            for result in [
                has_block(&text).map(|_| ()),
                with_block(&text).map(|_| ()),
                without_block(&text).map(|_| ()),
            ] {
                assert_eq!(result.unwrap_err().to_string(), wanted, "{text:?}");
            }
        }
    }

    #[test]
    fn unmatched_markers_stop_every_mode_and_change_nothing() {
        for text in [
            format!("export A=1\n{BEGIN}\nexport KEEP=1\nalias x=y\n"),
            format!("export A=1\n{END}\nexport KEEP=1\n"),
            format!("{END}\nexport KEEP=1\n{BEGIN}\n"),
        ] {
            let home = tempfile::tempdir().unwrap();
            let path = rc(home.path());
            std::fs::write(&path, &text).unwrap();
            for (write, remove) in [(false, false), (true, false), (false, true), (true, true)] {
                let error = run(home.path(), write, remove).unwrap_err();
                assert!(
                    format!("{error:#}").contains("unmatched herdr-revive marker at line"),
                    "{error:#}"
                );
                assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
            }
            assert!(!backup_path(&path).unwrap().exists());
        }
    }

    #[test]
    fn markers_with_trailing_whitespace_are_recognised() {
        let text = format!("{BEGIN} \t\nOLD\n{END}  \r\nexport AFTER=1\n");
        assert!(has_block(&text).unwrap());
        assert_eq!(without_block(&text).unwrap(), "export AFTER=1\n");
        assert_eq!(
            with_block(&text).unwrap(),
            format!("{}export AFTER=1\n", block())
        );
    }

    #[test]
    fn duplicate_blocks_collapse_into_the_first() {
        let text = format!("a\n{}b\n{}c\n", block(), block());
        assert_eq!(with_block(&text).unwrap(), format!("a\n{}b\nc\n", block()));
        assert_eq!(without_block(&text).unwrap(), "a\nb\nc\n");
    }

    fn rc(home: &std::path::Path) -> std::path::PathBuf {
        rc_file(home)
    }

    #[test]
    fn a_dangling_backup_symlink_stops_the_write() {
        let home = tempfile::tempdir().unwrap();
        let path = rc(home.path());
        std::fs::write(&path, "export A=1\n").unwrap();
        let backup = backup_path(&path).unwrap();
        std::os::unix::fs::symlink(home.path().join("missing"), &backup).unwrap();
        let error = run(home.path(), true, false).unwrap_err();
        assert!(
            format!("{error:#}").contains("is a dangling symlink"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "export A=1\n");

        let kept = home.path().join("kept");
        std::fs::write(&kept, "export OLD=1\n").unwrap();
        std::fs::remove_file(&backup).unwrap();
        std::os::unix::fs::symlink(&kept, &backup).unwrap();
        run(home.path(), true, false).unwrap();
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "export OLD=1\n");
        assert!(has_block(&std::fs::read_to_string(&path).unwrap()).unwrap());
    }

    #[test]
    fn write_backs_up_once_keeps_permissions_and_remove_restores() {
        // Darwin's temp dir is a symlink and install reports resolved paths.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let path = rc(&home);
        std::fs::write(&path, "export A=1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let dry = run(&home, false, false).unwrap();
        assert_eq!(dry["status"], "dry_run");
        assert_eq!(dry["installed"], false);
        assert_eq!(dry["up_to_date"], false);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "export A=1\n");

        let written = run(&home, true, false).unwrap();
        assert_eq!(written["status"], "written");
        assert_eq!(written["installed"], true);
        assert_eq!(written["up_to_date"], true);
        let file_name = path.file_name().unwrap().to_str().unwrap();
        let backup = path.with_file_name(format!("{file_name}.revive-backup"));
        assert_eq!(written["backup"], serde_json::json!(backup));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "export A=1\n");
        assert_eq!(
            std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(has_block(&std::fs::read_to_string(&path).unwrap()).unwrap());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let leftovers: Vec<_> = std::fs::read_dir(&home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 2, "{leftovers:?}");

        std::fs::write(&backup, "untouched").unwrap();
        assert_eq!(run(&home, true, false).unwrap()["status"], "unchanged");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "untouched");
        let dry = run(&home, false, false).unwrap();
        assert_eq!(dry["installed"], true);
        assert_eq!(dry["up_to_date"], true);

        let removed = run(&home, false, true).unwrap();
        assert_eq!(removed["status"], "removed");
        assert_eq!(removed["installed"], false);
        assert_eq!(removed["up_to_date"], false);
        assert_eq!(removed["backup"], serde_json::json!(backup));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "untouched");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "export A=1\n");
        assert_eq!(run(&home, false, true).unwrap()["status"], "unchanged");
    }

    #[test]
    fn remove_without_a_block_leaves_the_bytes_alone() {
        for text in ["export A=1", "export A=1\r\nexport B=2\r\n", "a\n\n\nb"] {
            let home = tempfile::tempdir().unwrap();
            let path = rc(home.path());
            std::fs::write(&path, text).unwrap();
            let removed = run(home.path(), false, true).unwrap();
            assert_eq!(removed["status"], "unchanged");
            assert!(removed["backup"].is_null());
            assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes());
            assert!(!backup_path(&path).unwrap().exists());
        }
    }

    #[test]
    fn write_keeps_the_rest_of_the_file_byte_for_byte() {
        let home = tempfile::tempdir().unwrap();
        let path = rc(home.path());
        std::fs::write(&path, "export A=1").unwrap();
        run(home.path(), true, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("export A=1\n{}", block())
        );
        run(home.path(), false, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "export A=1\n");

        let crlf = "export A=1\r\nexport B=2 \r\n";
        std::fs::write(&path, crlf).unwrap();
        run(home.path(), true, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{crlf}{}", block())
        );
        assert_eq!(
            run(home.path(), true, false).unwrap()["status"],
            "unchanged"
        );
        run(home.path(), false, true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), crlf.as_bytes());
    }

    #[test]
    fn a_stale_block_is_replaced_where_it_stands() {
        let home = tempfile::tempdir().unwrap();
        let path = rc(home.path());
        let before = "export A=1\r\n";
        let after = "export AFTER=1\r\nno newline";
        let stale = format!("{before}{BEGIN}\r\nOLD LINE\r\n{END}\r\n{after}");
        std::fs::write(&path, &stale).unwrap();
        let dry = run(home.path(), false, false).unwrap();
        assert_eq!(dry["installed"], true);
        assert_eq!(dry["up_to_date"], false);

        let written = run(home.path(), true, false).unwrap();
        assert_eq!(written["status"], "written");
        assert_eq!(written["up_to_date"], true);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{before}{}{after}", block())
        );
        assert_eq!(
            std::fs::read_to_string(backup_path(&path).unwrap()).unwrap(),
            stale
        );
        assert_eq!(
            run(home.path(), true, false).unwrap()["status"],
            "unchanged"
        );
    }

    #[test]
    fn a_file_owned_by_someone_else_is_refused() {
        let other = std::path::Path::new("/etc/hosts");
        let euid = nix::unistd::geteuid().as_raw();
        let Ok(meta) = std::fs::metadata(other) else {
            return;
        };
        if euid == 0 || std::os::unix::fs::MetadataExt::uid(&meta) == euid {
            return;
        }
        let before = std::fs::read(other).unwrap();
        let home = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(other, rc(home.path())).unwrap();
        assert_eq!(run(home.path(), false, false).unwrap()["status"], "dry_run");
        let error = run(home.path(), true, false).unwrap_err();
        assert!(
            format!("{error:#}").contains("not the current user"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(other).unwrap(), before);
    }

    #[test]
    fn a_missing_file_in_a_directory_owned_by_someone_else_is_refused() {
        let dir = std::path::Path::new("/etc");
        let euid = nix::unistd::geteuid().as_raw();
        let Ok(meta) = std::fs::metadata(dir) else {
            return;
        };
        if euid == 0 || std::os::unix::fs::MetadataExt::uid(&meta) == euid {
            return;
        }
        let missing = dir.join("herdr-revive-missing-rc-test");
        assert!(!missing.exists());
        let error = writable_mode(&missing).unwrap_err();
        assert!(
            format!("{error:#}").contains("not the current user"),
            "{error:#}"
        );
        assert!(!missing.exists());
    }

    #[test]
    fn symlinked_rc_files_are_edited_in_place_and_missing_ones_created() {
        let home = tempfile::tempdir().unwrap();
        let real = home.path().join("dotfiles").join("bashrc.symlink");
        std::fs::create_dir(home.path().join("dotfiles")).unwrap();
        std::fs::write(&real, "# mine\n").unwrap();
        std::os::unix::fs::symlink(&real, rc(home.path())).unwrap();
        let written = run(home.path(), true, false).unwrap();
        assert_eq!(
            written["target"],
            serde_json::json!(real.canonicalize().unwrap())
        );
        assert!(
            std::fs::symlink_metadata(rc(home.path()))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(has_block(&std::fs::read_to_string(&real).unwrap()).unwrap());
        assert!(real.with_file_name("bashrc.symlink.revive-backup").exists());

        let fresh = tempfile::tempdir().unwrap();
        let created = run(fresh.path(), true, false).unwrap();
        assert_eq!(created["status"], "written");
        assert!(created["backup"].is_null());
        assert_eq!(std::fs::read_to_string(rc(fresh.path())).unwrap(), block());

        let missing = tempfile::tempdir().unwrap();
        let removed = run(missing.path(), false, true).unwrap();
        assert_eq!(removed["status"], "unchanged");
        assert!(std::fs::read_dir(missing.path()).unwrap().next().is_none());

        let dangling = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/nonexistent/bashrc", rc(dangling.path())).unwrap();
        assert!(run(dangling.path(), true, false).is_err());
    }
}
