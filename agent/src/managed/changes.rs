//! Working tree changes, checkpoints and restore for an agent (P6): the one
//! path the CLI (`agent changes`, `agent checkpoint`) and the desk's changes
//! window both use.

use super::{Agent, Manager};
use crate::checkpoint;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What to do with an agent's working tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChangesOp {
    List,
    Diff {
        path: String,
    },
    Checkpoints,
    Make {
        reason: String,
    },
    Show {
        id: u64,
        path: Option<String>,
    },
    /// Without a token, a preview with the token to confirm it.
    Restore {
        id: u64,
        paths: Vec<String>,
        token: Option<String>,
        /// The run the request was made for: refused if the agent has
        /// restarted since.
        run: Option<String>,
    },
    Handoff {
        reviewer: String,
    },
}

const RESTORE_TIME: Duration = Duration::from_secs(120);
const LOCK_WAIT: Duration = Duration::from_secs(5);

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| error.to_string())?
}

impl Manager {
    pub(crate) async fn changes_op(&self, agent: &Agent, op: ChangesOp) -> Result<Value, String> {
        let cwd = PathBuf::from(&agent.cwd);
        let root = blocking(move || crate::changes::root_of(&cwd)).await?;
        let identity = (agent.name.clone(), agent.run.clone());
        match op {
            ChangesOp::List => blocking(move || crate::changes::list(&root)).await,
            ChangesOp::Diff { path } => blocking(move || crate::changes::diff(&root, &path)).await,
            ChangesOp::Checkpoints => {
                blocking(move || {
                    checkpoint::command_in(
                        &root,
                        (&identity.0, &identity.1),
                        checkpoint::Command::List,
                    )
                })
                .await
            }
            ChangesOp::Make { reason } => {
                blocking(move || {
                    checkpoint::command_in(
                        &root,
                        (&identity.0, &identity.1),
                        checkpoint::Command::Make { reason },
                    )
                })
                .await
            }
            ChangesOp::Show { id, path } => {
                blocking(move || {
                    checkpoint::command_in(
                        &root,
                        (&identity.0, &identity.1),
                        checkpoint::Command::Show { id, path },
                    )
                })
                .await
            }
            ChangesOp::Restore {
                id,
                paths,
                token,
                run,
            } => {
                if run.as_ref().is_some_and(|run| *run != agent.run) {
                    return Err(
                        "identity_mismatch: the agent restarted since this was shown; open it again"
                            .into(),
                    );
                }
                self.restore_files(agent, root, id, paths, token).await
            }
            ChangesOp::Handoff { reviewer } => {
                let list = blocking({
                    let root = root.clone();
                    move || crate::changes::list(&root)
                })
                .await?;
                let reviewer = self.get(&reviewer).await?;
                // The item limit counts the attached path too.
                let text = crate::changes::handoff_text(&list, 30 * 1024);
                let root_text = root.to_string_lossy().into_owned();
                let item = self
                    .queue_add(&reviewer, &text, &[root_text], &root)
                    .await?;
                Ok(json!({"stage": "queued", "reviewer": reviewer.name, "item": item}))
            }
        }
    }

    /// Shows `text` (a diff) in `$PAGER` in a popup over the desk's client.
    /// The text goes through an owner-only file the pager's shell removes.
    pub(crate) async fn open_pager(&self, text: String) -> Result<(), String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = std::env::temp_dir().join(format!("masil-diff-{}.txt", super::nonce()?));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        file.write_all(text.as_bytes())
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let path_text = path
            .to_str()
            .ok_or("the temporary directory is not UTF-8")?;
        let mut args = vec!["display-popup", "-E", "-w", "90%", "-h", "90%"];
        if let Some(client) = &self.native.client {
            args.extend(["-c", client.as_str()]);
        }
        args.extend([
            "sh",
            "-c",
            r#"${PAGER:-less} "$1"; rm -f -- "$1""#,
            "masil-pager",
            path_text,
        ]);
        // display-popup returns when the popup closes, so no time limit: it
        // runs in its own task. Once the popup has started, its shell owns
        // the file and removes it.
        let output = tokio::process::Command::new(crate::native_ui::native_executable()?)
            .arg("-u")
            .arg("-S")
            .arg(&self.native.socket)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|error| format!("cannot run masil: {error}"))?;
        if !output.status.success() {
            // The popup did not open: its shell never ran.
            let _ = std::fs::remove_file(&path);
            return Err(format!(
                "the pager popup did not open: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Agents of this server working in `root` that are not idle. Failing
    /// closed: an agent whose state is unknown counts as working.
    async fn busy_in(&self, root: &Path) -> Result<Vec<String>, String> {
        let mut busy = Vec::new();
        for agent in self.list().await? {
            // A directory that cannot be resolved counts by its spelling.
            let inside = match Path::new(&agent.cwd).canonicalize() {
                Ok(cwd) => cwd.starts_with(root),
                Err(_) => Path::new(&agent.cwd).starts_with(root),
            };
            // Anything but an ended or idle agent may be editing.
            if inside && agent.process != "exited" && agent.state != "idle" {
                busy.push(format!("{} ({})", agent.name, agent.state));
            }
        }
        Ok(busy)
    }

    async fn restore_files(
        &self,
        agent: &Agent,
        root: PathBuf,
        id: u64,
        paths: Vec<String>,
        token: Option<String>,
    ) -> Result<Value, String> {
        let refuse_busy = |busy: Vec<String>| -> Result<(), String> {
            if busy.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "checkpoint_agent_working: {} in this working tree {} not idle; restore when no agent works here",
                    busy.join(", "),
                    if busy.len() == 1 { "is" } else { "are" }
                ))
            }
        };
        refuse_busy(self.busy_in(&root).await?)?;
        let deadline = Instant::now() + RESTORE_TIME;
        let identity = (agent.name.clone(), agent.run.clone());
        // Preview, or the files saved as they are now: under the working
        // tree's checkpoint lock, held until the writes are done.
        let prepared = blocking(move || -> Result<checkpoint::Prepared, String> {
            checkpoint::prepare_restore(
                &root,
                id,
                &paths,
                token.as_deref(),
                (&identity.0, &identity.1),
                deadline,
            )
        })
        .await?;
        let prepared = match prepared {
            checkpoint::Prepared::Preview(preview) => return Ok(preview),
            prepared => prepared,
        };
        // No prompt reaches this server's agents while files change.
        let _lock = self.lock_within(LOCK_WAIT).await?;
        refuse_busy(self.busy_in(prepared.root()).await?)?;
        blocking(move || checkpoint::finish_restore(prepared, deadline)).await
    }
}
