//! `install` and `uninstall` for the herdr-core host: the dialog mod, its
//! `keys.json` bindings and the rc hook. The whole plan is computed before
//! anything is written, so a bad `keys.json` or rc file changes nothing.

use crate::core::install;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

pub const MOD_NAME: &str = "herdr-revive";
pub const ACTION: &str = "mod.open:herdr-revive";
pub const KEYS: [&str; 2] = ["ctrl-alt-r", "cmd-alt-r"];
/// Written by herdr-app into each mod folder it loads, for editors.
pub const APP_FILES: [&str; 2] = ["gpui-kit.d.ts", "jsconfig.json"];
pub const MOD_FILES: [(&str, &str); 2] = [
    ("main.js", include_str!("../../mods/herdr-revive/main.js")),
    ("mod.json", include_str!("../../mods/herdr-revive/mod.json")),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Install,
    Uninstall,
}

impl Direction {
    fn name(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Uninstall => "uninstall",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirm {
    Ask,
    Yes,
    DryRun,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Create,
    Update,
    Remove,
    Unchanged,
    Skip(String),
}

impl Change {
    fn label(&self) -> String {
        match self {
            Self::Create => "create".into(),
            Self::Update => "update".into(),
            Self::Remove => "remove".into(),
            Self::Unchanged => "unchanged".into(),
            Self::Skip(_) => "skip".into(),
        }
    }

    fn is_change(&self) -> bool {
        matches!(self, Self::Create | Self::Update | Self::Remove)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub change: Change,
    pub item: String,
}

#[derive(Debug)]
enum Step {
    WriteFile {
        path: PathBuf,
        bytes: &'static str,
    },
    RemoveFile {
        path: PathBuf,
    },
    RemoveDirIfEmpty {
        path: PathBuf,
    },
    /// `read` is the text the plan was computed from.
    Keys {
        path: PathBuf,
        read: String,
        text: String,
    },
    Rc {
        write: bool,
        remove: bool,
    },
}

#[derive(Debug)]
struct Task {
    name: String,
    step: Step,
}

#[derive(Debug)]
pub struct Plan {
    pub direction: Direction,
    pub lines: Vec<Line>,
    tasks: Vec<Task>,
}

impl Plan {
    pub fn has_changes(&self) -> bool {
        self.lines.iter().any(|line| line.change.is_change())
    }

    fn push(&mut self, change: Change, item: String, step: Option<Step>) {
        if let Some(step) = step {
            self.tasks.push(Task {
                name: item.clone(),
                step,
            });
        }
        self.lines.push(Line { change, item });
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
    pub app_home: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        let home = crate::core::paths::home_dir()?;
        let app_home = crate::core::paths::app_home(&home)?;
        Ok(Self { home, app_home })
    }

    pub fn mod_dir(&self) -> PathBuf {
        self.app_home.join("mods").join(MOD_NAME)
    }

    pub fn keys_file(&self) -> PathBuf {
        self.app_home.join("keys.json")
    }
}

/// Root with a user's HOME (`sudo` keeps it) would leave root-owned files
/// and directories in that home.
pub fn check_user(euid: u32, home_uid: u32, direction: Direction) -> Result<()> {
    ensure!(
        euid != 0 || home_uid == 0,
        "run {} as your user, not with sudo: HOME belongs to uid {home_uid}",
        direction.name()
    );
    Ok(())
}

/// Refuses, at plan time, a write under a directory owned by someone else:
/// the nearest existing ancestor of a path that does not exist yet.
fn owned_ancestor(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let euid = nix::unistd::geteuid().as_raw();
    let (dir, meta) = path
        .ancestors()
        .find_map(|dir| std::fs::metadata(dir).ok().map(|meta| (dir, meta)))
        .with_context(|| format!("no directory above {} exists", path.display()))?;
    ensure!(
        meta.uid() == euid,
        "{} is owned by uid {}, not the current user (uid {euid}); run install as its owner",
        dir.display(),
        meta.uid()
    );
    Ok(())
}

pub fn plan(direction: Direction, paths: &Paths) -> Result<Plan> {
    use std::os::unix::fs::MetadataExt;
    let home = std::fs::metadata(&paths.home)
        .with_context(|| format!("cannot inspect {}", paths.home.display()))?;
    check_user(nix::unistd::geteuid().as_raw(), home.uid(), direction)?;
    let mut plan = Plan {
        direction,
        lines: Vec::new(),
        tasks: Vec::new(),
    };
    plan_mod(&mut plan, &paths.mod_dir())?;
    plan_keys(&mut plan, &paths.keys_file())?;
    plan_rc(&mut plan, &paths.home)?;
    Ok(plan)
}

fn is_symlink(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.file_type().is_symlink()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

/// A linked mod directory or file usually points into a checkout; writing or
/// deleting through it would edit that checkout, so it is left alone.
fn plan_mod(plan: &mut Plan, dir: &Path) -> Result<()> {
    let linked_dir = is_symlink(dir)?;
    for (name, contents) in MOD_FILES {
        let path = dir.join(name);
        let item = format!("mod file {}", path.display());
        if linked_dir {
            let reason = format!("{} is a symlink", dir.display());
            plan.push(Change::Skip(reason), item, None);
            continue;
        }
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => Some(meta),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", path.display())),
        };
        let (change, step) = match (plan.direction, meta) {
            (_, Some(meta)) if meta.file_type().is_symlink() => {
                (Change::Skip("a symlink".into()), None)
            }
            (_, Some(meta)) if !meta.is_file() => bail!("{} is not a file", path.display()),
            (Direction::Install, None) => {
                owned_ancestor(&path)?;
                (
                    Change::Create,
                    Some(Step::WriteFile {
                        path,
                        bytes: contents,
                    }),
                )
            }
            (Direction::Install, Some(_)) => {
                let current = std::fs::read(&path)
                    .with_context(|| format!("cannot read {}", path.display()))?;
                if current == contents.as_bytes() {
                    (Change::Unchanged, None)
                } else {
                    install::writable_mode(&path)?;
                    install::no_leftover_copy(&path)?;
                    (
                        Change::Update,
                        Some(Step::WriteFile {
                            path,
                            bytes: contents,
                        }),
                    )
                }
            }
            (Direction::Uninstall, None) => (Change::Unchanged, None),
            (Direction::Uninstall, Some(_)) => {
                install::writable_mode(&path)?;
                (Change::Remove, Some(Step::RemoveFile { path }))
            }
        };
        plan.push(change, item, step);
    }
    if plan.direction == Direction::Uninstall && !linked_dir && dir.is_dir() {
        plan_mod_dir(plan, dir)?;
    }
    Ok(())
}

/// Uninstall also takes the editor files herdr-app writes into every mod
/// folder it loads, then the folder, unless something else is left in it.
fn plan_mod_dir(plan: &mut Plan, dir: &Path) -> Result<()> {
    let regular =
        |path: &Path| std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file());
    let mut removed: Vec<std::ffi::OsString> = MOD_FILES
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| regular(&dir.join(name)))
        .map(std::ffi::OsString::from)
        .collect();
    for name in APP_FILES {
        let path = dir.join(name);
        let item = format!("app-written file {}", path.display());
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                plan.push(Change::Skip("a symlink".into()), item, None);
            }
            Ok(meta) if meta.is_file() => {
                install::writable_mode(&path)?;
                removed.push(name.into());
                plan.push(Change::Remove, item, Some(Step::RemoveFile { path }));
            }
            Ok(_) => plan.push(Change::Skip("not a file".into()), item, None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", path.display())),
        }
    }
    let mut others: Vec<String> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("cannot read {}", dir.display()))?
        .into_iter()
        .filter(|name| !removed.contains(name))
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    others.sort();
    let item = format!("mod directory {}", dir.display());
    if others.is_empty() {
        let step = Step::RemoveDirIfEmpty {
            path: dir.to_path_buf(),
        };
        plan.push(Change::Remove, item, Some(step));
    } else {
        let reason = format!("kept, it also holds {}", others.join(", "));
        plan.push(Change::Skip(reason), item, None);
    }
    Ok(())
}

