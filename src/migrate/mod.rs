//! `migrate`: copy a herdr TUI session's workspaces into the desktop app, or
//! the app's spaces into a TUI session. The source is never changed.

pub mod external;
pub mod ledger;
pub mod tree;

use crate::core::setup::{self, Confirm};
use anyhow::{Result, bail};
use ledger::AppCreated;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    App,
    Tui,
}

impl Side {
    pub fn name(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Tui => "tui",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    To(Side),
    ExportTui,
}

/// The TUI side's overrides: the herdr host's global flags plus `--session`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TuiArgs {
    pub session: Option<String>,
    pub socket: Option<PathBuf>,
    pub config_dir: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub herdr_bin: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub mode: Mode,
    pub machine: Option<String>,
    pub only: Vec<String>,
    pub confirm: Confirm,
    pub json: bool,
    pub tui: TuiArgs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Create,
    Skip,
    Unchanged,
}

/// One source workspace or space. Unknown fields are accepted: a newer
/// herdr-revive on another machine may print more.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanLine {
    pub machine: Option<String>,
    pub source: String,
    pub status: Status,
    pub reason: Option<String>,
    /// The destination name; empty lets the destination name it.
    pub name: String,
    pub tabs: usize,
    pub panes: usize,
    pub agents: usize,
    /// Commands the source's policy does not allow; they open a shell.
    pub denied: usize,
    pub mod_blocks: usize,
    /// TUI panes whose agent still runs: after the copy two processes
    /// resume the same session until the user closes one.
    pub live_agents: Vec<String>,
    /// The existing copy of an `unchanged` source.
    pub copy: Option<String>,
    /// What an earlier run made before it stopped.
    pub partial: Option<AppCreated>,
    /// Earlier copies the plan cannot rule out as this source's, and an
    /// earlier run that may have left a space; lines from before this field
    /// have none.
    #[serde(default)]
    pub notes: Vec<String>,
}

impl PlanLine {
    pub fn new(machine: Option<&str>, source: &str, name: String) -> Self {
        Self {
            machine: machine.map(String::from),
            source: source.into(),
            status: Status::Create,
            reason: None,
            name,
            tabs: 0,
            panes: 0,
            agents: 0,
            denied: 0,
            mod_blocks: 0,
            live_agents: Vec::new(),
            copy: None,
            partial: None,
            notes: Vec::new(),
        }
    }

    pub fn skip(mut self, reason: &str) -> Self {
        self.status = Status::Skip;
        self.reason = Some(reason.into());
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub create: usize,
    pub skip: usize,
    pub unchanged: usize,
    pub ratios_lost: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub source: String,
    pub created: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub to: Side,
    pub machine: Option<String>,
    pub lines: Vec<PlanLine>,
    pub summary: Summary,
    /// `None` when what was copied is unknown: another machine's result
    /// could not be read.
    pub applied: Option<Vec<Applied>>,
}

impl Report {
    pub fn new(to: Side, machine: Option<&str>, lines: Vec<PlanLine>) -> Self {
        let count = |status: Status| lines.iter().filter(|line| line.status == status).count();
        let create = count(Status::Create);
        let summary = Summary {
            create,
            skip: count(Status::Skip),
            unchanged: count(Status::Unchanged),
            ratios_lost: to == Side::App && create > 0,
        };
        Self {
            to,
            machine: machine.map(String::from),
            lines,
            summary,
            applied: Some(Vec::new()),
        }
    }

    pub fn record(&mut self, applied: Applied) {
        self.applied.get_or_insert_with(Vec::new).push(applied);
    }
}

fn plural(count: usize, word: &str) -> String {
    format!("{count} {word}{}", if count == 1 { "" } else { "s" })
}

pub fn created_text(created: &AppCreated) -> String {
    let mut parts = Vec::new();
    if let Some(space) = &created.space {
        parts.push(format!("space {space}"));
    }
    if !created.terminals.is_empty() {
        parts.push(format!("terminals {}", created.terminals.join(" ")));
    }
    if parts.is_empty() {
        "nothing".into()
    } else {
        parts.join(", ")
    }
}

/// `2026-11-11 08:30 UTC` for a ledger time in ms since the epoch.
pub fn utc_minute(ms: u64) -> String {
    let minutes = ms / 60_000;
    let (days, minute_of_day) = (minutes / 1440, minutes % 1440);
    // Days since 1970-01-01 to a proleptic Gregorian date, by 400-year eras
    // that start on 1 March (Howard Hinnant's civil_from_days).
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        minute_of_day / 60,
        minute_of_day % 60
    )
}

