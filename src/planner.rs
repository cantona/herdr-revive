use crate::model::*;
use anyhow::{Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub session: String,
    pub entries: Vec<PlanEntry>,
}
#[derive(Clone, Debug, Serialize)]
pub struct PlanEntry {
    pub pane_id: String,
    pub decision: Decision,
    pub terminal_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Candidate,
    NoCommand,
    MissingPane,
    IdentityMismatch,
    DeniedByPolicy,
}

pub fn index_live(live: &LiveSnapshot) -> Result<BTreeMap<&str, &LivePane>> {
    validate_protocol(&live.version, live.protocol)?;
    let workspaces: HashSet<_> = live.workspaces.iter().map(|w| &w.workspace_id).collect();
    ensure!(
        workspaces.len() == live.workspaces.len(),
        "duplicate live workspace ID"
    );
    let mut tabs = BTreeMap::new();
    for tab in &live.tabs {
        ensure!(workspaces.contains(&tab.workspace_id), "orphan live tab");
        ensure!(
            tabs.insert(&tab.tab_id, &tab.workspace_id).is_none(),
            "duplicate live tab ID"
        );
    }
    let mut panes = BTreeMap::new();
    for pane in &live.panes {
        ensure!(
            tabs.get(&pane.tab_id) == Some(&&pane.workspace_id),
            "orphan live pane"
        );
        ensure!(
            panes.insert(pane.pane_id.as_str(), pane).is_none(),
            "duplicate live pane ID"
        );
    }
    Ok(panes)
}

pub fn plan(saved: &Snapshot, session: &str, live: &LiveSnapshot, config: &Config) -> Result<Plan> {
    saved.validate()?;
    config.validate()?;
    ensure!(
        saved.session == session,
        "cross-session full restore refused"
    );
    let index = index_live(live)?;
    let mut entries = Vec::new();
    for pane in &saved.panes {
        let current = index.get(pane.pane_id.as_str());
        let decision = match (&pane.command, current) {
            (None, _) => Decision::NoCommand,
            (_, None) => Decision::MissingPane,
            (_, Some(live))
                if live.tab_id != pane.tab_id || live.workspace_id != pane.workspace_id =>
            {
                Decision::IdentityMismatch
            }
            (Some(command), _) if command.allowed_argv(config).is_err() => Decision::DeniedByPolicy,
            _ => Decision::Candidate,
        };
        entries.push(PlanEntry {
            pane_id: pane.pane_id.clone(),
            decision,
            terminal_id: current.map(|p| p.terminal_id.clone()),
        });
    }
    entries.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
    Ok(Plan {
        session: session.into(),
        entries,
    })
}