fn describe(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The result of merging this mod's bindings into a `keys.json` text: one
/// change per key, and the new text when anything changes.
pub struct KeysMerge {
    pub changes: Vec<(&'static str, Change)>,
    pub text: Option<String>,
}

/// GPUI, whose keystroke parser herdr-app uses, reads `secondary` as the
/// platform's main modifier.
const SECONDARY: &str = if cfg!(target_os = "macos") {
    "cmd"
} else {
    "ctrl"
};

/// One spelling per chord: lowercase, modifier aliases mapped, modifiers
/// sorted, so `alt-ctrl-r`, `Option-Ctrl-R` and `ctrl-alt-r` compare equal.
/// The key itself may be `-` (`ctrl--`).
pub fn chord(spelling: &str) -> String {
    let lower = spelling.to_lowercase();
    let (prefix, key) = if lower == "-" {
        ("", "-")
    } else if let Some(prefix) = lower.strip_suffix("--") {
        (prefix, "-")
    } else {
        lower.rsplit_once('-').unwrap_or(("", lower.as_str()))
    };
    let mut modifiers: Vec<&str> = prefix
        .split('-')
        .filter(|part| !part.is_empty())
        .map(|part| match part {
            "option" => "alt",
            "control" => "ctrl",
            "super" | "win" => "cmd",
            "secondary" => SECONDARY,
            other => other,
        })
        .collect();
    modifiers.sort_unstable();
    modifiers.dedup();
    modifiers.push(key);
    modifiers.join("-")
}

fn is_ours(value: &Value) -> bool {
    matches!(value, Value::String(action) if action == ACTION)
}

/// The bindings in `section` for the same chord as `key`, however spelled.
fn bindings<'a>(
    section: &'a Map<String, Value>,
    key: &'a str,
) -> impl Iterator<Item = (&'a String, &'a Value)> + 'a {
    let wanted = chord(key);
    section
        .iter()
        .filter(move |(spelling, _)| chord(spelling) == wanted)
}

fn bound_to(spelling: &str, key: &str, value: &Value) -> String {
    if spelling == key {
        format!("bound to {}", describe(value))
    } else {
        format!("bound to {} as {spelling}", describe(value))
    }
}

/// herdr-app gives `terminal` precedence over every other section and a
/// `layout:<name>` section precedence over `global`, so a key bound there to
/// something else would never reach a global binding.
fn shadowing(doc: &Map<String, Value>, key: &str) -> Option<String> {
    doc.iter()
        .filter(|(section, _)| *section == "terminal" || section.starts_with("layout:"))
        .filter_map(|(section, value)| value.as_object().map(|map| (section, map)))
        .find_map(|(section, map)| {
            bindings(map, key)
                .find(|(_, value)| !is_ours(value))
                .map(|(spelling, value)| {
                    format!(
                        "{} in {section}, which takes precedence",
                        bound_to(spelling, key, value)
                    )
                })
        })
}

pub fn merge_keys(current: &str, direction: Direction) -> Result<KeysMerge> {
    let mut doc = match serde_json::from_str::<Value>(current) {
        Ok(Value::Object(map)) => map,
        Ok(_) => bail!("not a keymap (the top level is not an object)"),
        Err(e) => bail!("not valid JSON ({e})"),
    };
    let mut global = match doc.get("global") {
        None => Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => bail!("not a keymap (\"global\" is not an object)"),
    };
    let mut changes = Vec::new();
    for key in KEYS {
        let ours: Vec<String> = bindings(&global, key)
            .filter(|(_, value)| is_ours(value))
            .map(|(spelling, _)| spelling.clone())
            .collect();
        let other = bindings(&global, key).find_map(|(spelling, value)| {
            (!is_ours(value)).then(|| bound_to(spelling, key, value))
        });
        let change = match (direction, other) {
            (Direction::Install, Some(reason)) => Change::Skip(reason),
            (Direction::Install, None) if !ours.is_empty() => Change::Unchanged,
            (Direction::Install, None) => match shadowing(&doc, key) {
                Some(reason) => Change::Skip(reason),
                None => {
                    global.insert(key.to_string(), Value::String(ACTION.into()));
                    Change::Create
                }
            },
            (Direction::Uninstall, _) if !ours.is_empty() => {
                for spelling in &ours {
                    global.shift_remove(spelling);
                }
                Change::Remove
            }
            (Direction::Uninstall, Some(reason)) => Change::Skip(reason),
            (Direction::Uninstall, None) => Change::Unchanged,
        };
        changes.push((key, change));
    }
    if !changes.iter().any(|(_, change)| change.is_change()) {
        return Ok(KeysMerge {
            changes,
            text: None,
        });
    }
    doc.insert("global".into(), Value::Object(global));
    Ok(KeysMerge {
        changes,
        text: Some(serde_json::to_string_pretty(&Value::Object(doc))? + "\n"),
    })
}

