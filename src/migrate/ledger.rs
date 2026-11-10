//! `migrated.json` in the core state directory: which sources a migration
//! copied and what it made, so a later run reports them `unchanged` while
//! the copy exists. A copy that stopped part way is kept under `partial`
//! while the app lists its space or terminals, so the next plan skips that
//! source until it is closed.
//!
//! herdr numbers workspaces from a counter that restarts with its server,
//! so a TUI workspace id can come back on a different workspace. A TUI
//! workspace is therefore recorded with the ids of its terminals, which
//! herdr never gives out twice, and with its label. A restored workspace
//! gets new terminals but keeps its label, so it is the same workspace while
//! one of the terminals is still in it or its label is unchanged.

use super::tree::space_name;
use crate::model::{TOOL, native_tool};
use crate::store;
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The ledger's own version: the snapshot schema can move without it.
const LEDGER_SCHEMA: u32 = 1;
const LOCK_WAIT: Duration = Duration::from_secs(2);
const LOCK_STEP: Duration = Duration::from_millis(20);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSource {
    pub machine: Option<String>,
    pub session: String,
    pub workspace_id: String,
    /// `None` in entries from before the ids were recorded, and from an
    /// export that does not carry them.
    #[serde(default)]
    pub terminal_ids: Option<Vec<String>>,
    /// The workspace's label without the TUI's `[n] ` number; `None` in
    /// entries from before it was recorded.
    #[serde(default)]
    pub label: Option<String>,
}

impl AppSource {
    pub fn seen(&self) -> Seen<'_> {
        Seen {
            terminal_ids: self.terminal_ids.as_deref(),
            label: self.label.as_deref(),
        }
    }

    /// The same workspace id of the same TUI session; whether it is still
    /// the same workspace is `same_workspace`'s question.
    pub fn same_id(&self, other: &Self) -> bool {
        self.machine == other.machine
            && self.session == other.session
            && self.workspace_id == other.workspace_id
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppCreated {
    pub space: Option<String>,
    pub terminals: Vec<String>,
    /// A partial copy whose last `herdr-app do` timed out, so it may have
    /// made more than it lists: a space even when `space` is empty.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unknown: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuiSource {
    pub space: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuiCreated {
    pub session: String,
    pub workspace_id: String,
    /// `None` in entries from before the ids were recorded, or when the TUI
    /// did not list the new workspace after the copy.
    #[serde(default)]
    pub terminal_ids: Option<Vec<String>>,
    /// The label the TUI gave the new workspace, as `AppSource::label`.
    #[serde(default)]
    pub label: Option<String>,
}

impl TuiCreated {
    pub fn seen(&self) -> Seen<'_> {
        Seen {
            terminal_ids: self.terminal_ids.as_deref(),
            label: self.label.as_deref(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "direction", rename_all = "snake_case", deny_unknown_fields)]
pub enum Entry {
    ToApp {
        source: AppSource,
        created: AppCreated,
        at: u64,
    },
    ToTui {
        source: TuiSource,
        created: TuiCreated,
        at: u64,
    },
}

impl Entry {
    /// Whether `other` records the same copy, so it replaces this one. A
    /// space copied into two TUI sessions has a copy in each.
    fn same_source(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::ToApp { source: a, .. }, Self::ToApp { source: b, .. }) => a.same_id(b),
            (
                Self::ToTui {
                    source: a,
                    created: made_a,
                    ..
                },
                Self::ToTui {
                    source: b,
                    created: made_b,
                    ..
                },
            ) => a == b && made_a.session == made_b.session,
            _ => false,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    tool: String,
    schema: u32,
    entries: Vec<Entry>,
    #[serde(default)]
    partial: Vec<Entry>,
}

/// One ledger entry as the plan reads it.
#[derive(Debug, PartialEq, Eq)]
pub struct Recorded<'a, S, C> {
    pub source: &'a S,
    pub created: &'a C,
    pub at: u64,
}

// Derived, these would ask `S: Copy`; only the references are copied.
impl<S, C> Clone for Recorded<'_, S, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<S, C> Copy for Recorded<'_, S, C> {}

/// What tells a TUI workspace from another that later takes its id: its
/// terminal ids and its label. `None` where it is not known.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Seen<'a> {
    pub terminal_ids: Option<&'a [String]>,
    pub label: Option<&'a str>,
}

/// How a live TUI workspace matches the one recorded under its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Same {
    /// Neither a recorded terminal nor the label: maybe another workspace.
    No,
    /// One of its recorded terminals is still there.
    Terminal,
    /// Only by its label. A restart gives every terminal a new id, but
    /// herdr also makes an unnamed workspace's label from its folder, so
    /// another workspace in the same folder matches too; the plan says so.
    Label,
}

/// Whether the TUI workspace `recorded` under an id is the live workspace
/// `live` with that id: one of its terminals is still there, or (after a
/// restart) its label, without the `[n] ` number, is unchanged and not
/// empty. What is not known never matches, so an entry with neither cannot
/// tell and is not the same.
pub fn same_workspace(recorded: Seen, live: Seen) -> Same {
    let shared = match (recorded.terminal_ids, live.terminal_ids) {
        (Some(recorded), Some(live)) => recorded.iter().any(|id| live.contains(id)),
        _ => false,
    };
    let named = match (recorded.label, live.label) {
        (Some(recorded), Some(live)) => {
            let recorded = space_name(recorded);
            !recorded.is_empty() && recorded == space_name(live)
        }
        _ => false,
    };
    if shared {
        Same::Terminal
    } else if named {
        Same::Label
    } else {
        Same::No
    }
}

