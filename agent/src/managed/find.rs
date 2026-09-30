//! Search across managed agents and every pane of the local server.
use super::{Agent, Manager, records};
use crate::finder;
use serde::Serialize;
use serde_json::{Value, json};

const PANE_FORMAT: &str = "#{q:pane_id}\t#{q:session_name}\t#{window_index}\t#{q:window_name}\t#{q:pane_title}\t#{q:pane_current_command}\t#{q:pane_current_path}\t#{window_activity}\t#{q:masil_core_boot_id}\t#{masil_pty_generation}\t#{pane_dead}";
/// Enough for any real workspace; beyond it the search reports truncation.
pub(crate) const MAX_PANES: usize = 2048;

/// A pane the user can jump to, with the identity navigation re-checks.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct PaneTarget {
    pub pane_id: String,
    pub session: String,
    pub window_index: String,
    pub window_name: String,
    pub title: String,
    pub command: String,
    pub path: String,
    pub activity: u64,
    pub boot: String,
    pub generation: String,
    pub dead: bool,
}

impl PaneTarget {
    pub(crate) fn label(&self) -> String {
        format!(
            "{}:{} {}",
            self.session, self.window_index, self.window_name
        )
    }

    /// Pane, command and directory. The title is left out: shells default
    /// it to the host name, which would match almost any query.
    pub(crate) fn detail(&self) -> String {
        join(&[&self.pane_id, &self.command, &home_relative(&self.path)])
    }
}

fn join(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" · ")
}

fn home_relative(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
}

pub(crate) fn agent_label(agent: &Agent) -> String {
    if agent.endpoint_id.is_empty() || agent.endpoint_id == "local" {
        agent.name.clone()
    } else {
        format!("{}::{}", agent.endpoint_id, agent.name)
    }
}

pub(crate) fn agent_detail(agent: &Agent) -> String {
    join(&[
        &agent.provider,
        &agent.workspace,
        &agent.pane_id,
        agent.session_id.as_deref().unwrap_or(""),
    ])
}

impl Manager {
    /// Every pane of this server, most recently active window first.
    pub(crate) async fn pane_targets(&self) -> Result<(Vec<PaneTarget>, bool), String> {
        let output = self
            .command(&["list-panes", "-a", "-F", PANE_FORMAT])
            .await?;
        let rows = records(&output)?;
        let truncated = rows.len() > MAX_PANES;
        let mut seen = std::collections::HashSet::new();
        let mut panes = Vec::new();
        for fields in rows.into_iter().take(MAX_PANES) {
            if fields.len() != 11 || crate::pane_id(&fields[0]).is_err() {
                return Err("invalid native pane list".into());
            }
            // A linked window lists its panes once per session.
            if !seen.insert(fields[0].clone()) {
                continue;
            }
            panes.push(PaneTarget {
                pane_id: fields[0].clone(),
                session: fields[1].clone(),
                window_index: fields[2].clone(),
                window_name: fields[3].clone(),
                title: fields[4].clone(),
                command: fields[5].clone(),
                path: fields[6].clone(),
                activity: fields[7].parse().unwrap_or(0),
                boot: fields[8].clone(),
                generation: fields[9].clone(),
                dead: fields[10] == "1",
            });
        }
        panes.sort_by_key(|pane| std::cmp::Reverse(pane.activity));
        Ok((panes, truncated))
    }

    /// `agent find QUERY [--limit N]`: agents and panes ranked by one matcher.
    pub(crate) async fn find(&self, args: &[String]) -> Result<Value, String> {
        let (query, limit) = match args {
            [query] => (query.as_str(), 20usize),
            [query, flag, limit] if flag == "--limit" => (
                query.as_str(),
                limit
                    .parse()
                    .ok()
                    .filter(|limit| (1..=200).contains(limit))
                    .ok_or("--limit must be 1–200")?,
            ),
            _ => return Err("usage: find QUERY [--limit N]".into()),
        };
        if query.chars().any(char::is_control) || query.len() > 1024 {
            return Err("query must be at most 1024 bytes without control characters".into());
        }
        let agents = self.list().await?;
        let (panes, truncated) = self.pane_targets().await?;
        let mut results = Vec::new();
        for agent in &agents {
            if let Some(score) = finder::score(query, &[&agent_label(agent), &agent_detail(agent)])
            {
                results.push((score, 0, json!({"kind":"agent","id":agent.id,"label":agent_label(agent),
                    "detail":agent_detail(agent),"pane_id":agent.pane_id,"state":agent.state,"score":score})));
            }
        }
        for pane in &panes {
            if agents.iter().any(|agent| agent.pane_id == pane.pane_id) {
                continue;
            }
            if let Some(score) = finder::score(query, &[&pane.label(), &pane.detail()]) {
                results.push((
                    score,
                    1,
                    json!({"kind":"pane","id":pane.pane_id,"label":pane.label(),
                    "detail":pane.detail(),"pane_id":pane.pane_id,"dead":pane.dead,"score":score}),
                ));
            }
        }
        results.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let total = results.len();
        Ok(json!({
            "query": query,
            "total": total,
            "panes_truncated": truncated,
            "results": results.into_iter().take(limit).map(|(_, _, row)| row).collect::<Vec<_>>(),
        }))
    }
}