/// keys.json is the app's whole keymap, not an overlay on its defaults, so a
/// missing one is never created: it would hold only these two keys.
const NO_KEYS_FILE: &str =
    "keys.json does not exist yet: open the Herdr app once, then run install again";
/// herdr-app upgrades only a keys.json nobody has edited.
const EDITED_KEYS: &str = "herdr-app stops adding new default shortcuts to an edited keys.json";

/// keys.json being edited is a normal state (herdr-app keeps its last good
/// keymap meanwhile), so a file that cannot be read or written skips the
/// keys and lets the mod and rc steps go ahead.
fn skip_keys(plan: &mut Plan, link: &Path, reason: String) {
    for key in KEYS {
        let item = format!("key {key} in {}", link.display());
        plan.push(Change::Skip(reason.clone()), item, None);
    }
}

fn plan_keys(plan: &mut Plan, link: &Path) -> Result<()> {
    if std::fs::symlink_metadata(link).is_err() {
        for key in KEYS {
            let change = match plan.direction {
                Direction::Install => Change::Skip(NO_KEYS_FILE.into()),
                Direction::Uninstall => Change::Unchanged,
            };
            plan.push(change, format!("key {key} in {}", link.display()), None);
        }
        return Ok(());
    }
    // Uninstall's fix must not send the user to `install`, which would put
    // back what they are removing.
    let fix = match plan.direction {
        Direction::Install => "fix it and run install again",
        Direction::Uninstall => "fix it and run uninstall again",
    };
    let target = match std::fs::canonicalize(link) {
        Ok(target) => target,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            skip_keys(
                plan,
                link,
                format!("keys.json is a dangling symlink: {fix}"),
            );
            return Ok(());
        }
        Err(e) => {
            skip_keys(
                plan,
                link,
                format!("keys.json cannot be resolved ({e}): {fix}"),
            );
            return Ok(());
        }
    };
    let read = match std::fs::read_to_string(&target) {
        Ok(read) => read,
        Err(e) => {
            skip_keys(plan, link, format!("keys.json cannot be read ({e}): {fix}"));
            return Ok(());
        }
    };
    let merge = match merge_keys(&read, plan.direction) {
        Ok(merge) => merge,
        Err(e) => {
            skip_keys(plan, link, format!("keys.json is {e:#}: {fix}"));
            return Ok(());
        }
    };
    if merge.text.is_some()
        && let Err(e) =
            install::writable_mode(&target).and_then(|_| install::no_leftover_copy(&target))
    {
        skip_keys(
            plan,
            link,
            format!("keys.json cannot be written ({e:#}): {fix}"),
        );
        return Ok(());
    }
    let mut step = merge.text.map(|text| Step::Keys {
        path: target,
        read,
        text,
    });
    let mut warned = false;
    for (key, change) in merge.changes {
        let mut item = format!("key {key} in {}", link.display());
        if change == Change::Create && !warned {
            item.push_str(&format!("; {EDITED_KEYS}"));
            warned = true;
        }
        let task = if change.is_change() {
            step.take()
        } else {
            None
        };
        plan.push(change, item, task);
    }
    Ok(())
}

fn flag(value: &Value, name: &str) -> Result<bool> {
    value[name]
        .as_bool()
        .with_context(|| format!("rc dry run has no {name}"))
}

fn plan_rc(plan: &mut Plan, home: &Path) -> Result<()> {
    let dry = install::run(home, false, false)?;
    let installed = flag(&dry, "installed")?;
    let up_to_date = flag(&dry, "up_to_date")?;
    let rc_file = dry["rc_file"]
        .as_str()
        .context("rc dry run has no rc_file")?;
    let elsewhere: Vec<&str> = dry["elsewhere"]
        .as_array()
        .context("rc dry run has no elsewhere")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let change = match plan.direction {
        Direction::Install if !installed => Change::Create,
        Direction::Install if up_to_date && elsewhere.is_empty() => Change::Unchanged,
        Direction::Install => Change::Update,
        Direction::Uninstall if installed || !elsewhere.is_empty() => Change::Remove,
        Direction::Uninstall => Change::Unchanged,
    };
    let mut item = format!("shell hook in {rc_file}");
    if !elsewhere.is_empty() {
        item.push_str(&format!("; also removed from {}", elsewhere.join(", ")));
    }
    let write = plan.direction == Direction::Install;
    let remove = plan.direction == Direction::Uninstall;
    if change.is_change() {
        install::preflight(home, write, remove)?;
    }
    let step = change.is_change().then_some(Step::Rc { write, remove });
    plan.push(change, item, step);
    Ok(())
}

pub fn render_line(label: &str, item: &str, reason: Option<&str>) -> String {
    match reason {
        Some(reason) => format!("  {label:<10} {item}: {reason}\n"),
        None => format!("  {label:<10} {item}\n"),
    }
}

pub fn render(plan: &Plan) -> String {
    let mut text = format!("herdr-revive {}:\n", plan.direction.name());
    for line in &plan.lines {
        let reason = match &line.change {
            Change::Skip(reason) => Some(reason.as_str()),
            _ => None,
        };
        text.push_str(&render_line(&line.change.label(), &line.item, reason));
    }
    text
}

/// What apply did beyond the plan's lines: the rc backup and cleanups.
pub fn apply(plan: &Plan, home: &Path) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    for task in &plan.tasks {
        run_step(&task.step, home, &mut notes)
            .with_context(|| format!("{} failed at: {}", plan.direction.name(), task.name))?;
    }
    Ok(notes)
}

fn create_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))
}

