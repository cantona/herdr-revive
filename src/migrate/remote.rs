//! `--machine M`: what M must have, and the parts of a migration that run
//! on M through `hc exec`. The TUI capture for a copy into the app runs on
//! M; a copy into M's TUI runs entirely on M, where its core and TUI are.

use super::external;
use super::ledger::{
    self, AppCreated, AppSource, Entry, Ledger, Same, Seen, WorkspaceRecord, live_of,
};
use super::tui::Export;
use super::{Report, Request, Status, Summary, TuiArgs, drive, to_tui};
use crate::model::{SCHEMA, native_tool};
use anyhow::{Context, Result, bail, ensure};
use std::io::{BufRead, Write};
use std::path::Path;

pub const MIN_VERSION: (u32, u32, u32) = (0, 3, 0);
const INSTALL: &str = "cargo install --locked --git https://github.com/cantona/herdr-revive";
/// `hc exec --timeout` for the checks, the export and the plan.
const READ_SECS: u64 = 120;
/// `hc exec --timeout` for M's `--to tui --yes`.
const APPLY_SECS: u64 = 600;

/// Why a program on M failed, without the `herdr-revive: ` (or anyhow's
/// `Error: `) that its `main` puts in front and this run's `main` adds again.
/// `main` prints its error last, after any `herdr-revive: warning:` lines
/// about single panes, so the reason is the last other `herdr-revive: `
/// line and what follows it (the dialog's `summary` takes the same line).
/// A program that printed nothing, such as an `hc` that was killed, is
/// described by how it ended.
fn failure(ran: &external::Ran) -> String {
    const PREFIX: &str = "herdr-revive: ";
    let text = ran.stderr_text();
    let mut offset = 0;
    let mut start = None;
    for line in text.split_inclusive('\n') {
        if line.starts_with(PREFIX) && !line.starts_with("herdr-revive: warning:") {
            start = Some(offset + PREFIX.len());
        }
        offset += line.len();
    }
    let reason = match start {
        Some(start) => text[start..].trim(),
        None => text.strip_prefix("Error: ").unwrap_or(&text),
    };
    if reason.is_empty() {
        ran.status.to_string()
    } else {
        reason.to_string()
    }
}

/// M's own reason, with the recovery commands it names marked as M's.
fn failure_on(machine: &str, ran: &external::Ran) -> String {
    super::to_tui::on_machine(&failure(ran), machine)
}

pub fn check(machine: &str) -> Result<()> {
    let machines = external::machines()?;
    let found = machines
        .iter()
        .find(|m| m.name == machine)
        .with_context(|| format!("no machine named {machine} in `hc machine ls`"))?;
    ensure!(
        found.state == "connected",
        "machine {machine} is {}, not connected",
        found.state
    );
    let ran = external::hc_exec(machine, "herdr-revive", &["--version".into()], READ_SECS)?;
    let reported = String::from_utf8_lossy(&ran.stdout).trim().to_string();
    let version = if ran.status.success() {
        external::parse_version(&reported)
    } else {
        None
    };
    ensure!(
        version.is_some_and(|v| v >= MIN_VERSION),
        "install herdr-revive {}.{}.{} or newer on {machine} ({INSTALL}); it has: {}",
        MIN_VERSION.0,
        MIN_VERSION.1,
        MIN_VERSION.2,
        if ran.status.success() {
            reported
        } else {
            failure(&ran)
        }
    );
    let ran = external::hc_exec(machine, "herdr", &["--version".into()], READ_SECS)?;
    ensure!(
        ran.status.success(),
        "install the herdr TUI on {machine}: herdr --version failed there: {}",
        failure(&ran)
    );
    Ok(())
}

/// The TUI flags that name paths here mean nothing on M, and dropping them
/// would copy into or from a session the user did not name.
pub fn refuse_local_paths(machine: &str, tui: &TuiArgs) -> Result<()> {
    let local = [
        ("--socket", tui.socket.is_some()),
        ("--config-dir", tui.config_dir.is_some()),
        ("--state-dir", tui.state_dir.is_some()),
        ("--herdr-bin", tui.herdr_bin.is_some()),
    ];
    for (flag, given) in local {
        ensure!(
            !given,
            "{flag} names a path on this machine, so it cannot be used with --machine: \
             {machine} uses its own defaults (--session and --only are passed on)"
        );
    }
    Ok(())
}

fn with_selection(mut args: Vec<String>, tui: &TuiArgs, only: &[String]) -> Vec<String> {
    if let Some(session) = &tui.session {
        args.extend(["--session".to_string(), session.clone()]);
    }
    for id in only {
        args.extend(["--only".to_string(), id.clone()]);
    }
    args
}

pub fn export_args(tui: &TuiArgs, only: &[String]) -> Vec<String> {
    with_selection(
        vec!["migrate".into(), "--export".into(), "tui".into()],
        tui,
        only,
    )
}

pub fn to_tui_args(tui: &TuiArgs, only: &[String]) -> Vec<String> {
    with_selection(
        vec![
            "migrate".into(),
            "--to".into(),
            "tui".into(),
            "--json".into(),
        ],
        tui,
        only,
    )
}

pub fn export_tui(machine: &str, tui: &TuiArgs, only: &[String]) -> Result<Export> {
    refuse_local_paths(machine, tui)?;
    check(machine)?;
    let ran = external::hc_exec(machine, "herdr-revive", &export_args(tui, only), READ_SECS)?;
    ensure!(
        ran.status.success(),
        "reading the TUI session on {machine} failed: {}",
        failure(&ran)
    );
    let export: Export = serde_json::from_slice(&ran.stdout)
        .with_context(|| format!("herdr-revive on {machine} printed no export"))?;
    ensure!(
        native_tool(&export.tool) && export.schema == SCHEMA,
        "herdr-revive on {machine} printed an export of another schema"
    );
    export.snapshot.validate()?;
    Ok(export)
}

/// The entry M keeps for a copy into its app that this machine made with
/// `--machine M`: the one M would have written had it made the copy itself,
/// with no machine and M's own ids (`herdr-app` here prefixes them
/// `<machine>/`; M's core document and M's `herdr-app ls` do not).
pub fn entry_for_machine(
    machine: &str,
    source: &AppSource,
    created: &AppCreated,
    at: u64,
) -> Entry {
    let prefix = format!("{machine}/");
    let bare = |id: &String| id.strip_prefix(&prefix).unwrap_or(id).to_string();
    Entry::ToApp {
        source: AppSource {
            machine: None,
            ..source.clone()
        },
        created: AppCreated {
            space: created.space.as_ref().map(bare),
            terminals: created.terminals.iter().map(bare).collect(),
            unknown: created.unknown,
        },
        at,
    }
}

/// The hidden `migrate --record` that stores `entry` in M's ledger, as a
/// copy that stopped part way with `partial`.
pub fn record_args(entry: &Entry, partial: bool) -> Result<Vec<String>> {
    let mut args = vec![
        "migrate".into(),
        "--record".into(),
        serde_json::to_string(entry)?,
    ];
    if partial {
        args.push("--partial".into());
    }
    Ok(args)
}