fn app_in<'a>(
    entries: &'a [Entry],
    machine: Option<&str>,
    session: &str,
) -> Vec<Recorded<'a, AppSource, AppCreated>> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::ToApp {
                source,
                created,
                at,
            } if source.machine.as_deref() == machine && source.session == session => {
                Some(Recorded {
                    source,
                    created,
                    at: *at,
                })
            }
            _ => None,
        })
        .collect()
}

/// A workspace of the live TUI session.
#[derive(Clone, Copy, Debug)]
pub struct LiveWorkspace<'a> {
    pub id: &'a str,
    pub seen: Seen<'a>,
}

/// A record of a TUI workspace as it was: the id it had then, what tells
/// it apart, when it was made, and what it records (`value`).
#[derive(Clone, Debug)]
pub struct WorkspaceRecord<'a, T> {
    pub value: T,
    pub id: &'a str,
    pub seen: Seen<'a>,
    pub at: u64,
}

/// A record matched to a live workspace. `was` is the id it was recorded
/// under when the TUI has renumbered its workspaces since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Matched<T> {
    pub value: T,
    pub same: Same,
    pub at: u64,
    pub was: Option<String>,
}

/// The plan note for a record matched to a workspace that has another id
/// than the one it was recorded under (`was`): the TUI renumbered its
/// workspaces, or, by the label alone, a new workspace took the label of a
/// closed one. `copy` names the earlier copy; `if_another` says how to copy
/// anyway when the label alone matched.
pub fn moved_note(same: Same, was: &str, copy: &str, if_another: &str) -> String {
    if same == Same::Terminal {
        format!("matched an earlier copy {copy} by terminal (was {was})")
    } else {
        format!("matched an earlier copy {copy} by label (was {was}): if {if_another}")
    }
}

pub fn shares_terminal(a: Seen, b: Seen) -> bool {
    match (a.terminal_ids, b.terminal_ids) {
        (Some(a), Some(b)) => a.iter().any(|id| b.contains(id)),
        _ => false,
    }
}

fn label_of(seen: Seen) -> Option<String> {
    seen.label.map(space_name).filter(|label| !label.is_empty())
}

/// A record whose id no live workspace has, or whose id another live
/// workspace now has: herdr's ids restart with its server, and a rebuild
/// numbers the workspaces again from 1.
fn displaced(id: &str, seen: Seen, lives: &[LiveWorkspace]) -> bool {
    lives
        .iter()
        .find(|live| live.id == id)
        .is_none_or(|live| same_workspace(seen, live.seen) == Same::No)
}

/// How many live workspaces have `label`.
fn live_with_label(label: &str, lives: &[LiveWorkspace]) -> usize {
    lives
        .iter()
        .filter(|live| label_of(live.seen).as_deref() == Some(label))
        .count()
}

/// The record of the live workspace `target`: by its id, as
/// `same_workspace` judges it, the best of them (`best`). When none of
/// those is the same workspace, the TUI may have renumbered its workspaces:
/// a displaced record that shares a terminal with `target` (the newest), or
/// else the one displaced record with `target`'s label, when no other live
/// workspace has it. A record by `target`'s id that is another workspace is
/// returned as `Same::No` only when nothing else matched, so its plan note
/// does not name another workspace's copy.
pub fn match_live<T: Clone>(
    records: &[WorkspaceRecord<T>],
    lives: &[LiveWorkspace],
    target: &str,
) -> Option<Matched<T>> {
    let target_seen = lives
        .iter()
        .find(|live| live.id == target)
        .map(|live| live.seen);
    let direct = best(
        records.iter().filter(|record| record.id == target),
        |record| target_seen.map_or(Same::No, |seen| same_workspace(record.seen, seen)),
        |record| record.at,
    );
    let matched = |record: &WorkspaceRecord<T>, same, was: Option<&str>| Matched {
        value: record.value.clone(),
        same,
        at: record.at,
        was: was.map(String::from),
    };
    if let Some((record, same)) = &direct
        && *same != Same::No
    {
        return Some(matched(record, *same, None));
    }
    if let Some(target_seen) = target_seen {
        let candidates: Vec<&WorkspaceRecord<T>> = records
            .iter()
            .filter(|record| displaced(record.id, record.seen, lives))
            .collect();
        if let Some(record) = candidates
            .iter()
            .filter(|record| shares_terminal(record.seen, target_seen))
            .max_by_key(|record| record.at)
        {
            return Some(matched(record, Same::Terminal, Some(record.id)));
        }
        if let Some(label) =
            label_of(target_seen).filter(|label| live_with_label(label, lives) == 1)
        {
            let labelled: Vec<_> = candidates
                .iter()
                .filter(|record| label_of(record.seen).as_deref() == Some(label.as_str()))
                .collect();
            if let [record] = labelled.as_slice() {
                return Some(matched(record, Same::Label, Some(record.id)));
            }
        }
    }
    direct.map(|(record, same)| matched(record, same, None))
}