fn run_step(step: &Step, home: &Path, notes: &mut Vec<String>) -> Result<()> {
    match step {
        Step::WriteFile { path, bytes } => {
            create_parent(path)?;
            let mode = install::writable_mode(path)?;
            install::replace(path, bytes.as_bytes(), mode)
        }
        Step::RemoveFile { path } => {
            std::fs::remove_file(path).with_context(|| format!("cannot remove {}", path.display()))
        }
        Step::RemoveDirIfEmpty { path } => {
            let empty = std::fs::read_dir(path)
                .with_context(|| format!("cannot read {}", path.display()))?
                .next()
                .is_none();
            if empty {
                std::fs::remove_dir(path)
                    .with_context(|| format!("cannot remove {}", path.display()))?;
            } else {
                notes.push(format!(
                    "kept {}: something was added to it after the plan",
                    path.display()
                ));
            }
            Ok(())
        }
        Step::Keys { path, read, text } => {
            install::unchanged_since(path, read)?;
            let mode = install::writable_mode(path)?;
            install::replace(path, text.as_bytes(), mode)
        }
        Step::Rc { write, remove } => {
            let done = install::run(home, *write, *remove)?;
            if let Some(backup) = done["backup"].as_str() {
                notes.push(format!("rc file backup: {backup}"));
            }
            if let Some(files) = done["removed_elsewhere"].as_array() {
                for file in files.iter().filter_map(Value::as_str) {
                    notes.push(format!("removed the hook from {file}"));
                }
            }
            Ok(())
        }
    }
}

/// `y` or `yes` in any case applies; anything else, an empty line or end of
/// input included, does not.
pub fn parse_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Asks before applying, as `confirm` and `tty` allow. `None` means go
/// ahead; `Some(code)` is the exit code to stop with: 0 for a dry run, 1
/// declined, 2 not a terminal. The prompt goes to `errors`, so a piped
/// stdout cannot swallow it; `tty` means both stdin and stderr are terminals.
pub fn ask(
    confirm: Confirm,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    errors: &mut dyn Write,
    tty: bool,
) -> Result<Option<i32>> {
    match confirm {
        Confirm::DryRun => {
            output.flush()?;
            Ok(Some(0))
        }
        Confirm::Yes => Ok(None),
        Confirm::Ask => {
            if !tty {
                output.flush()?;
                writeln!(errors, "not a terminal: pass --yes to apply")?;
                errors.flush()?;
                return Ok(Some(2));
            }
            output.flush()?;
            write!(errors, "Apply? [y/N] ")?;
            errors.flush()?;
            let mut answer = String::new();
            input.read_line(&mut answer)?;
            if parse_yes(&answer) {
                Ok(None)
            } else {
                writeln!(output, "nothing changed")?;
                output.flush()?;
                Ok(Some(1))
            }
        }
    }
}