/// Records on M a copy into M's app made from here, whole or partial, so a
/// migration started on M, or from a third machine, knows it.
pub fn record_on(machine: &str, entry: &Entry, partial: bool) -> Result<()> {
    let args = record_args(entry, partial)?;
    let ran = external::hc_exec(machine, "herdr-revive", &args, READ_SECS)?;
    ensure!(ran.status.success(), "{}", failure(&ran));
    Ok(())
}

/// `migrate --record ENTRY [--partial]` on M: stores what another machine's
/// `--to app --machine M` copied into this machine's app, in the ledger in
/// `dir`, where M's own plans read it as a local copy (a partial one keeps
/// its space from being copied back and is named in the next plan).
pub fn record_here(json: &str, partial: bool, dir: &Path) -> Result<()> {
    let entry: Entry = serde_json::from_str(json).context("--record takes a ledger entry")?;
    ensure!(
        matches!(&entry, Entry::ToApp { source, created, .. }
            if source.machine.is_none() && (partial || created.space.is_some())),
        "--record takes a copy into this machine's app"
    );
    // As on the machine that made it: a partial copy that made nothing is
    // not kept (a timed-out one may have made a space, so it is).
    if partial
        && matches!(&entry, Entry::ToApp { created, .. }
            if ledger::names_nothing(created) && !created.unknown)
    {
        return Ok(());
    }
    let mut ledger = Ledger::open(dir)?;
    if partial {
        return ledger.record_partial(entry);
    }
    // The whole copy replaces the record of it in progress (same source,
    // same `at`) that the machine copying here made after `space.new`.
    let pending = match &entry {
        Entry::ToApp { source, at, .. } => Some((source.clone(), *at)),
        Entry::ToTui { .. } => None,
    };
    ledger.record(entry)?;
    if let Some((source, at)) = pending {
        ledger.forget_partial(&source, at)?;
    }
    Ok(())
}

/// `step` is "plan" (from `--dry-run`) or "result" (from `--yes`).
fn parse_report(machine: &str, step: &str, stdout: &[u8]) -> Result<Report> {
    let mut report: Report = serde_json::from_slice(stdout)
        .with_context(|| format!("herdr-revive on {machine} printed no readable {step}"))?;
    report.machine = Some(machine.into());
    for line in &mut report.lines {
        line.machine = Some(machine.into());
    }
    Ok(report)
}

fn plan_remote(machine: &str, args: &[String]) -> Result<Report> {
    let ran = external::hc_exec(machine, "herdr-revive", args, READ_SECS)?;
    ensure!(
        ran.status.success(),
        "migrate on {machine} failed: {}",
        failure_on(machine, &ran)
    );
    parse_report(machine, "plan", &ran.stdout)
}

/// M's `--yes` plans again, so its result repeats the plan's warnings; only
/// new ones are added.
fn merge_warnings(report: &mut Report, warnings: Vec<String>) {
    for warning in warnings {
        if !report.warnings.contains(&warning) {
            report.warnings.push(warning);
        }
    }
}

/// When M's result cannot be read, `applied` becomes `None`: the plan's
/// empty list would claim that nothing was copied.
fn apply_remote(machine: &str, args: &[String], report: &mut Report) -> Result<()> {
    let unknown = format!(
        "so what {machine} copied is unknown: check the TUI and the migrate ledger on {machine}"
    );
    let ran = match external::hc_exec(machine, "herdr-revive", args, APPLY_SECS) {
        Ok(ran) => ran,
        Err(error) => {
            report.applied = None;
            return Err(error.context(format!(
                "migrate on {machine} did not report back, {unknown}"
            )));
        }
    };
    // M's result holds only the `--only` sources, so this run's plan keeps
    // its lines (the skipped ones included) and takes M's `applied`.
    let parsed = parse_report(machine, "result", &ran.stdout);
    if ran.status.success() {
        match parsed {
            Ok(done) => {
                report.applied = done.applied;
                merge_warnings(report, done.warnings);
            }
            Err(error) => {
                report.applied = None;
                return Err(error.context(format!(
                    "migrate on {machine} finished but its result could not be read, {unknown}"
                )));
            }
        }
        return Ok(());
    }
    // M prints its report before it exits on a failed source; its `applied`
    // list is what the user has to know about to clean up.
    match parsed {
        Ok(partial) => {
            report.applied = partial.applied;
            merge_warnings(report, partial.warnings);
            bail!("migrate on {machine} failed: {}", failure_on(machine, &ran))
        }
        Err(_) => {
            report.applied = None;
            bail!(
                "migrate on {machine} failed and its result could not be read, {unknown}: {}",
                failure_on(machine, &ran)
            )
        }
    }
}

/// A space this machine's `--to app --machine M` made from one of M's TUI
/// workspaces is recorded only here, so M plans it as a `create`. While that
/// workspace still exists in the session M copies into, it is a `skip`. The
/// ledger here holds the space with its `M/` prefix; M's plan uses the id
/// in M's own document, without it. A workspace under that id with neither
/// a recorded terminal nor the recorded label may be another one that took
/// the id (or M is too old to list them), so the line stays a `create` and
/// names the earlier copy. An M too old to report its TUI session gives nothing to
/// compare with, so its plan stays as it is.
pub fn skip_app_copies(report: &mut Report, machine: &str, entries: &[Entry]) {
    let Some(tui) = report.tui_session.clone() else {
        return;
    };
    let prefix = format!("{machine}/");
    let lives = tui.lives();
    // This machine's copies from M's TUI session, matched to M's live
    // workspaces by id, or after M's TUI renumbered them by a terminal or a
    // label only one has (`ledger::live_of`).
    let records: Vec<(&AppSource, &AppCreated, u64)> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::ToApp {
                source,
                created,
                at,
            } if source.machine.as_deref() == Some(machine) && source.session == tui.session => {
                Some((source, created, *at))
            }
            _ => None,
        })
        .collect();
    // Only the copies whose space M's app still has count: a closed one
    // neither matches nor makes a label ambiguous.
    let listed = |created: &AppCreated| {
        tui.spaces.as_ref().is_none_or(|spaces| {
            created.space.as_deref().is_some_and(|space| {
                let space = space.strip_prefix(&prefix).unwrap_or(space);
                spaces.iter().any(|id| id == space)
            })
        })
    };
    let pool: Vec<(&str, Seen)> = records
        .iter()
        .filter(|(_, created, _)| listed(created))
        .map(|(source, ..)| (source.workspace_id.as_str(), source.seen()))
        .collect();
    for line in &mut report.lines {
        if line.status != Status::Create {
            continue;
        }
        let origin = records.iter().find_map(|(source, created, at)| {
            let space = created.space.as_deref()?;
            if space.strip_prefix(&prefix).unwrap_or(space) != line.source {
                return None;
            }
            let record = WorkspaceRecord {
                value: (),
                id: &source.workspace_id,
                seen: source.seen(),
                at: *at,
            };
            live_of(&record, &pool, &lives)
                .map(|(workspace, same, was)| (workspace, same, *at, was))
        });
        match origin {
            // M's own plan may have named the same earlier copy already.
            Some((workspace, Same::No, at, _)) => {
                let note = to_tui::origin_note(Some(at), &workspace);
                if !line.notes.contains(&note) {
                    line.notes.push(note);
                }
            }
            Some((workspace, same, at, was)) => {
                *line = line
                    .clone()
                    .skip(&format!("copy of TUI workspace {workspace}"));
                // Every note M's `--to tui` plan gives a `create` line is
                // about the copy it would make: an earlier copy or origin
                // under the id ("this copies it back"), a missing folder,
                // agents that keep running. None is true of a skip.
                line.notes.clear();
                let note = match &was {
                    Some(was) => Some(to_tui::moved_origin_note(same, was, &workspace)),
                    None if same == Same::Label => {
                        Some(to_tui::label_origin_note(Some(at), &workspace))
                    }
                    None => None,
                };
                if let Some(note) = note.filter(|note| !line.notes.contains(note)) {
                    line.notes.push(note);
                }
            }
            None => {}
        }
    }
    report.summary = Summary::of(report.to, &report.lines);
}