/// The panes whose folder is gone: they open a shell in the home folder,
/// and their command, which would run in the wrong place, is not started.
pub fn missing_note(folders: &[String]) -> String {
    let mut folders: Vec<&str> = folders.iter().map(String::as_str).collect();
    let panes = folders.len();
    folders.sort_unstable();
    folders.dedup();
    let folders = folders.join(", ");
    if panes == 1 {
        format!(
            "the folder of 1 pane no longer exists ({folders}): it opens a shell in the home \
             folder, without its command"
        )
    } else {
        format!(
            "the folders of {panes} panes no longer exist ({folders}): they open a shell in \
             the home folder, without their command"
        )
    }
}

/// "an earlier run at <time>", or without the time when it is not known.
pub fn earlier_run(at: Option<u64>) -> String {
    match at {
        Some(ms) => format!("an earlier run at {}", utc_minute(ms)),
        None => "an earlier run".into(),
    }
}

pub fn describe(line: &PlanLine) -> String {
    let mut item = match &line.machine {
        Some(machine) => format!("{machine}: {}", line.source),
        None => line.source.clone(),
    };
    if line.name.is_empty() {
        item.push_str(" -> (automatic name)");
    } else {
        item.push_str(&format!(" -> {:?}", line.name));
    }
    if line.status == Status::Skip {
        return item;
    }
    item.push_str(&format!(
        ", {}, {}, {}",
        plural(line.tabs, "tab"),
        plural(line.panes, "pane"),
        plural(line.agents, "agent")
    ));
    if line.denied > 0 {
        item.push_str(&format!(
            ", {} not allowed by the policy (opens a shell)",
            plural(line.denied, "command")
        ));
    }
    if line.mod_blocks > 0 {
        item.push_str(&format!(
            ", {} left out",
            plural(line.mod_blocks, "mod pane")
        ));
    }
    if let Some(copy) = &line.copy {
        item.push_str(&format!(", copy {copy}"));
    }
    if !line.live_agents.is_empty() {
        item.push_str(&format!(
            "; still live in the TUI: {} (both resume the same session until one is closed)",
            line.live_agents.join(" ")
        ));
    }
    if let Some(partial) = &line.partial {
        item.push_str(&format!(
            "; an earlier run stopped part way and left {}: close it first",
            created_text(partial)
        ));
    }
    for note in &line.notes {
        item.push_str("; ");
        item.push_str(note);
    }
    item
}

