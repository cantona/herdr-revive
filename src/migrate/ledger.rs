//! `migrated.json` in the core state directory: which sources a migration
//! copied and what it made, so a later run reports them `unchanged` while
//! the copy exists. A copy that stopped part way is kept under `partial`
//! until that source is copied in full, so the next plan can name it.
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
        let file: File = match std::fs::symlink_metadata(&path) {
            Ok(_) => store::read_json(&path)
                .with_context(|| format!("{} is unreadable", path.display()))?,
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

    pub fn partial_to_app(
        &self,
        source: &AppSource,
    ) -> Option<Recorded<'_, AppSource, AppCreated>> {
        find_app(&self.partial, source)
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

    pub fn record(&mut self, entry: Entry) -> Result<()> {
        self.partial.retain(|old| !old.same_source(&entry));
        self.entries.retain(|old| !old.same_source(&entry));
        // The TUI has just given this id to the new workspace, so a copy
        // recorded under it before is gone.
        if let Entry::ToTui { created, .. } = &entry {
            self.entries.retain(|old| match old {
                Entry::ToTui { created: older, .. } => {
                    older.session != created.session || older.workspace_id != created.workspace_id
                }
                Entry::ToApp { .. } => true,
            });
        }
        self.entries.push(entry);
        self.save()
    }

    pub fn record_partial(&mut self, entry: Entry) -> Result<()> {
        self.partial.retain(|old| !old.same_source(&entry));
        self.partial.push(entry);
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
    fn a_recorded_copy_survives_reopening_and_replaces_its_partial() {
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
        assert_eq!(ledger.partial_to_app(&source("w1")), None);
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
        let spaces: Vec<&str> = ledger
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::ToTui { source, .. } => Some(source.space.as_str()),
                Entry::ToApp { .. } => None,
            })
            .collect();
        assert_eq!(spaces, ["s2", "s3", "s9"], "only s1 had (a, w1)");
        assert!(
            ledger.app_space_origin("s5").is_some(),
            "to_app entries stay"
        );
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