/// The sources M is asked to copy: the plan's `create` lines, within the
/// user's `--only` when there is one.
pub fn apply_only(report: &Report, only: &[String]) -> Vec<String> {
    report
        .lines
        .iter()
        .filter(|line| line.status == Status::Create)
        .map(|line| line.source.clone())
        .filter(|source| only.is_empty() || only.contains(source))
        .collect()
}

pub fn run_to_tui(
    machine: &str,
    request: &Request,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    errors: &mut dyn Write,
    tty: bool,
) -> Result<i32> {
    refuse_local_paths(machine, &request.tui)?;
    // Held for the whole run, as every migration here holds it: a `--to app
    // --machine M` started here records its copy here only once it is built,
    // so the two must not overlap. `skip_app_copies` reads under it.
    let dir = ledger::dir()?;
    let _held = Ledger::open(&dir)?;
    check(machine)?;
    let mut planning = to_tui_args(&request.tui, &request.only);
    planning.push("--dry-run".into());
    let mut report = plan_remote(machine, &planning)?;
    skip_app_copies(&mut report, machine, &ledger::read(&dir)?);
    drive(
        report,
        request.confirm,
        request.json,
        input,
        output,
        errors,
        tty,
        |report| {
            // M plans again on `--yes`, so only the sources shown as `create`
            // here are named; without any, M would copy every space.
            let only = apply_only(report, &request.only);
            ensure!(
                !only.is_empty(),
                "the plan from {machine} has no space to copy within --only"
            );
            let mut applying = to_tui_args(&request.tui, &only);
            applying.push("--yes".into());
            apply_remote(machine, &applying, report)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::setup::Confirm;
    use crate::migrate::{Mode, Side};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    // The stand-ins read their answers from files in $STAND_IN, so a
    // scenario can change them between calls.
    const HC: &str = "#!/bin/sh\n\
        echo \"$*\" >> \"$STAND_IN/hc.log\"\n\
        if [ \"$1\" = machine ]; then cat \"$STAND_IN/machines.json\"; exit 0; fi\n\
        case \"$*\" in *--yes) case \"$(cat \"$STAND_IN/hc.mode\" 2>/dev/null)\" in\n\
        kill) kill -9 $$ ;;\n\
        flood) exec cat /dev/zero ;;\n\
        esac ;; esac\n\
        while [ \"$1\" != -- ]; do shift; done\n\
        shift\n\
        exec \"$@\"\n";
    const HERDR: &str = "#!/bin/sh\necho herdr 0.9.5\n";
    const REVIVE: &str = "#!/bin/sh\n\
        case \"$*\" in\n\
        --version) cat \"$STAND_IN/version\" ;;\n\
        *--dry-run) cat \"$STAND_IN/plan.json\" ;;\n\
        *--yes) cat \"$STAND_IN/result.out\"; cat \"$STAND_IN/result.err\" >&2;\n\
        exit \"$(cat \"$STAND_IN/result.code\")\" ;;\n\
        *) exit 9 ;;\n\
        esac\n";

    fn script(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Runs one `#[ignore]`d scenario in a child test process whose PATH
    /// holds only the stand-ins, `sh` and `cat`, and whose HOME is empty, so
    /// this process's environment stays untouched while other tests run and
    /// no real `hc`, `herdr` or `herdr-revive` is ever reached.
    fn in_child(scenario: &str) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let tools = dir.path().join("tools");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        script(&bin.join("hc"), HC);
        script(&bin.join("herdr"), HERDR);
        script(&bin.join("herdr-revive"), REVIVE);
        for tool in ["sh", "cat"] {
            let found = ["/usr/bin", "/bin"]
                .iter()
                .map(|dir| Path::new(dir).join(tool))
                .find(|path| path.exists())
                .unwrap();
            std::os::unix::fs::symlink(found, tools.join(tool)).unwrap();
        }
        let ran = Command::new(std::env::current_exe().unwrap())
            .args([
                &format!("migrate::remote::tests::{scenario}"),
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env("PATH", format!("{}:{}", bin.display(), tools.display()))
            .env("HOME", dir.path())
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env("STAND_IN", dir.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&ran.stdout);
        assert!(
            ran.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&ran.stderr)
        );
    }

    /// The stand-in directory inside `in_child`; `None` in a plain
    /// `--ignored` run, where the scenario does nothing.
    fn stand_in() -> Option<PathBuf> {
        let dir = PathBuf::from(std::env::var_os("STAND_IN")?);
        put(&dir, "machines.json", &machines("connected"));
        put(&dir, "version", "herdr-revive 0.3.0\n");
        put(&dir, "plan.json", &plan().to_string());
        Some(dir)
    }

    fn put(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(name), text).unwrap();
    }

    fn machines(state: &str) -> String {
        serde_json::json!({"machines": [{"host": "canix", "name": "canix", "state": state,
            "temporary": false}]})
        .to_string()
    }

    fn plan() -> serde_json::Value {
        let line = serde_json::json!({"machine": null, "source": "s1", "status": "create",
            "reason": null, "name": "api", "tabs": 1, "panes": 1, "agents": 0, "denied": 0,
            "mod_blocks": 0, "live_agents": [], "copy": null, "partial": null});
        serde_json::json!({"to": "tui", "machine": null, "lines": [line, line],
            "summary": {"create": 2, "skip": 0, "unchanged": 0, "ratios_lost": false},
            "applied": []})
    }

    fn result(dir: &Path, out: &str, err: &str, code: i32) {
        put(dir, "result.out", out);
        put(dir, "result.err", err);
        put(dir, "result.code", &code.to_string());
    }

    fn request(json: bool, tui: TuiArgs) -> Request {
        Request {
            mode: Mode::To(Side::Tui),
            machine: Some("canix".into()),
            only: Vec::new(),
            confirm: Confirm::Yes,
            json,
            tui,
        }
    }

    /// Runs `run_to_tui` and returns its error (`{:#}`) and its output.
    fn to_tui_fails(json: bool, tui: TuiArgs) -> (String, String) {
        let mut input = "".as_bytes();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let error = run_to_tui(
            "canix",
            &request(json, tui),
            &mut input,
            &mut output,
            &mut errors,
            false,
        )
        .unwrap_err();
        (format!("{error:#}"), String::from_utf8(output).unwrap())
    }

    fn message(result: Result<()>) -> String {
        format!("{:#}", result.unwrap_err())
    }

    #[test]
    fn a_remote_reason_is_ms_own_error_not_a_warning_printed_before_it() {
        use std::os::unix::process::ExitStatusExt;
        let ran = |stderr: &str, code: i32| external::Ran {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        };
        let warned = "herdr-revive: warning: pane a:pt7: foreground process 4242 is \
                      protected; saved with no command in the pane's cwd\n\
                      herdr-revive: warning: pane a:pt8: no shell\n\
                      herdr-revive: copy of sp1 stopped: no space sp1\n";
        assert_eq!(
            failure(&ran(warned, 1)),
            "copy of sp1 stopped: no space sp1"
        );
        assert_eq!(
            failure(&ran("herdr-revive: two\nlines\n", 1)),
            "two\nlines",
            "the rest of the error stays with it"
        );
        assert_eq!(failure(&ran("Error: older main\n", 1)), "older main");
        assert_eq!(
            failure(&ran("herdr-revive: warning: only\n", 1)),
            "herdr-revive: warning: only",
            "no error line: the text as it came"
        );
        assert_eq!(failure(&ran("", 9)), "exit status: 9");
    }

    #[test]
    fn a_copy_into_a_machines_app_is_recorded_there_with_its_own_ids() {
        let source = AppSource {
            machine: Some("canix".into()),
            session: "a".repeat(64),
            workspace_id: "w1".into(),
            terminal_ids: Some(vec!["term_1".into()]),
            label: Some("work".into()),
        };
        let created = AppCreated {
            space: Some("canix/sp4".into()),
            terminals: vec!["canix/t7".into(), "t8".into()],
            unknown: false,
        };
        let entry = entry_for_machine("canix", &source, &created, 9);
        let Entry::ToApp {
            source: theirs,
            created: made,
            at,
        } = &entry
        else {
            panic!("{entry:?}")
        };
        assert_eq!(theirs.machine, None);
        assert_eq!(
            (theirs.session.as_str(), theirs.workspace_id.as_str()),
            (source.session.as_str(), "w1")
        );
        assert_eq!(made.space.as_deref(), Some("sp4"));
        assert_eq!(made.terminals, ["t7", "t8"]);
        assert_eq!(*at, 9);
        assert_eq!(
            record_args(&entry, false).unwrap()[..2],
            ["migrate", "--record"]
        );
        assert_eq!(record_args(&entry, true).unwrap()[3], "--partial");

        let root = tempfile::tempdir().unwrap();
        record_here(&record_args(&entry, false).unwrap()[2], false, root.path()).unwrap();
        let ledger = Ledger::open(root.path()).unwrap();
        let mut here = source.clone();
        here.machine = None;
        let found = ledger.copied_to_app(&here).unwrap();
        assert_eq!(found.created.space.as_deref(), Some("sp4"));
        assert_eq!(ledger.app_space_origin("sp4").map(|r| r.at), Some(9));
        drop(ledger);

        let far = entry_for_machine("canix", &source, &created, 9);
        let mut far = serde_json::to_value(&far).unwrap();
        far["source"]["machine"] = "mac".into();
        let mut no_space = serde_json::to_value(&entry).unwrap();
        no_space["created"]["space"] = serde_json::Value::Null;
        let to_tui = serde_json::json!({"direction": "to_tui", "source": {"space": "s1"},
            "created": {"session": "b", "workspace_id": "w1"}, "at": 1});
        for bad in [far, no_space, to_tui] {
            let error = record_here(&bad.to_string(), false, root.path()).unwrap_err();
            assert!(
                error.to_string().contains("a copy into this machine's app"),
                "{error:#}"
            );
        }
        let error = record_here("{", false, root.path()).unwrap_err();
        assert!(error.to_string().contains("takes a ledger entry"));
        // A partial copy may have no space yet and keeps its timed-out mark.
        let mut partial = created.clone();
        partial.space = None;
        partial.unknown = true;
        let entry = entry_for_machine("canix", &source, &partial, 10);
        let other = tempfile::tempdir().unwrap();
        record_here(&serde_json::to_string(&entry).unwrap(), true, other.path()).unwrap();
        let ledger = Ledger::open(other.path()).unwrap();
        assert_eq!(ledger.copied_to_app(&here), None);
        let found = ledger.partial_to_app(&here).unwrap();
        assert!(found.created.unknown);
        assert_eq!(found.created.terminals, ["t7", "t8"]);
        drop(ledger);
        // A partial copy that made nothing is not kept on M either; a second
        // one is kept beside the first.
        let nothing = AppCreated::default();
        let entry = entry_for_machine("canix", &source, &nothing, 11);
        record_here(&serde_json::to_string(&entry).unwrap(), true, other.path()).unwrap();
        let mut second = created.clone();
        second.space = Some("canix/sp5".into());
        let entry = entry_for_machine("canix", &source, &second, 12);
        record_here(&serde_json::to_string(&entry).unwrap(), true, other.path()).unwrap();
        let ledger = Ledger::open(other.path()).unwrap();
        let ats: Vec<u64> = ledger.partials_to_app(&here).iter().map(|p| p.at).collect();
        assert_eq!(ats, [10, 12]);
    }

    #[test]
    fn the_machines_result_adds_only_its_new_warnings() {
        let mut report = Report::new(super::super::Side::Tui, Some("canix"), vec![]);
        report.warnings = vec!["capture".into()];
        merge_warnings(&mut report, vec!["capture".into(), "not listed".into()]);
        assert_eq!(report.warnings, ["capture", "not listed"]);
    }

    #[test]
    fn check_names_what_to_install_on_the_machine() {
        in_child("check_on_stand_ins");
    }

    #[test]
    #[ignore = "a scenario of check_names_what_to_install_on_the_machine"]
    fn check_on_stand_ins() {
        let Some(dir) = stand_in() else { return };
        check("canix").unwrap();
        let log = std::fs::read_to_string(dir.join("hc.log")).unwrap();
        assert!(log.contains("exec --on canix --timeout 120 -- sh -c"));
        assert!(!log.contains("--timeout 30 "));
        assert_eq!(
            message(check("nope")),
            "no machine named nope in `hc machine ls`"
        );
        put(&dir, "machines.json", &machines("disconnected"));
        assert_eq!(
            message(check("canix")),
            "machine canix is disconnected, not connected"
        );
        put(&dir, "machines.json", &machines("connected"));
        put(&dir, "version", "herdr-revive 0.2.1\n");
        assert_eq!(
            message(check("canix")),
            "install herdr-revive 0.3.0 or newer on canix (cargo install --locked --git \
             https://github.com/cantona/herdr-revive); it has: herdr-revive 0.2.1"
        );
        put(&dir, "version", "herdr-revive 0.3.0\n");
        let revive = dir.join("bin/herdr-revive");
        std::fs::remove_file(&revive).unwrap();
        assert_eq!(
            message(check("canix")),
            "install herdr-revive 0.3.0 or newer on canix (cargo install --locked --git \
             https://github.com/cantona/herdr-revive); it has: cannot run herdr-revive: not on \
             PATH, in ~/.local/bin or in ~/.cargo/bin"
        );
        script(&revive, REVIVE);
        std::fs::remove_file(dir.join("bin/herdr")).unwrap();
        assert_eq!(
            message(check("canix")),
            "install the herdr TUI on canix: herdr --version failed there: cannot run herdr: \
             not on PATH, in ~/.local/bin or in ~/.cargo/bin"
        );
    }

    #[test]
    fn a_copy_into_a_machines_tui_waits_for_a_migration_running_here() {
        in_child("held_lock_on_stand_ins");
    }

    #[test]
    #[ignore = "a scenario of a_copy_into_a_machines_tui_waits_for_a_migration_running_here"]
    fn held_lock_on_stand_ins() {
        let Some(dir) = stand_in() else { return };
        // Another migration on this machine (say a `--to app --machine canix`
        // still building) holds the ledger.
        let held = Ledger::open(&ledger::dir().unwrap()).unwrap();
        let (error, _) = to_tui_fails(false, TuiArgs::default());
        assert_eq!(error, "another migration is running");
        let log = std::fs::read_to_string(dir.join("hc.log")).unwrap_or_default();
        assert_eq!(log, "", "nothing on canix is asked before the lock");
        drop(held);
        let (report, _) = to_tui_ok(&dir, Confirm::DryRun, &[]);
        assert_eq!(report.summary.create, 2);
    }

    #[test]
    fn local_paths_are_refused_with_a_machine() {
        in_child("local_paths_on_stand_ins");
    }

    #[test]
    #[ignore = "a scenario of local_paths_are_refused_with_a_machine"]
    fn local_paths_on_stand_ins() {
        let Some(dir) = stand_in() else { return };
        let path = || Some(PathBuf::from("/x"));
        let cases = [
            (
                "--socket",
                TuiArgs {
                    socket: path(),
                    ..TuiArgs::default()
                },
            ),
            (
                "--config-dir",
                TuiArgs {
                    config_dir: path(),
                    ..TuiArgs::default()
                },
            ),
            (
                "--state-dir",
                TuiArgs {
                    state_dir: path(),
                    ..TuiArgs::default()
                },
            ),
            (
                "--herdr-bin",
                TuiArgs {
                    herdr_bin: path(),
                    session: Some("com".into()),
                    ..TuiArgs::default()
                },
            ),
        ];
        for (flag, tui) in cases {
            let expected = format!(
                "{flag} names a path on this machine, so it cannot be used with --machine: \
                 canix uses its own defaults (--session and --only are passed on)"
            );
            let export = export_tui("canix", &tui, &[]).map(|_| ());
            assert_eq!(message(export), expected);
            assert_eq!(to_tui_fails(false, tui).0, expected);
        }
        assert!(!dir.join("hc.log").exists());
    }

    #[test]
    fn a_failed_remote_apply_keeps_what_the_machine_copied() {
        in_child("failed_apply_on_stand_ins");
    }

    #[test]
    #[ignore = "a scenario of a_failed_remote_apply_keeps_what_the_machine_copied"]
    fn failed_apply_on_stand_ins() {
        let Some(dir) = stand_in() else { return };
        let mut applied = plan();
        applied["applied"] =
            serde_json::json!([{"source": "s1", "created": {"workspace_id": "w5"}}]);
        result(
            &dir,
            &applied.to_string(),
            "herdr-revive: rebuilding s2 failed\n",
            1,
        );
        let (error, output) = to_tui_fails(true, TuiArgs::default());
        assert_eq!(error, "migrate on canix failed: rebuilding s2 failed");
        let printed: Report = serde_json::from_str(&output).unwrap();
        let applied = printed.applied.unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].source, "s1");
        assert_eq!(applied[0].created["workspace_id"], "w5");
        assert_eq!(printed.machine.as_deref(), Some("canix"));
        let log = std::fs::read_to_string(dir.join("hc.log")).unwrap();
        assert!(log.contains("--timeout 600 -- sh -c"));

        let (_, text) = to_tui_fails(false, TuiArgs::default());
        assert!(text.ends_with("  copied     s1 -> TUI workspace w5\n"));
    }

    #[test]
    fn an_unreadable_result_says_what_the_machine_copied_is_unknown() {
        in_child("unreadable_on_stand_ins");
    }

    #[test]
    #[ignore = "a scenario of an_unreadable_result_says_what_the_machine_copied_is_unknown"]
    fn unreadable_on_stand_ins() {
        let Some(dir) = stand_in() else { return };
        result(
            &dir,
            "{\"to\": \"tu",
            "herdr-revive: rebuilding s2 failed\n",
            1,
        );
        let (error, output) = to_tui_fails(true, TuiArgs::default());
        assert_eq!(
            error,
            "migrate on canix failed and its result could not be read, so what canix copied \
             is unknown: check the TUI and the migrate ledger on canix: rebuilding s2 \
             failed"
        );
        let printed: Report = serde_json::from_str(&output).unwrap();
        assert_eq!(printed.applied, None, "not an empty list: unknown");

        result(&dir, "garbled", "", 0);
        let (error, output) = to_tui_fails(true, TuiArgs::default());
        let printed: Report = serde_json::from_str(&output).unwrap();
        assert_eq!(printed.applied, None);
        assert!(
            error.starts_with(
                "migrate on canix finished but its result could not be read, so what canix \
                 copied is unknown: check the TUI and the migrate ledger on canix: \
                 herdr-revive on canix printed no readable result: "
            ),
            "{error}"
        );

        put(&dir, "plan.json", "garbled");
        let (error, output) = to_tui_fails(true, TuiArgs::default());
        assert!(
            error.starts_with("herdr-revive on canix printed no readable plan: "),
            "{error}"
        );
        assert!(output.is_empty());
    }

    #[test]
    fn a_lost_hc_says_what_the_machine_copied_is_unknown() {
        in_child("lost_hc_on_stand_ins");
    }

    #[test]
    #[ignore = "a scenario of a_lost_hc_says_what_the_machine_copied_is_unknown"]
    fn lost_hc_on_stand_ins() {
        let Some(dir) = stand_in() else { return };
        result(&dir, &plan().to_string(), "", 0);
        put(&dir, "hc.mode", "kill");
        let (error, output) = to_tui_fails(true, TuiArgs::default());
        assert!(
            error.starts_with(
                "migrate on canix failed and its result could not be read, so what canix \
                 copied is unknown: check the TUI and the migrate ledger on canix: signal: 9"
            ),
            "{error}"
        );
        let printed: Report = serde_json::from_str(&output).unwrap();
        assert_eq!(printed.applied, None);

        // Past the output cap `run` gives up on hc the way it does at the
        // deadline: hc_exec returns an error and M's answer never arrives.
        put(&dir, "hc.mode", "flood");
        let (error, _) = to_tui_fails(true, TuiArgs::default());
        assert_eq!(
            error,
            "migrate on canix did not report back, so what canix copied is unknown: check the \
             TUI and the migrate ledger on canix: hc stdout: output exceeds size limit"
        );
    }

    #[test]
    fn recovery_commands_from_the_machine_are_marked_as_its_own() {
        in_child("recovery_on_stand_ins");
    }

    #[test]
    #[ignore = "a scenario of recovery_commands_from_the_machine_are_marked_as_its_own"]
    fn recovery_on_stand_ins() {
        let Some(dir) = stand_in() else { return };
        let recovery = "TUI saves and restores are paused until the recovery evidence is \
             acknowledged: run `herdr-revive --host herdr recovery inspect`, then \
             `herdr-revive --host herdr recovery acknowledge OP`";
        result(
            &dir,
            &plan().to_string(),
            &format!("herdr-revive: copy of s2 stopped; {recovery}\n"),
            1,
        );
        let (error, _) = to_tui_fails(true, TuiArgs::default());
        assert_eq!(
            error,
            "migrate on canix failed: copy of s2 stopped; TUI saves and restores are paused \
             until the recovery evidence is acknowledged: on canix: run `herdr-revive --host \
             herdr recovery inspect`, then `herdr-revive --host herdr recovery acknowledge OP`"
        );
        let plan_error = format!("herdr-revive: evidence; {recovery}\n");
        let revive = dir.join("bin/herdr-revive");
        script(
            &revive,
            &format!(
                "#!/bin/sh\ncase \"$*\" in --version) cat \"$STAND_IN/version\" ;; \
                 *) printf '%s' '{plan_error}' >&2; exit 1 ;; esac\n"
            ),
        );
        let (error, _) = to_tui_fails(true, TuiArgs::default());
        assert!(
            error.starts_with("migrate on canix failed: evidence; ")
                && error.contains("acknowledged: on canix: run `"),
            "{error}"
        );
    }

    #[test]
    fn the_machine_error_prefix_is_not_repeated() {
        in_child("prefixes_on_stand_ins");
    }

    #[test]
    #[ignore = "a scenario of the_machine_error_prefix_is_not_repeated"]
    fn prefixes_on_stand_ins() {
        let Some(dir) = stand_in() else { return };
        for stderr in [
            "herdr-revive: no workspace s9\n",
            "Error: no workspace s9\n",
            "no workspace s9\n",
        ] {
            result(&dir, &plan().to_string(), stderr, 1);
            let (error, _) = to_tui_fails(false, TuiArgs::default());
            assert_eq!(error, "migrate on canix failed: no workspace s9");
        }
    }

    fn line(source: &str) -> serde_json::Value {
        serde_json::json!({"machine": null, "source": source, "status": "create",
            "reason": null, "name": "", "tabs": 1, "panes": 1, "agents": 0, "denied": 0,
            "mod_blocks": 0, "live_agents": [], "copy": null, "partial": null})
    }

    /// M's plan for spaces `sources`, into a TUI session whose live
    /// workspaces are `workspaces` (`None`: an M that does not report them).
    fn plan_of(sources: &[&str], workspaces: Option<&[&str]>) -> serde_json::Value {
        let lines: Vec<_> = sources.iter().map(|source| line(source)).collect();
        serde_json::json!({"to": "tui", "machine": null, "lines": lines,
            "summary": {"create": sources.len(), "skip": 0, "unchanged": 0,
                "ratios_lost": false},
            "applied": [],
            "tui_session": workspaces.map(|w| serde_json::json!({"session": "a".repeat(64),
                "workspaces": w,
                "terminal_ids": w.iter().map(|id| (id.to_string(), serde_json::json!([format!("term_{id}")])))
                    .collect::<serde_json::Map<_, _>>(),
                "labels": w.iter().map(|id| (id.to_string(), serde_json::json!(format!("[1] label {id}"))))
                    .collect::<serde_json::Map<_, _>>()}))})
    }

    /// This machine's ledger, with one `--to app --machine` copy per
    /// (workspace, space) pair.
    fn local_ledger(dir: &Path, machine: &str, copies: &[(&str, &str)]) {
        let entries: Vec<_> = copies
            .iter()
            .map(|(workspace, space)| {
                serde_json::json!({"direction": "to_app", "source": {"machine": machine,
                    "session": "a".repeat(64), "workspace_id": workspace,
                    "terminal_ids": [format!("term_{workspace}")]},
                    "created": {"space": space, "terminals": []}, "at": 1})
            })
            .collect();
        let state = dir.join("state/herdr-revive");
        std::fs::create_dir_all(&state).unwrap();
        put(
            &state,
            "migrated.json",
            &serde_json::json!({"tool": "herdr-revive", "schema": 1, "entries": entries,
                "partial": []})
            .to_string(),
        );
    }

    /// Runs `run_to_tui` with `confirm`, and returns its JSON report and the
    /// `hc` calls that asked M to apply.
    fn to_tui_ok(dir: &Path, confirm: Confirm, only: &[&str]) -> (Report, Vec<String>) {
        let _ = std::fs::remove_file(dir.join("hc.log"));
        let mut input = "".as_bytes();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let mut request = request(true, TuiArgs::default());
        request.confirm = confirm;
        request.only = only.iter().map(|id| id.to_string()).collect();
        let code = run_to_tui(
            "canix",
            &request,
            &mut input,
            &mut output,
            &mut errors,
            false,
        )
        .unwrap();
        assert_eq!(code, 0);
        let log = std::fs::read_to_string(dir.join("hc.log")).unwrap_or_default();
        let applies = log
            .lines()
            .filter(|call| call.ends_with("--yes"))
            .map(String::from)
            .collect();
        (serde_json::from_slice(&output).unwrap(), applies)
    }

    #[test]
    fn a_space_copied_here_from_the_machines_tui_is_not_copied_back() {
        in_child("copied_back_on_stand_ins");
    }

    #[test]
    #[ignore = "a scenario of a_space_copied_here_from_the_machines_tui_is_not_copied_back"]
    fn copied_back_on_stand_ins() {
        let Some(dir) = stand_in() else { return };
        local_ledger(&dir, "canix", &[("w2", "canix/s1"), ("w3", "other/s2")]);
        put(
            &dir,
            "plan.json",
            &plan_of(&["s1", "s2"], Some(&["w1", "w2", "w3"])).to_string(),
        );
        let (report, applies) = to_tui_ok(&dir, Confirm::DryRun, &[]);
        assert_eq!(report.lines[0].status, Status::Skip);
        assert_eq!(
            report.lines[0].reason.as_deref(),
            Some("copy of TUI workspace w2")
        );
        assert_eq!(report.lines[1].status, Status::Create, "another machine's");
        assert_eq!((report.summary.create, report.summary.skip), (1, 1));
        assert!(applies.is_empty());

        let mut done = plan_of(&["s2"], Some(&["w1", "w2", "w3"]));
        done["applied"] = serde_json::json!([{"source": "s2", "created": {"workspace_id": "w5"}}]);
        result(&dir, &done.to_string(), "", 0);
        let (report, applies) = to_tui_ok(&dir, Confirm::Yes, &[]);
        assert_eq!(applies.len(), 1);
        assert!(
            applies[0].ends_with("migrate --to tui --json --only s2 --yes"),
            "{applies:?}"
        );
        assert_eq!(
            report.lines.len(),
            2,
            "the skipped line stays in the report"
        );
        assert_eq!(report.lines[0].status, Status::Skip);
        assert_eq!(report.applied.unwrap()[0].source, "s2");

        // A user --only is narrowed, never widened by a union with it.
        let (_, applies) = to_tui_ok(&dir, Confirm::Yes, &["s1", "s2"]);
        assert!(
            applies[0].ends_with("--json --only s2 --yes"),
            "{applies:?}"
        );

        local_ledger(&dir, "canix", &[("w2", "canix/s1"), ("w3", "canix/s2")]);
        let (report, applies) = to_tui_ok(&dir, Confirm::Yes, &[]);
        assert_eq!(report.summary.create, 0);
        assert!(applies.is_empty(), "nothing left: M is not asked to apply");
        let mut input = "".as_bytes();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        run_to_tui(
            "canix",
            &request(false, TuiArgs::default()),
            &mut input,
            &mut output,
            &mut errors,
            false,
        )
        .unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .ends_with("nothing to copy\n")
        );

        // Once the TUI workspace is gone, its copy is copied back.
        put(
            &dir,
            "plan.json",
            &plan_of(&["s1", "s2"], Some(&["w1", "w3"])).to_string(),
        );
        let (report, applies) = to_tui_ok(&dir, Confirm::Yes, &[]);
        assert_eq!(report.lines[0].status, Status::Create);
        assert!(applies[0].ends_with("--only s1 --yes"), "{applies:?}");

        // An M that does not name its TUI session keeps its plan.
        put(&dir, "plan.json", &plan_of(&["s1", "s2"], None).to_string());
        let (report, applies) = to_tui_ok(&dir, Confirm::Yes, &[]);
        assert_eq!(report.summary.create, 2);
        assert!(
            applies[0].ends_with("--only s1 --only s2 --yes"),
            "{applies:?}"
        );
    }

    #[test]
    fn only_create_lines_within_the_users_only_are_applied() {
        let mut report: Report =
            serde_json::from_value(plan_of(&["s1", "s2", "s3"], None)).unwrap();
        report.lines[1].status = Status::Unchanged;
        assert_eq!(apply_only(&report, &[]), ["s1", "s3"]);
        assert_eq!(apply_only(&report, &["s3".into(), "s2".into()]), ["s3"]);
        assert!(apply_only(&report, &["s9".into()]).is_empty());
    }

    #[test]
    fn a_copy_is_skipped_only_for_its_machine_session_and_live_workspace() {
        use crate::migrate::ledger::{AppCreated, AppSource};
        let entry = |machine: &str, session: String, workspace: &str, space: &str| Entry::ToApp {
            source: AppSource {
                machine: Some(machine.into()),
                session,
                workspace_id: workspace.into(),
                terminal_ids: Some(vec![format!("term_{workspace}")]),
                label: None,
            },
            created: AppCreated {
                space: Some(space.into()),
                terminals: vec![],
                unknown: false,
            },
            at: 1,
        };
        let entries = [
            entry("canix", "a".repeat(64), "w1", "canix/s1"),
            entry("canix", "a".repeat(64), "w9", "canix/s2"),
            entry("canix", "b".repeat(64), "w1", "canix/s3"),
            entry("mac", "a".repeat(64), "w1", "mac/s4"),
            entry("canix", "a".repeat(64), "w1", "s5"),
        ];
        let mut report: Report = serde_json::from_value(plan_of(
            &["s1", "s2", "s3", "s4", "s5", "s6"],
            Some(&["w1"]),
        ))
        .unwrap();
        report.lines[5].status = Status::Unchanged;
        skip_app_copies(&mut report, "canix", &entries);
        let statuses: Vec<_> = report.lines.iter().map(|line| line.status).collect();
        assert_eq!(
            statuses,
            [
                Status::Skip,
                Status::Create,
                Status::Create,
                Status::Create,
                Status::Skip,
                Status::Unchanged
            ]
        );
        assert_eq!(
            report.lines[0].reason.as_deref(),
            Some("copy of TUI workspace w1")
        );
        assert_eq!(
            (
                report.summary.create,
                report.summary.skip,
                report.summary.unchanged
            ),
            (3, 2, 1)
        );
        assert!(report.lines.iter().all(|line| line.notes.is_empty()));
    }

    #[test]
    fn a_workspace_that_took_the_id_of_a_copied_one_leaves_a_note_not_a_skip() {
        use crate::migrate::ledger::{AppCreated, AppSource};
        let entry = |terminal_ids: Option<Vec<String>>, label: Option<&str>| Entry::ToApp {
            source: AppSource {
                machine: Some("canix".into()),
                session: "a".repeat(64),
                workspace_id: "w1".into(),
                terminal_ids,
                label: label.map(String::from),
            },
            created: AppCreated {
                space: Some("canix/s1".into()),
                terminals: vec![],
                unknown: false,
            },
            at: 1,
        };
        let note = to_tui::origin_note(Some(1), "w1");
        // M's w1 is "[1] label w1" on term_w1.
        let old = || Some(vec!["term_old".to_string()]);
        let plan =
            || -> Report { serde_json::from_value(plan_of(&["s1"], Some(&["w1"]))).unwrap() };
        for (recorded, label) in [(old(), Some("frontend")), (None, None)] {
            let mut report = plan();
            skip_app_copies(&mut report, "canix", &[entry(recorded.clone(), label)]);
            assert_eq!(report.lines[0].status, Status::Create, "{recorded:?}");
            assert_eq!(report.lines[0].notes, std::slice::from_ref(&note));
        }
        // canix's own plan named the same earlier copy (it was recorded there
        // too): the sentence is not added twice.
        let mut report = plan();
        report.lines[0].notes = vec![note.clone()];
        skip_app_copies(&mut report, "canix", &[entry(old(), Some("frontend"))]);
        assert_eq!(report.lines[0].notes, std::slice::from_ref(&note));
        let mut report = plan();
        skip_app_copies(&mut report, "canix", &[entry(old(), Some("label w1"))]);
        assert_eq!(
            report.lines[0].status,
            Status::Skip,
            "restarted: same label, new terminals"
        );
        assert_eq!(
            report.lines[0].notes,
            [to_tui::label_origin_note(Some(1), "w1")],
            "a match by the label alone says so"
        );
        let mut report = plan();
        let left = Some(vec!["term_w1".to_string()]);
        skip_app_copies(&mut report, "canix", &[entry(left, Some("label w1"))]);
        assert_eq!(report.lines[0].status, Status::Skip, "a terminal is left");
        assert!(report.lines[0].notes.is_empty(), "a terminal is left");
        let mut older = plan_of(&["s1"], Some(&["w1"]));
        let session = older["tui_session"].as_object_mut().unwrap();
        session.remove("terminal_ids");
        session.remove("labels");
        let mut report: Report = serde_json::from_value(older).unwrap();
        let recorded = entry(Some(vec!["term_w1".into()]), Some("label w1"));
        skip_app_copies(&mut report, "canix", &[recorded]);
        assert_eq!(
            report.lines[0].status,
            Status::Create,
            "an M that lists no terminals or labels cannot show it is the same"
        );
        assert_eq!(report.lines[0].notes, [note]);
    }

    #[test]
    fn a_copy_from_a_workspace_the_tui_renumbered_is_still_skipped() {
        use crate::migrate::ledger::{AppCreated, AppSource};
        let entry = |workspace: &str| Entry::ToApp {
            source: AppSource {
                machine: Some("canix".into()),
                session: "a".repeat(64),
                workspace_id: workspace.into(),
                terminal_ids: Some(vec!["term_old".into()]),
                label: Some("label w2".into()),
            },
            created: AppCreated {
                space: Some("canix/s1".into()),
                terminals: vec![],
                unknown: false,
            },
            at: 1,
        };
        // M's w1 and w2 are "label w1" and "label w2"; s1 was made from the
        // workspace now w2, when it was w3 or w1.
        for was in ["w3", "w1"] {
            let mut report: Report =
                serde_json::from_value(plan_of(&["s1"], Some(&["w1", "w2"]))).unwrap();
            skip_app_copies(&mut report, "canix", &[entry(was)]);
            assert_eq!(report.lines[0].status, Status::Skip, "{was}");
            assert_eq!(
                report.lines[0].reason.as_deref(),
                Some("copy of TUI workspace w2")
            );
            assert_eq!(
                report.lines[0].notes,
                [to_tui::moved_origin_note(Same::Label, was, "w2")]
            );
        }
    }

    #[test]
    fn a_line_turned_into_a_skip_drops_the_notes_about_the_copy() {
        use crate::migrate::ledger::{AppCreated, AppSource};
        let entry = |terminal: &str| Entry::ToApp {
            source: AppSource {
                machine: Some("canix".into()),
                session: "a".repeat(64),
                workspace_id: "w1".into(),
                terminal_ids: Some(vec![terminal.into()]),
                label: Some("label w1".into()),
            },
            created: AppCreated {
                space: Some("canix/s1".into()),
                terminals: vec![],
                unknown: false,
            },
            at: 1,
        };
        // M has no record of s1 (its `--record` failed), so its plan made
        // s1 a `create` with the notes of a copy.
        let plan = || {
            let mut report: Report =
                serde_json::from_value(plan_of(&["s1"], Some(&["w1"]))).unwrap();
            report.lines[0].notes = vec![
                to_tui::origin_note(Some(1), "w1"),
                crate::migrate::missing_note(&["/gone".to_string()]),
                to_tui::live_agents_note(1),
            ];
            report
        };
        let mut report = plan();
        skip_app_copies(&mut report, "canix", &[entry("term_w1")]);
        assert_eq!(report.lines[0].status, Status::Skip);
        assert_eq!(report.lines[0].notes, Vec::<String>::new());
        let mut report = plan();
        skip_app_copies(&mut report, "canix", &[entry("term_old")]);
        assert_eq!(report.lines[0].status, Status::Skip, "by its label");
        assert_eq!(
            report.lines[0].notes,
            [to_tui::label_origin_note(Some(1), "w1")]
        );
    }

    #[test]
    fn a_closed_spaces_record_on_m_does_not_make_a_label_ambiguous() {
        use crate::migrate::ledger::{AppCreated, AppSource};
        let entry = |workspace: &str, space: &str, at| Entry::ToApp {
            source: AppSource {
                machine: Some("canix".into()),
                session: "a".repeat(64),
                workspace_id: workspace.into(),
                terminal_ids: Some(vec![format!("old_{workspace}")]),
                label: Some("label w3".into()),
            },
            created: AppCreated {
                space: Some(format!("canix/{space}")),
                terminals: vec![],
                unknown: false,
            },
            at,
        };
        // M's w1 is "label w1", its w3 "label w3": s0 (closed on M) and s1
        // were made from the workspace now w3, when it was w2 and w1.
        let entries = [entry("w2", "s0", 1), entry("w1", "s1", 2)];
        let plan = |spaces: Option<Vec<String>>| {
            let mut report: Report =
                serde_json::from_value(plan_of(&["s1"], Some(&["w1", "w3"]))).unwrap();
            if let Some(tui) = report.tui_session.as_mut() {
                tui.spaces = spaces;
            }
            skip_app_copies(&mut report, "canix", &entries);
            report
        };
        let report = plan(Some(vec!["s1".into()]));
        assert_eq!(
            (report.lines[0].status, report.lines[0].reason.as_deref()),
            (Status::Skip, Some("copy of TUI workspace w3"))
        );
        assert_eq!(
            report.lines[0].notes,
            [to_tui::moved_origin_note(Same::Label, "w1", "w3")]
        );
        // An M that does not list its spaces: every record counts, and two
        // name "label w3".
        let report = plan(None);
        assert_eq!(report.lines[0].status, Status::Create);
    }

    #[test]
    fn remote_arguments_carry_session_and_only() {
        let tui = TuiArgs {
            session: Some("com".into()),
            ..TuiArgs::default()
        };
        let only = vec!["w1".to_string(), "w3".to_string()];
        assert_eq!(
            export_args(&tui, &only),
            [
                "migrate",
                "--export",
                "tui",
                "--session",
                "com",
                "--only",
                "w1",
                "--only",
                "w3"
            ]
        );
        assert_eq!(
            to_tui_args(&TuiArgs::default(), &[]),
            ["migrate", "--to", "tui", "--json"]
        );
    }

    #[test]
    fn a_remote_report_takes_the_machine_on_every_line() {
        let text = serde_json::json!({"to": "tui", "machine": null, "lines": [
            {"machine": null, "source": "s1", "status": "create", "reason": null, "name": "api",
             "tabs": 1, "panes": 1, "agents": 0, "denied": 0, "mod_blocks": 0,
             "live_agents": [], "copy": null, "partial": null, "future": 1}],
            "summary": {"create": 1, "skip": 0, "unchanged": 0, "ratios_lost": false},
            "applied": []})
        .to_string();
        let report = parse_report("canix", "plan", text.as_bytes()).unwrap();
        assert_eq!(report.machine.as_deref(), Some("canix"));
        assert_eq!(report.lines[0].machine.as_deref(), Some("canix"));
    }
}