pub fn render(report: &Report) -> String {
    let mut text = format!("herdr-revive migrate --to {}:\n", report.to.name());
    for line in &report.lines {
        let (label, reason) = match line.status {
            Status::Create => ("create", None),
            Status::Unchanged => ("unchanged", None),
            // `describe` stops before the notes of a skip line, which follow
            // its reason instead.
            Status::Skip => (
                "skip",
                line.reason.as_deref().map(|reason| {
                    std::iter::once(reason)
                        .chain(line.notes.iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join("; ")
                }),
            ),
        };
        text.push_str(&setup::render_line(
            label,
            &describe(line),
            reason.as_deref(),
        ));
    }
    if report.summary.ratios_lost {
        text.push_str("split sizes are not copied: the app's splits come out even\n");
    }
    text
}

/// Prints the plan (text, or the JSON report for the dialog), confirms the
/// way `install` does, and applies. With `--json` there is no prompt:
/// `--yes` applies and `--dry-run` only prints.
#[allow(clippy::too_many_arguments)]
pub fn drive(
    mut report: Report,
    confirm: Confirm,
    json: bool,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    errors: &mut dyn Write,
    tty: bool,
    apply: impl FnOnce(&mut Report) -> Result<()>,
) -> Result<i32> {
    let creates = report
        .lines
        .iter()
        .any(|line| line.status == Status::Create);
    if json {
        if confirm == Confirm::Ask {
            bail!("--json needs --dry-run or --yes");
        }
        let result = if creates && confirm == Confirm::Yes {
            apply(&mut report)
        } else {
            Ok(())
        };
        let written = (|| -> Result<()> {
            serde_json::to_writer_pretty(&mut *output, &report)?;
            writeln!(output)?;
            output.flush()?;
            Ok(())
        })();
        finish(result, written)?;
        return Ok(0);
    }
    output.write_all(render(&report).as_bytes())?;
    if !creates {
        writeln!(output, "nothing to copy")?;
        output.flush()?;
        return Ok(0);
    }
    if let Some(code) = setup::ask(confirm, input, output, errors, tty)? {
        return Ok(code);
    }
    let result = apply(&mut report);
    let written = (|| -> Result<()> {
        for applied in report.applied.iter().flatten() {
            let item = format!(
                "{} -> {}",
                applied.source,
                applied_text(report.to, &applied.created)
            );
            output.write_all(setup::render_line("copied", &item, None).as_bytes())?;
        }
        Ok(())
    })();
    finish(result, written)?;
    writeln!(output, "done")?;
    output.flush()?;
    Ok(0)
}

/// What a copy made, as the plan lines name it. `--json` keeps the object;
/// one this run cannot read (another machine's newer report) is shown as
/// it came.
fn applied_text(to: Side, created: &Value) -> String {
    match to {
        Side::App => AppCreated::deserialize(created)
            .map(|created| created_text(&created))
            .unwrap_or_else(|_| created.to_string()),
        Side::Tui => match created["workspace_id"].as_str() {
            Some(workspace) => format!("TUI workspace {workspace}"),
            None => created.to_string(),
        },
    }
}

/// A failed apply's error names what it made, the only clean-up record for
/// a copy into the TUI, so a failed write (say, a closed pipe) must not
/// replace it.
fn finish(result: Result<()>, written: Result<()>) -> Result<()> {
    match (result, written) {
        (Err(error), Err(write)) => {
            Err(error.context(format!("the report could not be printed ({write:#})")))
        }
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines() -> Vec<PlanLine> {
        let mut create = PlanLine::new(Some("canix"), "w1", "work".into());
        create.tabs = 2;
        create.panes = 3;
        create.agents = 1;
        create.live_agents = vec!["w1:p2".into()];
        let skip = PlanLine::new(None, "w2", String::new()).skip("no panes");
        let mut unchanged = PlanLine::new(None, "w3", "api".into());
        unchanged.status = Status::Unchanged;
        unchanged.tabs = 1;
        unchanged.panes = 1;
        unchanged.copy = Some("s7".into());
        vec![create, skip, unchanged]
    }

    #[test]
    fn the_text_plan_has_one_line_per_source() {
        let report = Report::new(Side::App, None, lines());
        assert_eq!(
            render(&report),
            "herdr-revive migrate --to app:\n\
             \x20 create     canix: w1 -> \"work\", 2 tabs, 3 panes, 1 agent; still live in the TUI: w1:p2 (both resume the same session until one is closed)\n\
             \x20 skip       w2 -> (automatic name): no panes\n\
             \x20 unchanged  w3 -> \"api\", 1 tab, 1 pane, 0 agents, copy s7\n\
             split sizes are not copied: the app's splits come out even\n"
        );
    }

    #[test]
    fn the_summary_counts_lines_and_ratios_are_lost_only_into_the_app() {
        let report = Report::new(Side::App, None, lines());
        assert_eq!(
            report.summary,
            Summary {
                create: 1,
                skip: 1,
                unchanged: 1,
                ratios_lost: true
            }
        );
        assert!(!Report::new(Side::Tui, None, lines()).summary.ratios_lost);
    }

    #[test]
    fn the_json_shape_is_stable() {
        let mut report = Report::new(Side::Tui, Some("canix"), lines());
        report.record(Applied {
            source: "s1".into(),
            created: serde_json::json!({"session": "x", "workspace_id": "w5"}),
        });
        let value = serde_json::to_value(&report).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["to", "machine", "lines", "summary", "applied"]);
        let line: Vec<&str> = value["lines"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            line,
            [
                "machine",
                "source",
                "status",
                "reason",
                "name",
                "tabs",
                "panes",
                "agents",
                "denied",
                "mod_blocks",
                "live_agents",
                "copy",
                "partial",
                "notes"
            ]
        );
        assert_eq!(value["to"], "tui");
        assert_eq!(value["lines"][1]["status"], "skip");
        let back: Report = serde_json::from_value(value).unwrap();
        assert_eq!(back, report);
    }

    fn driven(
        confirm: Confirm,
        json: bool,
        answer: &str,
        tty: bool,
        lines: Vec<PlanLine>,
    ) -> (i32, String, bool) {
        let mut input = answer.as_bytes();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let mut applied = false;
        let code = drive(
            Report::new(Side::Tui, None, lines),
            confirm,
            json,
            &mut input,
            &mut output,
            &mut errors,
            tty,
            |report| {
                applied = true;
                report.record(Applied {
                    source: "w1".into(),
                    created: serde_json::json!({"workspace_id": "w9"}),
                });
                Ok(())
            },
        )
        .unwrap();
        (code, String::from_utf8(output).unwrap(), applied)
    }

    #[test]
    fn drive_applies_only_when_confirmed() {
        let (code, _, applied) = driven(Confirm::DryRun, false, "", true, lines());
        assert_eq!((code, applied), (0, false));
        let (code, _, applied) = driven(Confirm::Ask, false, "y\n", false, lines());
        assert_eq!((code, applied), (2, false));
        let (code, output, applied) = driven(Confirm::Ask, false, "y\n", true, lines());
        assert_eq!((code, applied), (0, true));
        assert!(output.ends_with("  copied     w1 -> TUI workspace w9\ndone\n"));
        let nothing = vec![lines().remove(1)];
        let (code, output, applied) = driven(Confirm::Yes, false, "", false, nothing);
        assert_eq!((code, applied), (0, false));
        assert!(output.ends_with("nothing to copy\n"));
    }

    #[test]
    fn drive_prints_json_and_applies_with_yes() {
        let (code, output, applied) = driven(Confirm::DryRun, true, "", false, lines());
        assert_eq!((code, applied), (0, false));
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["applied"], serde_json::json!([]));
        let (_, output, applied) = driven(Confirm::Yes, true, "", false, lines());
        assert!(applied);
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["applied"][0]["source"], "w1");
    }

    #[test]
    fn describe_names_denied_commands_left_out_panes_and_partial_copies() {
        let mut line = PlanLine::new(None, "w4", "x".into());
        line.tabs = 1;
        line.panes = 2;
        line.denied = 2;
        line.mod_blocks = 1;
        line.partial = Some(AppCreated {
            space: Some("sp1".into()),
            terminals: vec!["t1".into(), "t2".into()],
            unknown: false,
        });
        assert_eq!(
            describe(&line),
            "w4 -> \"x\", 1 tab, 2 panes, 0 agents, 2 commands not allowed by the policy (opens a shell), 1 mod pane left out; an earlier run stopped part way and left space sp1, terminals t1 t2: close it first"
        );
    }

    #[test]
    fn notes_follow_the_line_and_name_the_time_in_utc() {
        let mut line = PlanLine::new(None, "w4", "x".into());
        line.notes = vec![format!("{} copied it", earlier_run(Some(0))), "b".into()];
        assert_eq!(
            describe(&line),
            "w4 -> \"x\", 0 tabs, 0 panes, 0 agents; an earlier run at 1970-01-01 00:00 UTC copied it; b"
        );
        assert_eq!(earlier_run(None), "an earlier run");
        assert_eq!(utc_minute(951_782_400_000), "2000-02-29 00:00 UTC");
        assert_eq!(utc_minute(1_793_000_000_000), "2026-10-26 07:33 UTC");
        assert_eq!(utc_minute(1_767_225_599_999), "2025-12-31 23:59 UTC");
        assert_eq!(utc_minute(4_107_542_400_000), "2100-03-01 00:00 UTC");
    }

    #[test]
    fn a_skip_line_shows_its_notes_after_its_reason() {
        let mut line = PlanLine::new(None, "w4", "x".into()).skip("copy of app space s1");
        line.notes = vec!["a".into(), "b".into()];
        let text = render(&Report::new(Side::App, None, vec![line]));
        assert!(
            text.contains("  skip       w4 -> \"x\": copy of app space s1; a; b\n"),
            "{text}"
        );
    }

    #[test]
    fn a_copy_is_named_as_the_plan_names_it() {
        let app = serde_json::json!({"space": "sp27", "terminals": ["t41", "t42"]});
        assert_eq!(
            applied_text(Side::App, &app),
            "space sp27, terminals t41 t42"
        );
        let tui = serde_json::json!({"session": "b".repeat(64), "workspace_id": "w4"});
        assert_eq!(applied_text(Side::Tui, &tui), "TUI workspace w4");
        let newer = serde_json::json!({"kind": "other"});
        assert_eq!(applied_text(Side::Tui, &newer), newer.to_string());
        assert_eq!(applied_text(Side::App, &newer), newer.to_string());
    }

    #[test]
    fn a_missing_folder_note_counts_panes_and_names_each_folder_once() {
        assert_eq!(
            missing_note(&["/w/gone".into()]),
            "the folder of 1 pane no longer exists (/w/gone): it opens a shell in the home \
             folder, without its command"
        );
        assert_eq!(
            missing_note(&["/b".into(), "/a".into(), "/b".into()]),
            "the folders of 3 panes no longer exist (/a, /b): they open a shell in the home \
             folder, without their command"
        );
    }

    #[test]
    fn created_text_lists_what_exists() {
        assert_eq!(created_text(&AppCreated::default()), "nothing");
        let only_terminals = AppCreated {
            space: None,
            terminals: vec!["t1".into()],
            unknown: false,
        };
        assert_eq!(created_text(&only_terminals), "terminals t1");
        let only_space = AppCreated {
            space: Some("sp1".into()),
            terminals: Vec::new(),
            unknown: false,
        };
        assert_eq!(created_text(&only_space), "space sp1");
    }

    #[test]
    fn a_name_with_quotes_or_newlines_stays_on_one_line() {
        let line = PlanLine::new(None, "w1", "a\"b\nc".into());
        assert_eq!(
            describe(&line),
            "w1 -> \"a\\\"b\\nc\", 0 tabs, 0 panes, 0 agents"
        );
    }

    #[test]
    fn a_declined_prompt_exits_1_without_applying() {
        let (code, output, applied) = driven(Confirm::Ask, false, "n\n", true, lines());
        assert_eq!((code, applied), (1, false));
        assert!(output.ends_with("nothing changed\n"));
    }

    #[test]
    fn json_without_dry_run_or_yes_is_refused() {
        let mut input = "".as_bytes();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let result = drive(
            Report::new(Side::Tui, None, lines()),
            Confirm::Ask,
            true,
            &mut input,
            &mut output,
            &mut errors,
            true,
            |_| Ok(()),
        );
        assert!(result.is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn drive_decides_from_the_lines_not_the_summary() {
        let mut report = Report::new(Side::Tui, None, lines());
        report.lines.remove(0);
        let mut input = "".as_bytes();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let mut applied = false;
        let code = drive(
            report,
            Confirm::Yes,
            false,
            &mut input,
            &mut output,
            &mut errors,
            false,
            |_| {
                applied = true;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!((code, applied), (0, false));
        assert!(
            String::from_utf8(output)
                .unwrap()
                .ends_with("nothing to copy\n")
        );
    }

    fn failing_apply(report: &mut Report) -> Result<()> {
        report.record(Applied {
            source: "w1".into(),
            created: serde_json::json!({"workspace_id": "w9"}),
        });
        bail!("second source failed")
    }

    #[test]
    fn a_failed_apply_still_reports_what_was_copied() {
        let mut input = "".as_bytes();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let result = drive(
            Report::new(Side::Tui, None, lines()),
            Confirm::Yes,
            false,
            &mut input,
            &mut output,
            &mut errors,
            false,
            failing_apply,
        );
        assert_eq!(result.unwrap_err().to_string(), "second source failed");
        let text = String::from_utf8(output).unwrap();
        assert!(text.ends_with("  copied     w1 -> TUI workspace w9\n"));

        let mut output = Vec::new();
        let result = drive(
            Report::new(Side::Tui, None, lines()),
            Confirm::Yes,
            true,
            &mut input,
            &mut output,
            &mut errors,
            false,
            failing_apply,
        );
        assert!(result.is_err());
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["applied"][0]["source"], "w1");
    }

    /// Fails every write that holds `needle`, as a pipe closed by then would.
    struct FailOn(&'static str);

    impl Write for FailOn {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if String::from_utf8_lossy(buf).contains(self.0) {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_output_does_not_hide_the_apply_error() {
        for (json, needle) in [(true, "applied"), (false, "copied")] {
            let mut input = "".as_bytes();
            let mut errors = Vec::new();
            let error = drive(
                Report::new(Side::Tui, None, lines()),
                Confirm::Yes,
                json,
                &mut input,
                &mut FailOn(needle),
                &mut errors,
                false,
                failing_apply,
            )
            .unwrap_err();
            let text = format!("{error:#}");
            assert!(
                text.starts_with("the report could not be printed (")
                    && text.ends_with("): second source failed"),
                "{text}"
            );
        }
        assert!(finish(Ok(()), Ok(())).is_ok());
        let write = finish(Ok(()), Err(anyhow::anyhow!("pipe")));
        assert_eq!(write.unwrap_err().to_string(), "pipe");
    }

    #[test]
    fn an_unknown_result_prints_applied_null() {
        let mut report = Report::new(Side::Tui, Some("canix"), lines());
        report.applied = None;
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["applied"], serde_json::Value::Null);
        let back: Report = serde_json::from_value(value).unwrap();
        assert_eq!(back.applied, None);
        report.record(Applied {
            source: "s1".into(),
            created: serde_json::json!({}),
        });
        assert_eq!(report.applied.map(|a| a.len()), Some(1));
    }
}