/// The live workspace `record` is: the one with its id, as `same_workspace`
/// judges it; else, after the TUI renumbered its workspaces, one that shares
/// a terminal with it, or else the one live workspace with its label when
/// no other displaced record of `pool` (the records of the same kind and
/// session) has that label. Returns the live id, how it matched, and the
/// recorded id when it was renumbered; `Same::No` with the recorded id when
/// a live workspace has that id and nothing else matched.
pub fn live_of<T>(
    record: &WorkspaceRecord<T>,
    pool: &[(&str, Seen)],
    lives: &[LiveWorkspace],
) -> Option<(String, Same, Option<String>)> {
    let by_id = lives.iter().find(|live| live.id == record.id);
    if let Some(live) = by_id {
        let same = same_workspace(record.seen, live.seen);
        if same != Same::No {
            return Some((live.id.to_string(), same, None));
        }
    }
    let renumbered = Some(record.id.to_string());
    if let Some(live) = lives
        .iter()
        .find(|live| shares_terminal(record.seen, live.seen))
    {
        return Some((live.id.to_string(), Same::Terminal, renumbered));
    }
    if let Some(label) = label_of(record.seen).filter(|label| live_with_label(label, lives) == 1) {
        let displaced_with_label = pool
            .iter()
            .filter(|(id, seen)| displaced(id, *seen, lives))
            .filter(|(_, seen)| label_of(*seen).as_deref() == Some(label.as_str()))
            .count();
        if displaced_with_label == 1
            && let Some(live) = lives
                .iter()
                .find(|live| label_of(live.seen).as_deref() == Some(label.as_str()))
        {
            return Some((live.id.to_string(), Same::Label, renumbered));
        }
    }
    by_id.map(|live| (live.id.to_string(), Same::No, None))
}

/// Of several records of one TUI workspace id, the one that matches the
/// live workspace best: a shared terminal over the label alone over
/// neither, then the newest.
pub fn best<T>(
    candidates: impl Iterator<Item = T>,
    same: impl Fn(&T) -> Same,
    at: impl Fn(&T) -> u64,
) -> Option<(T, Same)> {
    let rank = |same: Same| match same {
        Same::No => 0,
        Same::Label => 1,
        Same::Terminal => 2,
    };
    candidates
        .map(|candidate| {
            let same = same(&candidate);
            (candidate, same)
        })
        .max_by_key(|(candidate, same)| (rank(*same), at(candidate)))
}

/// Whether two records of a TUI session are one workspace: the same
/// record, the same id that `same_workspace` matches, or a shared terminal
/// (herdr never reuses a terminal id).
fn same_workspace_of(a: &AppSource, b: &AppSource) -> bool {
    a.machine == b.machine
        && a.session == b.session
        && (a == b
            || a.same_id(b) && same_workspace(a.seen(), b.seen()) != Same::No
            || shares_terminal(a.seen(), b.seen()))
}

/// A partial copy with no space and no terminal to look for: only a
/// timed-out one is kept like that, as one that may have left a space.
pub fn names_nothing(created: &AppCreated) -> bool {
    created.space.is_none() && created.terminals.is_empty()
}

/// The core state directory: the ledger lives where the app is.
pub fn dir() -> Result<PathBuf> {
    crate::core::paths::state_dir(None)
}

pub struct Ledger {
    path: PathBuf,
    entries: Vec<Entry>,
    partial: Vec<Entry>,
    _lock: std::fs::File,
}