/// Prints the plan and applies it as `confirm` and `tty` allow; the result
/// is the exit code: 0 done or nothing to do, 1 declined, 2 not a terminal.
pub fn run(
    direction: Direction,
    confirm: Confirm,
    paths: &Paths,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    errors: &mut dyn Write,
    tty: bool,
) -> Result<i32> {
    let plan = plan(direction, paths)?;
    output.write_all(render(&plan).as_bytes())?;
    if !plan.has_changes() {
        writeln!(output, "nothing to change")?;
        output.flush()?;
        return Ok(0);
    }
    if let Some(code) = ask(confirm, input, output, errors, tty)? {
        return Ok(code);
    }
    for note in apply(&plan, &paths.home)? {
        writeln!(output, "{note}")?;
    }
    writeln!(output, "done")?;
    output.flush()?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDERED: &str = "{\n  \"global\": {\n    \"ctrl-q\": \"app.quit\",\n    \"ctrl-alt-r\": \"pane.reload\"\n  },\n  \"terminal\": {\n    \"ctrl-c\": \"copy\"\n  }\n}\n";
    const STOCK: &str = "{\n  \"global\": {\n    \"ctrl-q\": \"app.quit\"\n  }\n}\n";

    fn paths(root: &Path) -> Paths {
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        Paths {
            app_home: home.join(".config/herdr-app"),
            home,
        }
    }

    fn with_keys(root: &Path, text: &str) -> Paths {
        let paths = paths(root);
        std::fs::create_dir_all(&paths.app_home).unwrap();
        std::fs::write(paths.keys_file(), text).unwrap();
        paths
    }

    fn changes(merge: &KeysMerge) -> Vec<Change> {
        merge.changes.iter().map(|(_, c)| c.clone()).collect()
    }

    fn run_with(
        direction: Direction,
        confirm: Confirm,
        paths: &Paths,
        input: &[u8],
        tty: bool,
    ) -> (i32, String, String) {
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let code = run(
            direction,
            confirm,
            paths,
            &mut std::io::Cursor::new(input.to_vec()),
            &mut output,
            &mut errors,
            tty,
        )
        .unwrap();
        (
            code,
            String::from_utf8(output).unwrap(),
            String::from_utf8(errors).unwrap(),
        )
    }

    fn quiet(
        direction: Direction,
        confirm: Confirm,
        paths: &Paths,
        tty: bool,
    ) -> (i32, String, String) {
        run_with(direction, confirm, paths, b"y\n", tty)
    }

    fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut found = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    stack.push(path.clone());
                    found.push((path, Vec::new()));
                } else {
                    found.push((path.clone(), std::fs::read(&path).unwrap_or_default()));
                }
            }
        }
        found.sort();
        found
    }

    #[test]
    fn keys_merge_adds_to_an_existing_file() {
        let merge = merge_keys(STOCK, Direction::Install).unwrap();
        assert_eq!(changes(&merge), vec![Change::Create, Change::Create]);
        assert_eq!(
            merge.text.unwrap(),
            "{\n  \"global\": {\n    \"ctrl-q\": \"app.quit\",\n    \"ctrl-alt-r\": \"mod.open:herdr-revive\",\n    \"cmd-alt-r\": \"mod.open:herdr-revive\"\n  }\n}\n"
        );
    }

    #[test]
    fn keys_merge_keeps_order_and_skips_a_key_bound_elsewhere() {
        let merge = merge_keys(ORDERED, Direction::Install).unwrap();
        assert_eq!(
            changes(&merge),
            vec![Change::Skip("bound to pane.reload".into()), Change::Create]
        );
        let text = merge.text.unwrap();
        assert_eq!(
            text,
            "{\n  \"global\": {\n    \"ctrl-q\": \"app.quit\",\n    \"ctrl-alt-r\": \"pane.reload\",\n    \"cmd-alt-r\": \"mod.open:herdr-revive\"\n  },\n  \"terminal\": {\n    \"ctrl-c\": \"copy\"\n  }\n}\n"
        );
        let back = merge_keys(&text, Direction::Uninstall).unwrap();
        assert_eq!(
            changes(&back),
            vec![Change::Skip("bound to pane.reload".into()), Change::Remove]
        );
        assert_eq!(back.text.unwrap(), ORDERED);
    }

    #[test]
    fn keys_merge_skips_a_key_a_terminal_or_layout_section_takes() {
        let text = r#"{"terminal":{"ctrl-alt-r":"terminal"},"layout:dev":{"cmd-alt-r":["a"]}}"#;
        let merge = merge_keys(text, Direction::Install).unwrap();
        assert_eq!(
            changes(&merge),
            vec![
                Change::Skip("bound to terminal in terminal, which takes precedence".into()),
                Change::Skip(r#"bound to ["a"] in layout:dev, which takes precedence"#.into()),
            ]
        );
        assert!(merge.text.is_none());
        let ours =
            r#"{"terminal":{"ctrl-alt-r":"mod.open:herdr-revive"},"other":{"cmd-alt-r":"x"}}"#;
        let merge = merge_keys(ours, Direction::Install).unwrap();
        assert_eq!(changes(&merge), vec![Change::Create, Change::Create]);
    }

    #[test]
    fn keys_merge_reports_our_bindings_unchanged_and_keeps_an_emptied_global() {
        let ours = "{ \"global\": { \"ctrl-alt-r\": \"mod.open:herdr-revive\", \"cmd-alt-r\": \"mod.open:herdr-revive\" } }";
        let merge = merge_keys(ours, Direction::Install).unwrap();
        assert_eq!(changes(&merge), vec![Change::Unchanged, Change::Unchanged]);
        assert!(merge.text.is_none());
        let removed = merge_keys(ours, Direction::Uninstall).unwrap();
        assert_eq!(changes(&removed), vec![Change::Remove, Change::Remove]);
        assert_eq!(removed.text.unwrap(), "{\n  \"global\": {}\n}\n");
        let nothing = merge_keys("{}", Direction::Uninstall).unwrap();
        assert_eq!(
            changes(&nothing),
            vec![Change::Unchanged, Change::Unchanged]
        );
        assert!(nothing.text.is_none());
    }

    #[test]
    fn keys_merge_describes_a_non_string_binding() {
        let merge = merge_keys(
            r#"{"global":{"cmd-alt-r":["a","b"]}}"#,
            Direction::Uninstall,
        )
        .unwrap();
        assert_eq!(
            changes(&merge),
            vec![
                Change::Unchanged,
                Change::Skip(r#"bound to ["a","b"]"#.into())
            ]
        );
        assert!(merge.text.is_none());
    }

    #[test]
    fn keys_merge_refuses_what_it_cannot_read() {
        for (text, message) in [
            ("{", "not valid JSON (EOF while parsing"),
            ("", "not valid JSON ("),
            ("[]", "not a keymap (the top level is not an object)"),
            (
                r#"{"global": 1}"#,
                "not a keymap (\"global\" is not an object)",
            ),
        ] {
            for direction in [Direction::Install, Direction::Uninstall] {
                let error = merge_keys(text, direction).err().unwrap();
                assert!(format!("{error:#}").contains(message), "{text}: {error:#}");
            }
        }
    }

    #[test]
    fn chords_compare_by_a_normal_spelling() {
        for (a, b) in [
            ("ctrl-alt-r", "alt-ctrl-r"),
            ("ctrl-alt-r", "Option-Control-R"),
            ("cmd-alt-r", "alt-cmd-r"),
            ("ctrl-shift--", "shift-ctrl--"),
            ("-", "-"),
            ("r", "R"),
        ] {
            assert_eq!(chord(a), chord(b), "{a} vs {b}");
        }
        assert_eq!(chord("Option-Control-R"), "alt-ctrl-r");
        assert_eq!(chord("super-alt-r"), "alt-cmd-r");
        assert_eq!(chord("Win-Alt-R"), "alt-cmd-r");
        let secondary = if cfg!(target_os = "macos") {
            "alt-cmd-r"
        } else {
            "alt-ctrl-r"
        };
        assert_eq!(chord("secondary-alt-r"), secondary);
        assert_eq!(chord("ctrl--"), "ctrl--");
        assert_eq!(chord("-"), "-");
        assert_ne!(chord("ctrl-alt-r"), chord("cmd-alt-r"));
        assert_ne!(chord("ctrl-alt-r"), chord("ctrl-r"));
        assert_ne!(chord("ctrl--"), chord("ctrl-r"));
    }

    #[test]
    fn keys_merge_sees_a_key_however_it_is_spelled() {
        let other =
            r#"{"global":{"alt-ctrl-r":"pane.reload","Alt-Cmd-R":"mod.open:herdr-revive"}}"#;
        let merge = merge_keys(other, Direction::Install).unwrap();
        assert_eq!(
            changes(&merge),
            vec![
                Change::Skip("bound to pane.reload as alt-ctrl-r".into()),
                Change::Unchanged
            ]
        );
        assert!(merge.text.is_none());
        let back = merge_keys(other, Direction::Uninstall).unwrap();
        assert_eq!(
            changes(&back),
            vec![
                Change::Skip("bound to pane.reload as alt-ctrl-r".into()),
                Change::Remove
            ]
        );
        assert_eq!(
            back.text.unwrap(),
            "{\n  \"global\": {\n    \"alt-ctrl-r\": \"pane.reload\"\n  }\n}\n"
        );
        let shadowed = r#"{"terminal":{"option-control-r":"terminal"},"layout:x":{"alt-cmd-r":"mod.open:herdr-revive"}}"#;
        let merge = merge_keys(shadowed, Direction::Install).unwrap();
        assert_eq!(
            changes(&merge),
            vec![
                Change::Skip(
                    "bound to terminal as option-control-r in terminal, which takes precedence"
                        .into()
                ),
                Change::Create
            ]
        );
    }

    #[test]
    fn parse_yes_accepts_only_yes() {
        for answer in ["y", "Y\n", " yes \n", "YES"] {
            assert!(parse_yes(answer), "{answer:?}");
        }
        for answer in ["", "\n", "n", "no", "yep", "y es", "true"] {
            assert!(!parse_yes(answer), "{answer:?}");
        }
    }

    #[test]
    fn root_with_a_users_home_is_refused() {
        let error = check_user(0, 1000, Direction::Install).unwrap_err();
        assert_eq!(
            error.to_string(),
            "run install as your user, not with sudo: HOME belongs to uid 1000"
        );
        assert!(check_user(0, 1000, Direction::Uninstall).is_err());
        assert!(check_user(0, 0, Direction::Install).is_ok());
        assert!(check_user(1000, 1000, Direction::Install).is_ok());
        assert!(check_user(1000, 0, Direction::Install).is_ok());
    }

    #[test]
    fn a_missing_keys_file_is_never_created() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let planned = plan(Direction::Install, &paths).unwrap();
        let keys: Vec<_> = planned
            .lines
            .iter()
            .filter(|l| l.item.starts_with("key "))
            .map(|l| l.change.clone())
            .collect();
        assert_eq!(keys, vec![Change::Skip(NO_KEYS_FILE.into()); 2]);
        assert_eq!(quiet(Direction::Install, Confirm::Yes, &paths, false).0, 0);
        assert!(!paths.keys_file().exists());
        assert!(paths.mod_dir().join("main.js").is_file());
        let planned = plan(Direction::Uninstall, &paths).unwrap();
        assert!(
            planned
                .lines
                .iter()
                .filter(|l| l.item.starts_with("key "))
                .all(|l| l.change == Change::Unchanged)
        );
    }

    #[test]
    fn mod_plan_creates_updates_and_leaves_links_and_other_files() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let lines = |direction| {
            plan(direction, &paths)
                .unwrap()
                .lines
                .into_iter()
                .filter(|l| l.item.starts_with("mod file"))
                .map(|l| l.change)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            lines(Direction::Install),
            vec![Change::Create, Change::Create]
        );
        assert_eq!(
            lines(Direction::Uninstall),
            vec![Change::Unchanged, Change::Unchanged]
        );
        let dir = paths.mod_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.js"), "old").unwrap();
        std::fs::write(dir.join("mod.json"), MOD_FILES[1].1).unwrap();
        assert_eq!(
            lines(Direction::Install),
            vec![Change::Update, Change::Unchanged]
        );
        std::fs::write(dir.join("notes.txt"), "mine").unwrap();
        let (code, _, _) = quiet(Direction::Uninstall, Confirm::Yes, &paths, false);
        assert_eq!(code, 0);
        assert!(!dir.join("main.js").exists() && !dir.join("mod.json").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
            "mine"
        );

        std::fs::remove_dir_all(&dir).unwrap();
        let checkout = root.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        std::fs::write(checkout.join("main.js"), "checkout").unwrap();
        std::os::unix::fs::symlink(&checkout, &dir).unwrap();
        for direction in [Direction::Install, Direction::Uninstall] {
            assert!(
                lines(direction)
                    .iter()
                    .all(|c| matches!(c, Change::Skip(r) if r.ends_with("is a symlink")))
            );
        }
        let (code, _, _) = quiet(Direction::Uninstall, Confirm::Yes, &paths, false);
        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(checkout.join("main.js")).unwrap(),
            "checkout"
        );
    }

    #[test]
    fn install_twice_is_unchanged_and_uninstall_takes_out_only_ours() {
        let root = tempfile::tempdir().unwrap();
        let paths = with_keys(root.path(), STOCK);
        let (code, out, _) = quiet(Direction::Install, Confirm::Yes, &paths, false);
        assert_eq!(code, 0, "{out}");
        assert!(out.ends_with("done\n"), "{out}");
        assert_eq!(out.matches(EDITED_KEYS).count(), 1, "{out}");
        assert_eq!(
            std::fs::read_to_string(paths.mod_dir().join("main.js")).unwrap(),
            MOD_FILES[0].1
        );
        let rc = install::rc_file(&paths.home);
        assert!(
            std::fs::read_to_string(&rc)
                .unwrap()
                .contains(install::BEGIN)
        );
        let again = plan(Direction::Install, &paths).unwrap();
        assert!(
            again.lines.iter().all(|l| l.change == Change::Unchanged),
            "{again:?}"
        );
        let (code, out, _) = quiet(Direction::Install, Confirm::Ask, &paths, false);
        assert_eq!(
            (code, out.ends_with("nothing to change\n")),
            (0, true),
            "{out}"
        );

        let (code, _, _) = quiet(Direction::Uninstall, Confirm::Yes, &paths, false);
        assert_eq!(code, 0);
        assert_eq!(std::fs::read_to_string(paths.keys_file()).unwrap(), STOCK);
        assert!(!paths.mod_dir().exists());
        assert!(
            !std::fs::read_to_string(rc)
                .unwrap()
                .contains(install::BEGIN)
        );
        let after = plan(Direction::Uninstall, &paths).unwrap();
        assert!(!after.has_changes(), "{after:?}");
    }

    #[test]
    fn uninstall_removes_only_our_keys_from_an_existing_file() {
        let root = tempfile::tempdir().unwrap();
        let paths = with_keys(root.path(), ORDERED);
        let (code, out, _) = quiet(Direction::Install, Confirm::Yes, &paths, false);
        assert_eq!(code, 0);
        assert!(
            out.contains("  skip       key ctrl-alt-r in ")
                && out.contains("keys.json: bound to pane.reload\n"),
            "{out}"
        );
        assert!(
            std::fs::read_to_string(paths.keys_file())
                .unwrap()
                .contains("\"cmd-alt-r\": \"mod.open:herdr-revive\"")
        );
        let (code, _, _) = quiet(Direction::Uninstall, Confirm::Yes, &paths, false);
        assert_eq!(code, 0);
        assert_eq!(std::fs::read_to_string(paths.keys_file()).unwrap(), ORDERED);
    }

    #[test]
    fn a_linked_keys_file_is_edited_at_its_target() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        std::fs::create_dir_all(&paths.app_home).unwrap();
        let dotfiles = root.path().join("keys.json");
        std::fs::write(&dotfiles, "{}").unwrap();
        std::os::unix::fs::symlink(&dotfiles, paths.keys_file()).unwrap();
        assert_eq!(quiet(Direction::Install, Confirm::Yes, &paths, false).0, 0);
        assert!(is_symlink(&paths.keys_file()).unwrap());
        assert!(std::fs::read_to_string(&dotfiles).unwrap().contains(ACTION));
        assert_eq!(
            quiet(Direction::Uninstall, Confirm::Yes, &paths, false).0,
            0
        );
        assert!(is_symlink(&paths.keys_file()).unwrap());
        assert_eq!(
            std::fs::read_to_string(&dotfiles).unwrap(),
            "{\n  \"global\": {}\n}\n"
        );
    }

    fn key_lines(planned: &Plan) -> Vec<Change> {
        planned
            .lines
            .iter()
            .filter(|l| l.item.starts_with("key "))
            .map(|l| l.change.clone())
            .collect()
    }

    #[test]
    fn an_unusable_keys_json_on_uninstall_points_back_at_uninstall() {
        let root = tempfile::tempdir().unwrap();
        let paths = with_keys(root.path(), "{ \"global\": {}, }");
        let planned = plan(Direction::Uninstall, &paths).unwrap();
        let lines = key_lines(&planned);
        assert_eq!(lines.len(), KEYS.len());
        for change in lines {
            assert!(
                matches!(&change, Change::Skip(r) if r.ends_with("): fix it and run uninstall again")),
                "{change:?}"
            );
        }
    }

    #[test]
    fn an_unusable_keys_json_skips_the_keys_and_the_rest_still_applies() {
        for text in ["{ \"global\": {}, }", "[]", r#"{"global": 1}"#] {
            let root = tempfile::tempdir().unwrap();
            let paths = with_keys(root.path(), text);
            let planned = plan(Direction::Install, &paths).unwrap();
            for change in key_lines(&planned) {
                assert!(
                    matches!(&change, Change::Skip(r) if r.starts_with("keys.json is not ")
                        && r.ends_with("): fix it and run install again")),
                    "{change:?}"
                );
            }
            let (code, out, _) = quiet(Direction::Install, Confirm::Yes, &paths, false);
            assert_eq!(code, 0, "{out}");
            assert_eq!(std::fs::read_to_string(paths.keys_file()).unwrap(), text);
            assert!(paths.mod_dir().join("main.js").is_file());
            let rc = install::rc_file(&paths.home);
            assert!(
                std::fs::read_to_string(&rc)
                    .unwrap()
                    .contains(install::BEGIN)
            );
            let (code, out, _) = quiet(Direction::Uninstall, Confirm::Yes, &paths, false);
            assert_eq!(code, 0, "{out}");
            assert_eq!(std::fs::read_to_string(paths.keys_file()).unwrap(), text);
            assert!(!paths.mod_dir().exists());
            assert!(
                !std::fs::read_to_string(&rc)
                    .unwrap()
                    .contains(install::BEGIN)
            );
        }
    }

    #[test]
    fn a_keys_json_that_cannot_be_written_is_skipped_before_anything_is_written() {
        let root = tempfile::tempdir().unwrap();
        let paths = with_keys(root.path(), STOCK);
        std::fs::hard_link(paths.keys_file(), root.path().join("dotfiles-keys")).unwrap();
        let mut old = paths.keys_file().into_os_string();
        old.push(".revive-old");
        std::fs::write(&old, "{}").unwrap();
        let planned = plan(Direction::Install, &paths).unwrap();
        for change in key_lines(&planned) {
            assert!(
                matches!(&change, Change::Skip(r) if r.starts_with("keys.json cannot be written (")
                    && r.contains("exists from an earlier rewrite that failed")),
                "{change:?}"
            );
        }
        assert_eq!(quiet(Direction::Install, Confirm::Yes, &paths, false).0, 0);
        assert_eq!(std::fs::read_to_string(paths.keys_file()).unwrap(), STOCK);
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "{}");
    }

    #[test]
    fn a_mod_file_with_a_leftover_copy_stops_the_plan() {
        let root = tempfile::tempdir().unwrap();
        let paths = with_keys(root.path(), STOCK);
        let main = paths.mod_dir().join("main.js");
        std::fs::create_dir_all(paths.mod_dir()).unwrap();
        std::fs::write(&main, "old").unwrap();
        std::fs::hard_link(&main, root.path().join("checkout-main.js")).unwrap();
        let mut old = main.clone().into_os_string();
        old.push(".revive-old");
        std::fs::write(&old, "older").unwrap();
        let before = tree(root.path());
        let error = plan(Direction::Install, &paths).unwrap_err();
        assert!(
            format!("{error:#}").contains("exists from an earlier rewrite that failed"),
            "{error:#}"
        );
        assert_eq!(tree(root.path()), before);
    }

    #[test]
    fn uninstall_takes_the_app_written_files_and_says_why_a_folder_stays() {
        let root = tempfile::tempdir().unwrap();
        let paths = with_keys(root.path(), STOCK);
        assert_eq!(quiet(Direction::Install, Confirm::Yes, &paths, false).0, 0);
        let dir = paths.mod_dir();
        for name in APP_FILES {
            std::fs::write(dir.join(name), "written by the app").unwrap();
        }
        let planned = plan(Direction::Uninstall, &paths).unwrap();
        let dir_lines: Vec<_> = planned
            .lines
            .iter()
            .filter(|l| {
                l.item.starts_with("app-written file") || l.item.starts_with("mod directory")
            })
            .map(|l| l.change.clone())
            .collect();
        assert_eq!(dir_lines, vec![Change::Remove; 3]);
        assert_eq!(
            quiet(Direction::Uninstall, Confirm::Yes, &paths, false).0,
            0
        );
        assert!(!dir.exists());

        assert_eq!(quiet(Direction::Install, Confirm::Yes, &paths, false).0, 0);
        std::fs::write(dir.join("gpui-kit.d.ts"), "app").unwrap();
        std::fs::write(dir.join("notes.txt"), "mine").unwrap();
        std::os::unix::fs::symlink(root.path().join("elsewhere"), dir.join("jsconfig.json"))
            .unwrap();
        let (code, out, _) = quiet(Direction::Uninstall, Confirm::Yes, &paths, false);
        assert_eq!(code, 0, "{out}");
        assert!(
            out.contains("/jsconfig.json: a symlink\n")
                && out.contains("herdr-revive: kept, it also holds jsconfig.json, notes.txt\n"),
            "{out}"
        );
        assert!(!dir.join("gpui-kit.d.ts").exists());
        assert!(dir.join("notes.txt").exists());
        let (code, out, _) = quiet(Direction::Uninstall, Confirm::Yes, &paths, false);
        assert_eq!(code, 0);
        assert!(out.ends_with("nothing to change\n"), "{out}");
    }

    #[test]
    fn rc_failures_stop_the_plan_before_anything_is_written() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let paths = with_keys(root.path(), STOCK);
        let rc = install::rc_file(&paths.home);
        // A hard-linked rc file with the copy a failed rewrite leaves beside it.
        std::fs::write(&rc, "export A=1\n").unwrap();
        std::fs::hard_link(&rc, root.path().join("dotfiles-rc")).unwrap();
        let mut old = rc.as_os_str().to_owned();
        old.push(".revive-old");
        std::fs::write(&old, "export A=0\n").unwrap();
        let before = tree(root.path());
        let error = plan(Direction::Install, &paths).unwrap_err();
        assert!(
            format!("{error:#}").contains("exists from an earlier rewrite that failed"),
            "{error:#}"
        );
        assert_eq!(tree(root.path()), before);
        std::fs::remove_file(&old).unwrap();

        // A dangling symlink where the rc backup goes.
        let mut backup = rc.as_os_str().to_owned();
        backup.push(".revive-backup");
        std::os::unix::fs::symlink(root.path().join("gone"), &backup).unwrap();
        let before = tree(root.path());
        let error = plan(Direction::Install, &paths).unwrap_err();
        assert!(
            format!("{error:#}").contains("is a dangling symlink"),
            "{error:#}"
        );
        assert_eq!(tree(root.path()), before);
        std::fs::remove_file(&backup).unwrap();

        // An rc file owned by someone else, linked in as install.rs's test does.
        let other = Path::new("/etc/hosts");
        let euid = nix::unistd::geteuid().as_raw();
        let Ok(meta) = std::fs::metadata(other) else {
            return;
        };
        if euid == 0 || meta.uid() == euid {
            return;
        }
        std::fs::remove_file(&rc).unwrap();
        std::os::unix::fs::symlink(other, &rc).unwrap();
        let before = tree(root.path());
        let error = plan(Direction::Install, &paths).unwrap_err();
        assert!(
            format!("{error:#}").contains("not the current user"),
            "{error:#}"
        );
        assert_eq!(tree(root.path()), before);
    }

    #[test]
    fn a_stray_block_that_cannot_be_written_stops_the_plan() {
        let root = tempfile::tempdir().unwrap();
        let paths = with_keys(root.path(), STOCK);
        let rc = install::rc_file(&paths.home);
        let stray = if rc.ends_with(".bashrc") {
            paths.home.join(".bash_profile")
        } else {
            paths.home.join(".bashrc")
        };
        std::fs::write(&stray, install::block()).unwrap();
        let mut backup = stray.as_os_str().to_owned();
        backup.push(".revive-backup");
        std::os::unix::fs::symlink(root.path().join("gone"), &backup).unwrap();
        let before = tree(root.path());
        for direction in [Direction::Install, Direction::Uninstall] {
            let error = plan(direction, &paths).unwrap_err();
            assert!(
                format!("{error:#}").contains("is a dangling symlink"),
                "{error:#}"
            );
        }
        assert_eq!(tree(root.path()), before);
    }

    #[test]
    fn asking_needs_a_terminal_and_a_yes() {
        let root = tempfile::tempdir().unwrap();
        let paths = with_keys(root.path(), STOCK);
        let before = tree(root.path());
        let (code, out, err) = quiet(Direction::Install, Confirm::Ask, &paths, false);
        assert_eq!(code, 2);
        assert!(out.contains("create     key ctrl-alt-r"), "{out}");
        assert_eq!(err, "not a terminal: pass --yes to apply\n");
        assert_eq!(tree(root.path()), before);
        let (code, out, err) = quiet(Direction::Install, Confirm::DryRun, &paths, true);
        assert_eq!(code, 0);
        assert!(!out.contains("Apply?") && err.is_empty());
        assert_eq!(tree(root.path()), before);

        let (code, out, err) = run_with(Direction::Install, Confirm::Ask, &paths, b"\n", true);
        assert_eq!(code, 1);
        assert_eq!(err, "Apply? [y/N] ");
        assert!(out.ends_with("nothing changed\n"), "{out}");
        assert_eq!(tree(root.path()), before);

        let (code, out, err) = quiet(Direction::Install, Confirm::Ask, &paths, true);
        assert_eq!(code, 0, "{out}");
        assert_eq!(err, "Apply? [y/N] ");
        assert!(out.ends_with("done\n"));
        assert!(paths.mod_dir().exists());
    }

    #[test]
    fn a_failed_step_is_named_and_earlier_steps_stay() {
        let root = tempfile::tempdir().unwrap();
        let paths = with_keys(root.path(), STOCK);
        let plan = plan(Direction::Install, &paths).unwrap();
        std::fs::write(paths.keys_file(), "{}").unwrap();
        let error = apply(&plan, &paths.home).err().unwrap();
        assert!(
            format!("{error:#}").contains("install failed at: key ctrl-alt-r in"),
            "{error:#}"
        );
        assert!(paths.mod_dir().join("main.js").exists());
        assert!(!install::rc_file(&paths.home).exists());
    }

    fn asked(confirm: Confirm, answer: &str, tty: bool) -> (Option<i32>, String, String) {
        let mut input = answer.as_bytes();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let code = ask(confirm, &mut input, &mut output, &mut errors, tty).unwrap();
        (
            code,
            String::from_utf8(output).unwrap(),
            String::from_utf8(errors).unwrap(),
        )
    }

    #[test]
    fn ask_follows_the_install_rules() {
        assert_eq!(asked(Confirm::DryRun, "", true).0, Some(0));
        assert_eq!(asked(Confirm::Yes, "", false).0, None);
        let (code, _, errors) = asked(Confirm::Ask, "y\n", false);
        assert_eq!(code, Some(2));
        assert_eq!(errors, "not a terminal: pass --yes to apply\n");
        let (code, _, errors) = asked(Confirm::Ask, "yes\n", true);
        assert_eq!(code, None);
        assert_eq!(errors, "Apply? [y/N] ");
        let (code, output, _) = asked(Confirm::Ask, "\n", true);
        assert_eq!(code, Some(1));
        assert_eq!(output, "nothing changed\n");
    }

    #[test]
    fn render_line_pads_the_label_and_appends_a_reason() {
        assert_eq!(render_line("create", "w1", None), "  create     w1\n");
        assert_eq!(
            render_line("skip", "w2", Some("no panes")),
            "  skip       w2: no panes\n"
        );
    }
}