impl Ledger {
    /// Holds `migrated.lock` until dropped, so two migrations cannot both
    /// decide that one source still needs copying.
    pub fn open(dir: &Path) -> Result<Self> {
        store::private_dir(dir)?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("migrated.lock"))
            .context("cannot open the migration lock")?;
        // A process forked elsewhere in this program shares the lock until it
        // execs, so a lock that frees within moments is not another migration.
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match lock.try_lock_exclusive() {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        bail!("another migration is running")
                    }
                    std::thread::sleep(LOCK_STEP);
                }
                Err(e) => return Err(e).context("cannot lock the migration ledger"),
            }
        }
        let path = dir.join("migrated.json");
        let file = load(&path)?;
        Ok(Self {
            path,
            entries: file.entries,
            partial: file.partial,
            _lock: lock,
        })
    }

    /// The copy of the TUI workspace with `source`'s id, whichever
    /// workspace had that id then.
    pub fn copied_to_app(&self, source: &AppSource) -> Option<Recorded<'_, AppSource, AppCreated>> {
        find_app(&self.entries, source)
    }

    /// The newest partial copy of the TUI workspace with `source`'s id.
    pub fn partial_to_app(
        &self,
        source: &AppSource,
    ) -> Option<Recorded<'_, AppSource, AppCreated>> {
        self.partials_to_app(source).pop()
    }

    /// Every partial copy of the TUI workspace with `source`'s id, oldest
    /// first: a later run of that source does not make an earlier partial
    /// space go away.
    pub fn partials_to_app(&self, source: &AppSource) -> Vec<Recorded<'_, AppSource, AppCreated>> {
        self.partial
            .iter()
            .filter_map(|entry| match entry {
                Entry::ToApp {
                    source: s,
                    created,
                    at,
                } if s.same_id(source) => Some(Recorded {
                    source: s,
                    created,
                    at: *at,
                }),
                _ => None,
            })
            .collect()
    }

    /// The copy of `source` into the TUI session `session`.
    pub fn copied_to_tui(
        &self,
        source: &TuiSource,
        session: &str,
    ) -> Option<Recorded<'_, TuiSource, TuiCreated>> {
        self.entries.iter().find_map(|entry| match entry {
            Entry::ToTui {
                source: s,
                created,
                at,
            } if s == source && created.session == session => Some(Recorded {
                source: s,
                created,
                at: *at,
            }),
            _ => None,
        })
    }

    /// The TUI workspace `--to app` copied into `space`, partial copies
    /// included.
    pub fn app_space_origin(&self, space: &str) -> Option<Recorded<'_, AppSource, AppCreated>> {
        self.entries
            .iter()
            .chain(&self.partial)
            .find_map(|entry| match entry {
                Entry::ToApp {
                    source,
                    created,
                    at,
                } if created.space.as_deref() == Some(space) => Some(Recorded {
                    source,
                    created,
                    at: *at,
                }),
                _ => None,
            })
    }

    /// The app space `--to tui` copied into the TUI workspace that had this
    /// id, and how it matches the live workspace `live` with that id. A
    /// ledger from before `record` dropped the copies an id outlived can
    /// hold several; the best match wins (`best`).
    pub fn tui_workspace_origin(
        &self,
        session: &str,
        workspace_id: &str,
        live: Seen,
    ) -> Option<(Recorded<'_, TuiSource, TuiCreated>, Same)> {
        let candidates = self.entries.iter().filter_map(|entry| match entry {
            Entry::ToTui {
                source,
                created,
                at,
            } if created.session == session && created.workspace_id == workspace_id => {
                Some(Recorded {
                    source,
                    created,
                    at: *at,
                })
            }
            _ => None,
        });
        best(
            candidates,
            |recorded| same_workspace(recorded.created.seen(), live),
            |recorded| recorded.at,
        )
    }

    /// Records a full copy. The source's partial copies stay while they can
    /// still be open (`prune_partials`), except a timed-out one that names
    /// nothing, which nothing could tell is closed.
    pub fn record(&mut self, entry: Entry) -> Result<()> {
        // A full copy drops a timed-out partial that names nothing only when
        // it is the same workspace: another workspace may have its id after
        // a renumbering, and that partial is the only record of a space it
        // may have left (`to_app::apply` drops one matched by its label).
        self.partial.retain(|old| match (old, &entry) {
            (
                Entry::ToApp {
                    source: a, created, ..
                },
                Entry::ToApp { source: b, .. },
            ) if names_nothing(created) => !same_workspace_of(a, b),
            _ => true,
        });
        // A copy recorded under the same TUI workspace id is replaced only
        // when it is that workspace: after the TUI renumbers its workspaces
        // another one may hold the id, and its copy is still found
        // (`match_live`, `live_of`). A space is a copy of one workspace, so
        // a new record of it replaces the old one.
        self.entries.retain(|old| match (old, &entry) {
            (
                Entry::ToApp {
                    source: a,
                    created: made_a,
                    ..
                },
                Entry::ToApp {
                    source: b,
                    created: made_b,
                    ..
                },
            ) => {
                let same_space = made_a.space.is_some() && made_a.space == made_b.space;
                !(same_space || a.same_id(b) && same_workspace(a.seen(), b.seen()) != Same::No)
            }
            _ => !old.same_source(&entry),
        });
        self.entries.push(entry);
        self.save()
    }

    /// The full copies to the app of `machine`'s TUI session `session`.
    pub fn app_entries(
        &self,
        machine: Option<&str>,
        session: &str,
    ) -> Vec<Recorded<'_, AppSource, AppCreated>> {
        app_in(&self.entries, machine, session)
    }

    /// The partial copies to the app of `machine`'s TUI session `session`.
    pub fn app_partials(
        &self,
        machine: Option<&str>,
        session: &str,
    ) -> Vec<Recorded<'_, AppSource, AppCreated>> {
        app_in(&self.partial, machine, session)
    }

    /// The copies into TUI session `session`.
    pub fn tui_entries(&self, session: &str) -> Vec<Recorded<'_, TuiSource, TuiCreated>> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::ToTui {
                    source,
                    created,
                    at,
                } if created.session == session => Some(Recorded {
                    source,
                    created,
                    at: *at,
                }),
                _ => None,
            })
            .collect()
    }

    /// Adds a partial copy beside the source's earlier ones. The same copy
    /// recorded again replaces itself, and so does the run's own pending
    /// entry (same source, same `at`).
    pub fn record_partial(&mut self, entry: Entry) -> Result<()> {
        self.partial.retain(|old| match (old, &entry) {
            (
                Entry::ToApp {
                    source: a,
                    created: made_a,
                    at: at_a,
                },
                Entry::ToApp {
                    source: b,
                    created: made_b,
                    at: at_b,
                },
            ) => {
                // A copy that names nothing cannot be told from another such.
                !(a.same_id(b)
                    && (at_a == at_b
                        || (!names_nothing(made_b)
                            && made_a.space == made_b.space
                            && made_a.terminals == made_b.terminals)))
            }
            _ => !old.same_source(&entry),
        });
        self.partial.push(entry);
        self.save()
    }

    /// `now`, or the next millisecond no partial copy of `source` uses yet,
    /// so a run's own entries are told apart from an earlier run's by `at`.
    pub fn fresh_at(&self, source: &AppSource, now: u64) -> u64 {
        let used: Vec<u64> = self
            .partials_to_app(source)
            .iter()
            .map(|partial| partial.at)
            .collect();
        let mut at = now;
        while used.contains(&at) {
            at += 1;
        }
        at
    }

    /// Drops the partial copy of `source` recorded at `at`: the pending
    /// entry of a run that ended having made nothing.
    pub fn forget_partial(&mut self, source: &AppSource, at: u64) -> Result<()> {
        let before = self.partial.len();
        self.partial.retain(|entry| {
            !matches!(entry, Entry::ToApp { source: s, at: when, .. } if s.same_id(source) && *when == at)
        });
        if self.partial.len() == before {
            return Ok(());
        }
        self.save()
    }

    /// Drops the partial copies to `machine`'s app (`None`: this one's) that
    /// are closed: `open` says whether the app still lists its space or one
    /// of its terminals. A timed-out one that names nothing stays until its
    /// source is copied in full (`record`).
    pub fn prune_partials(
        &mut self,
        machine: Option<&str>,
        open: impl Fn(&AppCreated) -> bool,
    ) -> Result<()> {
        let before = self.partial.len();
        self.partial.retain(|entry| match entry {
            Entry::ToApp {
                source, created, ..
            } => source.machine.as_deref() != machine || names_nothing(created) || open(created),
            Entry::ToTui { .. } => true,
        });
        if self.partial.len() == before {
            return Ok(());
        }
        self.save()
    }

    fn save(&self) -> Result<()> {
        store::atomic_json(
            &self.path,
            &File {
                tool: TOOL.into(),
                schema: LEDGER_SCHEMA,
                entries: self.entries.clone(),
                partial: self.partial.clone(),
            },
        )
    }
}

fn load(path: &Path) -> Result<File> {
    let file: File = match std::fs::symlink_metadata(path) {
        Ok(_) => {
            store::read_json(path).with_context(|| format!("{} is unreadable", path.display()))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => File {
            tool: TOOL.into(),
            schema: LEDGER_SCHEMA,
            entries: Vec::new(),
            partial: Vec::new(),
        },
        Err(e) => return Err(e.into()),
    };
    ensure!(
        native_tool(&file.tool) && file.schema == LEDGER_SCHEMA,
        "{} is not a herdr-revive migration ledger",
        path.display()
    );
    Ok(file)
}

/// Every entry in `dir`'s ledger, partial copies included, read without the
/// lock and without creating anything: a run that only looks at this
/// machine's copies for another machine's plan. Each save replaces the file
/// whole, so a read never sees half of one.
pub fn read(dir: &Path) -> Result<Vec<Entry>> {
    let file = load(&dir.join("migrated.json"))?;
    Ok(file.entries.into_iter().chain(file.partial).collect())
}

/// `read` without the partial copies.
pub fn read_copies(dir: &Path) -> Result<Vec<Entry>> {
    Ok(load(&dir.join("migrated.json"))?.entries)
}

/// Only the partial copies of `dir`'s ledger.
pub fn read_partials(dir: &Path) -> Result<Vec<Entry>> {
    Ok(load(&dir.join("migrated.json"))?.partial)
}

fn find_app<'a>(
    entries: &'a [Entry],
    source: &AppSource,
) -> Option<Recorded<'a, AppSource, AppCreated>> {
    entries.iter().find_map(|entry| match entry {
        Entry::ToApp {
            source: s,
            created,
            at,
        } if s.same_id(source) => Some(Recorded {
            source: s,
            created,
            at: *at,
        }),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(workspace: &str) -> AppSource {
        AppSource {
            machine: None,
            session: "a".repeat(64),
            workspace_id: workspace.into(),
            terminal_ids: Some(vec![format!("term_{workspace}")]),
            label: Some("work".into()),
        }
    }

    fn copy(space: &str) -> AppCreated {
        AppCreated {
            space: Some(space.into()),
            terminals: vec!["t1".into()],
            unknown: false,
        }
    }

    #[test]
    fn a_recorded_copy_survives_reopening_and_keeps_a_partial_that_names_its_space() {
        let root = tempfile::tempdir().unwrap();
        {
            let mut ledger = Ledger::open(root.path()).unwrap();
            ledger
                .record_partial(Entry::ToApp {
                    source: source("w1"),
                    created: copy("s1"),
                    at: 1,
                })
                .unwrap();
            assert_eq!(
                ledger.partial_to_app(&source("w1")).map(|r| r.created),
                Some(&copy("s1"))
            );
            ledger
                .record(Entry::ToApp {
                    source: source("w1"),
                    created: copy("s2"),
                    at: 2,
                })
                .unwrap();
        }
        let ledger = Ledger::open(root.path()).unwrap();
        let copied = ledger.copied_to_app(&source("w1")).unwrap();
        assert_eq!((copied.created, copied.at), (&copy("s2"), 2));
        assert_eq!(
            ledger.partial_to_app(&source("w1")).map(|r| r.created),
            Some(&copy("s1")),
            "s1 may still be open"
        );
        assert_eq!(ledger.copied_to_app(&source("w2")), None);
        let mut other_machine = source("w1");
        other_machine.machine = Some("canix".into());
        assert_eq!(ledger.copied_to_app(&other_machine), None);
        let mut other_terminals = source("w1");
        other_terminals.terminal_ids = Some(vec!["term_new".into()]);
        let found = ledger.copied_to_app(&other_terminals).unwrap();
        assert_eq!(
            found.source.terminal_ids,
            source("w1").terminal_ids,
            "found by id; the terminals are the plan's to compare"
        );
    }

    #[test]
    fn a_new_copy_of_a_source_replaces_the_old_entry_of_its_session_only() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        for (at, session, workspace) in [(1, "b", "w5"), (2, "b", "w6"), (3, "c", "w7")] {
            ledger
                .record(Entry::ToTui {
                    source: TuiSource { space: "s1".into() },
                    created: TuiCreated {
                        session: session.repeat(64),
                        workspace_id: workspace.into(),
                        terminal_ids: None,
                        label: None,
                    },
                    at,
                })
                .unwrap();
        }
        let text = std::fs::read_to_string(root.path().join("migrated.json")).unwrap();
        let file: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(file["entries"].as_array().unwrap().len(), 2);
        assert_eq!(file["entries"][0]["direction"], "to_tui");
        assert_eq!(file["entries"][0]["created"]["workspace_id"], "w6");
        assert_eq!(file["entries"][1]["created"]["workspace_id"], "w7");
        let source = TuiSource { space: "s1".into() };
        let found = |session: &str| {
            ledger
                .copied_to_tui(&source, &session.repeat(64))
                .map(|recorded| recorded.created.workspace_id.clone())
        };
        assert_eq!(found("b").as_deref(), Some("w6"));
        assert_eq!(found("c").as_deref(), Some("w7"));
        assert_eq!(found("d"), None);
    }

    #[test]
    fn a_copy_given_a_reused_tui_id_drops_the_copy_that_had_it() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let to_tui = |space: &str, session: &str, workspace: &str, at| Entry::ToTui {
            source: TuiSource {
                space: space.into(),
            },
            created: TuiCreated {
                session: session.repeat(64),
                workspace_id: workspace.into(),
                terminal_ids: None,
                label: None,
            },
            at,
        };
        ledger.record(to_tui("s1", "a", "w1", 1)).unwrap();
        ledger.record(to_tui("s2", "a", "w2", 2)).unwrap();
        ledger.record(to_tui("s3", "b", "w1", 3)).unwrap();
        ledger
            .record(Entry::ToApp {
                source: source("w1"),
                created: copy("s5"),
                at: 4,
            })
            .unwrap();
        ledger.record(to_tui("s9", "a", "w1", 5)).unwrap();
        ledger.record(to_tui("s2", "a", "w3", 6)).unwrap();
        let spaces: Vec<(&str, &str)> = ledger
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::ToTui {
                    source, created, ..
                } => Some((source.space.as_str(), created.workspace_id.as_str())),
                Entry::ToApp { .. } => None,
            })
            .collect();
        assert_eq!(
            spaces,
            [("s1", "w1"), ("s3", "w1"), ("s9", "w1"), ("s2", "w3")],
            "a renumbered TUI may have moved s1's workspace; s2 was copied again"
        );
        assert!(
            ledger.app_space_origin("s5").is_some(),
            "to_app entries stay"
        );
    }

    #[test]
    fn a_copy_to_the_app_replaces_the_same_workspace_or_space_only() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let seen = |terminal: &str, label: &str| {
            let mut source = source("w1");
            source.terminal_ids = Some(vec![terminal.to_string()]);
            source.label = Some(label.to_string());
            source
        };
        let mut record = |source: AppSource, space: &str, at| {
            ledger
                .record(Entry::ToApp {
                    source,
                    created: copy(space),
                    at,
                })
                .unwrap();
        };
        record(seen("t_api", "api"), "s1", 1);
        record(seen("t_web", "web"), "s2", 2);
        record(seen("t_api", "api"), "s3", 3);
        record(seen("t_new", "new"), "s2", 4);
        let spaces: Vec<(&str, &str)> = ledger
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::ToApp {
                    source, created, ..
                } => Some((created.space.as_deref()?, source.label.as_deref()?)),
                Entry::ToTui { .. } => None,
            })
            .collect();
        assert_eq!(
            spaces,
            [("s3", "api"), ("s2", "new")],
            "w1 as web is another workspace than api, but s2 is one space"
        );
    }

    #[test]
    fn a_record_is_matched_by_id_then_terminal_then_a_unique_label() {
        fn seen<'a>(terminal_ids: &'a [String], label: &'a str) -> Seen<'a> {
            Seen {
                terminal_ids: Some(terminal_ids),
                label: Some(label),
            }
        }
        let [t_api, t_web, t_new, t_old] =
            ["t_api", "t_web", "t_new", "t_old"].map(|id| vec![id.to_string()]);
        let (t_api, t_web, t_new, t_old) = (&t_api[..], &t_web[..], &t_new[..], &t_old[..]);
        let record = |value: &'static str, id, seen, at| WorkspaceRecord {
            value,
            id,
            seen,
            at,
        };
        let lives = [
            LiveWorkspace {
                id: "w1",
                seen: seen(t_api, "[1] api"),
            },
            LiveWorkspace {
                id: "w2",
                seen: seen(t_new, "[2] web"),
            },
        ];
        let records = [
            record("s_api", "w2", seen(t_api, "api"), 1),
            record("s_web", "w3", seen(t_old, "web"), 2),
        ];
        let w1 = match_live(&records, &lives, "w1").unwrap();
        assert_eq!(
            (w1.value, w1.same, w1.was.as_deref()),
            ("s_api", Same::Terminal, Some("w2"))
        );
        let w2 = match_live(&records, &lives, "w2").unwrap();
        assert_eq!(
            (w2.value, w2.same, w2.was.as_deref()),
            ("s_web", Same::Label, Some("w3"))
        );
        assert_eq!(
            live_of(
                &records[1],
                &[("w2", records[0].seen), ("w3", records[1].seen)],
                &lives
            ),
            Some(("w2".to_string(), Same::Label, Some("w3".to_string())))
        );
        // A second displaced "web" record: the label tells neither apart, and
        // the record under w2 is returned as another workspace's.
        let records = [
            records[0].clone(),
            records[1].clone(),
            record("s_old", "w4", seen(t_web, "web"), 3),
        ];
        let w2 = match_live(&records, &lives, "w2").unwrap();
        assert_eq!((w2.value, w2.same, w2.was), ("s_api", Same::No, None));
        let pool: Vec<(&str, Seen)> = records.iter().map(|r| (r.id, r.seen)).collect();
        assert_eq!(live_of(&records[1], &pool, &lives), None);
    }

    #[test]
    fn the_best_record_of_an_id_is_the_closest_match_then_the_newest() {
        let ranked = |records: &[(Same, u64)]| {
            best(records.iter(), |record| record.0, |record| record.1).map(|(r, _)| *r)
        };
        assert_eq!(
            ranked(&[(Same::No, 9), (Same::Terminal, 1), (Same::Label, 5)]),
            Some((Same::Terminal, 1))
        );
        assert_eq!(
            ranked(&[(Same::No, 9), (Same::Label, 2), (Same::Label, 5)]),
            Some((Same::Label, 5))
        );
        assert_eq!(ranked(&[(Same::No, 3), (Same::No, 7)]), Some((Same::No, 7)));
        assert_eq!(ranked(&[]), None);
    }

    #[test]
    fn partial_copies_of_a_source_add_up_and_closed_ones_are_pruned() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let partial = |space: Option<&str>, unknown: bool, at| Entry::ToApp {
            source: source("w1"),
            created: AppCreated {
                space: space.map(String::from),
                terminals: vec![],
                unknown,
            },
            at,
        };
        ledger
            .record_partial(partial(Some("s1"), false, 1))
            .unwrap();
        ledger
            .record_partial(partial(Some("s2"), false, 2))
            .unwrap();
        ledger
            .record_partial(partial(Some("s1"), false, 3))
            .unwrap();
        ledger.record_partial(partial(None, true, 4)).unwrap();
        let spaces = |ledger: &Ledger| {
            ledger
                .partials_to_app(&source("w1"))
                .iter()
                .map(|p| (p.created.space.clone(), p.at))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            spaces(&ledger),
            [(Some("s2".into()), 2), (Some("s1".into()), 3), (None, 4)],
            "the same copy again replaces itself"
        );
        ledger
            .prune_partials(None, |created| created.space.as_deref() == Some("s2"))
            .unwrap();
        assert_eq!(
            spaces(&ledger),
            [(Some("s2".into()), 2), (None, 4)],
            "a timed-out one naming nothing stays"
        );
        ledger
            .record(Entry::ToApp {
                source: source("w1"),
                created: copy("s3"),
                at: 5,
            })
            .unwrap();
        assert_eq!(
            spaces(&ledger),
            [(Some("s2".into()), 2)],
            "the full copy drops only the one naming nothing"
        );
    }

    #[test]
    fn a_runs_pending_partial_is_told_apart_by_its_own_time() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let nothing = AppCreated {
            unknown: true,
            ..AppCreated::default()
        };
        let partial = |created: AppCreated, at| Entry::ToApp {
            source: source("w1"),
            created,
            at,
        };
        // An earlier run's timed-out copy, then this run's pending entry in
        // the same millisecond.
        ledger.record_partial(partial(nothing.clone(), 7)).unwrap();
        let at = ledger.fresh_at(&source("w1"), 7);
        assert_eq!(at, 8);
        ledger.record_partial(partial(nothing.clone(), at)).unwrap();
        assert_eq!(ledger.partials_to_app(&source("w1")).len(), 2);
        // The run ends: what it made replaces its pending entry only.
        ledger.record_partial(partial(copy("s1"), at)).unwrap();
        let ats: Vec<_> = ledger
            .partials_to_app(&source("w1"))
            .iter()
            .map(|p| (p.at, p.created.space.clone()))
            .collect();
        assert_eq!(ats, [(7, None), (8, Some("s1".into()))]);
        // A run that made nothing drops its own pending entry only.
        let at = ledger.fresh_at(&source("w1"), 7);
        assert_eq!(at, 9);
        ledger.record_partial(partial(nothing, at)).unwrap();
        ledger.forget_partial(&source("w1"), at).unwrap();
        assert_eq!(ledger.partials_to_app(&source("w1")).len(), 2);
    }

    #[test]
    fn a_second_migration_cannot_open_the_ledger() {
        let root = tempfile::tempdir().unwrap();
        let _first = Ledger::open(root.path()).unwrap();
        let error = Ledger::open(root.path()).err().unwrap();
        assert!(error.to_string().contains("another migration is running"));
    }

    #[test]
    fn a_lock_held_briefly_by_another_holder_is_waited_out() {
        let root = tempfile::tempdir().unwrap();
        let first = Ledger::open(root.path()).unwrap();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(first);
        });
        let started = std::time::Instant::now();
        Ledger::open(root.path()).unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(150));
        holder.join().unwrap();
    }

    #[test]
    fn each_copy_names_its_origin_on_the_other_side() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        ledger
            .record(Entry::ToApp {
                source: source("w1"),
                created: copy("s1"),
                at: 1,
            })
            .unwrap();
        ledger
            .record_partial(Entry::ToApp {
                source: source("w2"),
                created: copy("s2"),
                at: 2,
            })
            .unwrap();
        ledger
            .record(Entry::ToTui {
                source: TuiSource { space: "s9".into() },
                created: TuiCreated {
                    session: "b".repeat(64),
                    workspace_id: "w7".into(),
                    terminal_ids: Some(vec!["term_7".into()]),
                    label: Some("api".into()),
                },
                at: 3,
            })
            .unwrap();
        assert_eq!(
            ledger.app_space_origin("s1").map(|r| r.source),
            Some(&source("w1"))
        );
        assert_eq!(
            ledger.app_space_origin("s2").map(|r| (r.source, r.at)),
            Some((&source("w2"), 2))
        );
        assert_eq!(ledger.app_space_origin("s9"), None);
        let origin = ledger
            .tui_workspace_origin(&"b".repeat(64), "w7", Seen::default())
            .map(|(origin, _)| origin);
        assert_eq!(origin.map(|o| o.source.space.as_str()), Some("s9"));
        assert_eq!(
            origin.and_then(|o| o.created.terminal_ids.clone()),
            Some(vec!["term_7".to_string()])
        );
        assert_eq!(
            ledger.tui_workspace_origin(&"a".repeat(64), "w7", Seen::default()),
            None
        );
        assert_eq!(
            ledger.tui_workspace_origin(&"b".repeat(64), "w1", Seen::default()),
            None
        );
    }

    #[test]
    fn the_ledger_keeps_its_own_schema() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        ledger
            .record(Entry::ToApp {
                source: source("w1"),
                created: copy("s1"),
                at: 1,
            })
            .unwrap();
        let text = std::fs::read_to_string(root.path().join("migrated.json")).unwrap();
        let file: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(file["schema"], LEDGER_SCHEMA);
        assert_eq!(
            LEDGER_SCHEMA, 1,
            "existing migrated.json files are schema 1"
        );
    }

    #[test]
    fn read_lists_entries_and_partials_without_locking_or_creating() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("none");
        assert_eq!(read(&missing).unwrap(), Vec::new());
        assert!(!missing.exists());
        let mut ledger = Ledger::open(root.path()).unwrap();
        ledger
            .record(Entry::ToApp {
                source: source("w1"),
                created: copy("s1"),
                at: 1,
            })
            .unwrap();
        ledger
            .record_partial(Entry::ToApp {
                source: source("w2"),
                created: copy("s2"),
                at: 2,
            })
            .unwrap();
        let entries = read(root.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[1], Entry::ToApp { source, .. } if source.workspace_id == "w2"));
        std::fs::write(root.path().join("migrated.json"), "{").unwrap();
        assert!(read(root.path()).is_err());
    }

    #[test]
    fn a_workspace_is_the_same_while_a_terminal_or_its_label_is_left() {
        let ids = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        let (old, shared, new) = (
            ids(&["term_a", "term_b"]),
            ids(&["term_b", "term_c"]),
            ids(&["term_c"]),
        );
        fn seen<'a>(terminal_ids: Option<&'a [String]>, label: Option<&'a str>) -> Seen<'a> {
            Seen {
                terminal_ids,
                label,
            }
        }
        let (old, shared, new) = (old.as_slice(), shared.as_slice(), new.as_slice());
        let recorded = seen(Some(old), Some("work"));
        // Renamed without a restart: the terminals are the same.
        assert_eq!(
            same_workspace(recorded, seen(Some(shared), Some("[1] frontend"))),
            Same::Terminal
        );
        assert_eq!(
            same_workspace(recorded, seen(Some(shared), Some("[1] work"))),
            Same::Terminal,
            "a terminal outranks the label"
        );
        // Restarted: new terminals, same label, its number may move.
        assert_eq!(
            same_workspace(recorded, seen(Some(new), Some("[3] work"))),
            Same::Label
        );
        // Another workspace took the id.
        assert_eq!(
            same_workspace(recorded, seen(Some(new), Some("[1] frontend"))),
            Same::No
        );
        assert_eq!(same_workspace(recorded, seen(Some(&[]), None)), Same::No);
        assert_eq!(
            same_workspace(seen(Some(&[]), None), seen(Some(old), None)),
            Same::No
        );
        assert_eq!(
            same_workspace(seen(Some(old), Some("")), seen(Some(new), Some("[2] "))),
            Same::No,
            "an empty label tells nothing"
        );
        assert_eq!(
            same_workspace(seen(Some(old), None), seen(None, Some("work"))),
            Same::No,
            "nothing known on both sides"
        );
        assert_eq!(
            same_workspace(seen(None, Some("work")), seen(None, Some("work"))),
            Same::Label,
            "an entry with a label and no ids matches on the label"
        );
        assert_eq!(
            same_workspace(Seen::default(), seen(Some(old), Some("work"))),
            Same::No,
            "an entry with neither cannot tell"
        );
    }

    #[test]
    fn entries_from_before_terminal_ids_and_the_unknown_marker_still_load() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("migrated.json"),
            serde_json::json!({"tool": TOOL, "schema": 1, "entries": [
                {"direction": "to_app", "source": {"machine": null, "session": "a".repeat(64),
                 "workspace_id": "w1"}, "created": {"space": "s1", "terminals": ["t1"]}, "at": 5},
                {"direction": "to_tui", "source": {"space": "s9"},
                 "created": {"session": "b".repeat(64), "workspace_id": "w7"}, "at": 6}
            ], "partial": []})
            .to_string(),
        )
        .unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let old = ledger.copied_to_app(&source("w1")).unwrap();
        assert_eq!(old.source.terminal_ids, None);
        assert!(!old.created.unknown);
        let (origin, _) = ledger
            .tui_workspace_origin(&"b".repeat(64), "w7", Seen::default())
            .unwrap();
        assert_eq!(origin.created.terminal_ids, None);
        ledger
            .record_partial(Entry::ToApp {
                source: source("w2"),
                created: AppCreated {
                    space: None,
                    terminals: vec![],
                    unknown: true,
                },
                at: 7,
            })
            .unwrap();
        let text = std::fs::read_to_string(root.path().join("migrated.json")).unwrap();
        let file: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            file["entries"][0]["created"],
            serde_json::json!({"space": "s1", "terminals": ["t1"]})
        );
        assert_eq!(file["partial"][0]["created"]["unknown"], true);
        assert_eq!(
            file["partial"][0]["source"]["terminal_ids"],
            serde_json::json!(["term_w2"])
        );
    }

    #[test]
    fn a_foreign_file_is_refused() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("migrated.json"),
            r#"{"tool":"other","schema":1,"entries":[]}"#,
        )
        .unwrap();
        assert!(Ledger::open(root.path()).is_err());
    }
}
