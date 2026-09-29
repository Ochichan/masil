use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;

const COMMAND_DEADLINE: Duration = Duration::from_secs(3);
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_SCRIPT: usize = 64 * 1024;
const MAX_COPY: usize = 64 * 1024;
const SIDEBAR_WIDTH: &str = "34";
const MIN_WINDOW_WIDTH: u32 = 100;
const REJECTED: &str = "rmux-native-rejected";
const MARKED: &str = "rmux-native-marked";
const COPIED: &str = "rmux-native-copied";
const FOCUSED: &str = "rmux-native-focused";
const FOCUSED_RESTORED: &str = "rmux-native-focused-restored";
const OPT_OWNED: &str = "@rmux-sidebar-owned";
const OPT_BOOT: &str = "@rmux-sidebar-core-boot";
const OPT_MANAGER: &str = "@rmux-sidebar-manager";
const OPT_GENERATION: &str = "@rmux-sidebar-pty-generation";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const CLIENT_FORMAT: &str =
    "#{client_name}\t#{client_readonly}\t#{client_control_mode}\t#{pane_id}\t#{window_id}";
const TARGET_FORMAT: &str = "#{rmux_core_boot_id}\t#{rmux_pty_generation}\t#{pane_id}\t#{window_id}\t#{window_width}\t#{window_zoomed_flag}\t#{pane_zoomed_flag}\t#{window_modal_pane}\t#{window_active_clients}\t#{pane_dead}";
const OWNED_FORMAT: &str = "#{rmux_core_boot_id}\t#{rmux_pty_generation}\t#{pane_id}\t#{window_id}\t#{window_zoomed_flag}\t#{pane_zoomed_flag}\t#{window_modal_pane}\t#{window_active_clients}\t#{pane_dead}\t#{@rmux-sidebar-owned}\t#{@rmux-sidebar-core-boot}\t#{@rmux-sidebar-manager}\t#{@rmux-sidebar-pty-generation}";

#[derive(Clone, Debug)]
pub struct Context {
    pub socket: PathBuf,
    pub client: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SidebarOutcome {
    Created {
        pane_id: String,
        pty_generation: String,
    },
    Reused {
        pane_id: String,
        pty_generation: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZoomOutcome {
    Expanded,
    Restored,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationOutcome {
    Focused,
    FocusedAfterRestoringSidebar,
}

#[derive(Debug)]
struct ProcessOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug)]
struct ClientInfo {
    name: String,
    pane_id: String,
}

#[derive(Debug)]
struct TargetInfo {
    boot: String,
    generation: String,
    pane_id: String,
    window_id: String,
    width: u32,
    window_zoomed: bool,
    modal: bool,
    active_clients: u32,
    dead: bool,
}

#[derive(Debug)]
struct OwnedInfo {
    generation: String,
    window_zoomed: bool,
    pane_zoomed: bool,
    modal: bool,
    active_clients: u32,
    dead: bool,
}

struct NativeLock {
    _file: File,
}

struct TempFile {
    path: PathBuf,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Context {
    pub async fn sidebar(
        &self,
        manager_socket: &Path,
        executable: &Path,
        target: Option<&str>,
        language: Option<&str>,
        theme: Option<&str>,
    ) -> Result<SidebarOutcome, String> {
        validate_optional_label(language, "language")?;
        validate_optional_label(theme, "theme")?;
        let expected_boot = manager_boot_id(executable, manager_socket).await?;
        let client = self.resolve_client().await?;
        let requested = target.unwrap_or(&client.pane_id);
        let initial = self.target_info(requested).await?;
        require_matching_boot(&initial, &expected_boot)?;
        let _lock = self.native_lock(&initial.window_id)?;

        // Resolve again under the per-window lock so two helpers cannot both
        // decide that the window has no sidebar.
        let target = self.target_info(&initial.pane_id).await?;
        require_matching_boot(&target, &expected_boot)?;
        let manager_marker = marker_for_path(manager_socket);
        if let Some((pane_id, generation)) = self
            .find_reusable_sidebar(&target.window_id, &expected_boot, &manager_marker)
            .await?
        {
            return Ok(SidebarOutcome::Reused {
                pane_id,
                pty_generation: generation,
            });
        }
        if target.width < MIN_WINDOW_WIDTH {
            return Err(format!(
                "native sidebar needs a window at least {MIN_WINDOW_WIDTH} columns wide"
            ));
        }
        if target.window_zoomed {
            return Err("native sidebar is unavailable while the window is zoomed".into());
        }
        if target.modal {
            return Err("native sidebar is unavailable while the window has a modal pane".into());
        }
        if target.active_clients > 1 {
            return Err("native sidebar is unavailable in a shared active window".into());
        }
        if target.dead {
            return Err("native sidebar target pane is dead".into());
        }

        let guard = and(
            core_guard(&expected_boot, &target.generation),
            and(
                client_scope_guard(&client.name),
                and(
                    "#{==:0,#{window_zoomed_flag}}".into(),
                    and(
                        "#{==:0,#{pane_dead}}".into(),
                        format!("#{{e|>=:#{{window_width}},{MIN_WINDOW_WIDTH}}}"),
                    ),
                ),
            ),
        );
        let mut command = vec![
            "split-window".to_owned(),
            "-h".to_owned(),
            "-f".to_owned(),
            "-b".to_owned(),
            "-l".to_owned(),
            SIDEBAR_WIDTH.to_owned(),
            "-d".to_owned(),
            "-P".to_owned(),
            "-F".to_owned(),
            "#{pane_id}\t#{rmux_pty_generation}".to_owned(),
            "-t".to_owned(),
            target.pane_id.clone(),
            "--".to_owned(),
            path_text(executable, "executable")?.to_owned(),
            "--socket".to_owned(),
            path_text(manager_socket, "manager socket")?.to_owned(),
            "ui".to_owned(),
            "--compact".to_owned(),
            "--core-native".to_owned(),
            path_text(&self.socket, "native socket")?.to_owned(),
            "--client".to_owned(),
            client.name.clone(),
        ];
        if let Some(language) = language {
            command.extend(["--lang".to_owned(), language.to_owned()]);
        }
        if let Some(theme) = theme {
            command.extend(["--theme".to_owned(), theme.to_owned()]);
        }
        let output = self
            .guarded_script(&target.pane_id, &guard, &[command], REJECTED)
            .await?;
        let line = output_line(&output)?;
        if line == REJECTED {
            return Err("native sidebar target changed before creation".into());
        }
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 2 || !valid_pane_id(fields[0]) || !valid_generation(fields[1]) {
            if fields.first().is_some_and(|pane| valid_pane_id(pane)) {
                return Err(format!(
                    "native sidebar {} was created but returned no safe PTY identity; it was not removed",
                    fields[0]
                ));
            }
            return Err("native sidebar returned an invalid pane identity".into());
        }
        let pane_id = fields[0].to_owned();
        let generation = fields[1].to_owned();

        let marker_guard = and(
            core_guard(&expected_boot, &generation),
            and(
                client_scope_guard(&client.name),
                format!("#{{==:{},#{{window_id}}}}", target.window_id),
            ),
        );
        let marker_commands = vec![
            vec![
                "set-option".into(),
                "-p".into(),
                "-t".into(),
                pane_id.clone(),
                "remain-on-exit".into(),
                "off".into(),
            ],
            set_marker(&pane_id, OPT_OWNED, "1"),
            set_marker(&pane_id, OPT_BOOT, &expected_boot),
            set_marker(&pane_id, OPT_MANAGER, &manager_marker),
            set_marker(&pane_id, OPT_GENERATION, &generation),
            vec!["display-message".into(), "-p".into(), MARKED.into()],
        ];
        let marked = self
            .guarded_script(&pane_id, &marker_guard, &marker_commands, REJECTED)
            .await
            .map_err(|error| {
                format!(
                    "native sidebar {pane_id} could not be marked ({error}); it was not removed"
                )
            })?;
        if output_line(&marked)? != MARKED {
            return Err(format!(
                "native sidebar {pane_id} changed before ownership markers were installed"
            ));
        }
        let owned = self
            .owned_info(&expected_boot, manager_socket, &pane_id)
            .await?;
        if owned.dead {
            self.cleanup_dead_sidebar(
                &expected_boot,
                &manager_marker,
                &pane_id,
                &generation,
                &target.window_id,
                &client.name,
            )
            .await?;
            return Err(format!(
                "native sidebar {pane_id} exited during creation and was removed"
            ));
        }
        Ok(SidebarOutcome::Created {
            pane_id,
            pty_generation: generation,
        })
    }

    pub async fn navigate(
        &self,
        expected_core_boot_id: &str,
        pane_id: &str,
        expected_pty_generation: &str,
        originating_owned_ui_pane: Option<&str>,
        manager_socket: &Path,
    ) -> Result<NavigationOutcome, String> {
        require_boot(expected_core_boot_id)?;
        require_pane(pane_id)?;
        require_generation(expected_pty_generation)?;
        let client = self.resolve_client().await?;
        let target = self.target_info(pane_id).await?;
        if target.boot != expected_core_boot_id || target.generation != expected_pty_generation {
            return Err("native pane identity is stale".into());
        }
        if target.dead || target.modal || target.active_clients > 1 {
            return Err("native pane cannot be focused in the current window state".into());
        }
        let target_guard = and(
            core_guard(expected_core_boot_id, expected_pty_generation),
            and(
                client_scope_guard(&client.name),
                "#{==:0,#{pane_dead}}".into(),
            ),
        );
        let switch = vec![
            "switch-client".into(),
            "-E".into(),
            "-c".into(),
            client.name.clone(),
            "-t".into(),
            pane_id.into(),
        ];

        let (commands, expected) = if let Some(origin) = originating_owned_ui_pane {
            require_pane(origin)?;
            if origin == pane_id {
                return Err("native sidebar cannot navigate to itself".into());
            }
            let owned = self
                .owned_info(expected_core_boot_id, manager_socket, origin)
                .await?;
            if owned.window_zoomed && !owned.pane_zoomed {
                return Err("refusing to restore a zoom owned by another pane".into());
            }
            if owned.pane_zoomed {
                let origin_guard = and(
                    owned_guard(
                        expected_core_boot_id,
                        &owned.generation,
                        &marker_for_path(manager_socket),
                    ),
                    and(
                        client_scope_guard(&client.name),
                        "#{==:1,#{pane_zoomed_flag}}".into(),
                    ),
                );
                let mut nested = String::new();
                write_if(
                    &mut nested,
                    origin,
                    &origin_guard,
                    &[
                        vec![
                            "resize-pane".into(),
                            "-Z".into(),
                            "-t".into(),
                            origin.into(),
                        ],
                        switch,
                        vec![
                            "display-message".into(),
                            "-p".into(),
                            FOCUSED_RESTORED.into(),
                        ],
                    ],
                    REJECTED,
                );
                (
                    vec![vec!["source-file".into(), "-".into(), nested]],
                    FOCUSED_RESTORED,
                )
            } else {
                (
                    vec![
                        switch,
                        vec!["display-message".into(), "-p".into(), FOCUSED.into()],
                    ],
                    FOCUSED,
                )
            }
        } else {
            (
                vec![
                    switch,
                    vec!["display-message".into(), "-p".into(), FOCUSED.into()],
                ],
                FOCUSED,
            )
        };

        let output = if commands.len() == 1
            && commands[0].first().map(String::as_str) == Some("source-file")
        {
            // The third element is a prebuilt nested command list. Insert it
            // directly so both unzoom and switch remain in the outer guard's
            // synchronous queue.
            let nested = &commands[0][2];
            let mut script = String::new();
            let mut branch = String::new();
            branch.push_str(nested);
            write!(
                script,
                "if-shell -F -t {} {} {{\n{}\n}} {{ display-message -p {}; }}\n",
                tmux_quote(pane_id),
                tmux_quote(&target_guard),
                branch,
                tmux_quote(REJECTED)
            )
            .unwrap();
            self.source_script(script).await?
        } else {
            self.guarded_script(pane_id, &target_guard, &commands, REJECTED)
                .await?
        };
        match output_line(&output)? {
            value if value == expected => Ok(if value == FOCUSED_RESTORED {
                NavigationOutcome::FocusedAfterRestoringSidebar
            } else {
                NavigationOutcome::Focused
            }),
            _ => Err("native focus guard rejected changed state".into()),
        }
    }

    pub async fn toggle_zoom(
        &self,
        expected_core_boot_id: &str,
        owned_pane: &str,
        manager_socket: &Path,
    ) -> Result<ZoomOutcome, String> {
        require_boot(expected_core_boot_id)?;
        require_pane(owned_pane)?;
        let client = self.resolve_client().await?;
        let owned = self
            .owned_info(expected_core_boot_id, manager_socket, owned_pane)
            .await?;
        if owned.dead || owned.modal || owned.active_clients > 1 {
            return Err("native sidebar cannot change zoom in the current window state".into());
        }
        if owned.window_zoomed && !owned.pane_zoomed {
            return Err("refusing to restore a zoom owned by another pane".into());
        }
        let (state_guard, marker, outcome) = if owned.pane_zoomed {
            (
                and(
                    "#{==:1,#{window_zoomed_flag}}".into(),
                    "#{==:1,#{pane_zoomed_flag}}".into(),
                ),
                "rmux-native-restored",
                ZoomOutcome::Restored,
            )
        } else {
            (
                and(
                    "#{==:0,#{window_zoomed_flag}}".into(),
                    "#{==:0,#{pane_zoomed_flag}}".into(),
                ),
                "rmux-native-expanded",
                ZoomOutcome::Expanded,
            )
        };
        let guard = and(
            owned_guard(
                expected_core_boot_id,
                &owned.generation,
                &marker_for_path(manager_socket),
            ),
            and(client_scope_guard(&client.name), state_guard),
        );
        let commands = vec![
            vec![
                "resize-pane".into(),
                "-Z".into(),
                "-t".into(),
                owned_pane.into(),
            ],
            vec!["display-message".into(), "-p".into(), marker.into()],
        ];
        let output = self
            .guarded_script(owned_pane, &guard, &commands, REJECTED)
            .await?;
        if output_line(&output)? == marker {
            Ok(outcome)
        } else {
            Err("native zoom guard rejected changed state".into())
        }
    }

    pub async fn copy_to_buffer(
        &self,
        expected_core_boot_id: &str,
        value: &str,
    ) -> Result<(), String> {
        require_boot(expected_core_boot_id)?;
        if value.is_empty() || value.len() > MAX_COPY || value.contains('\0') {
            return Err("tmux buffer value must contain 1 to 65536 bytes".into());
        }
        let client = self.resolve_client().await?;
        let pane = self.target_info(&client.pane_id).await?;
        require_matching_boot(&pane, expected_core_boot_id)?;
        if pane.dead || pane.modal || pane.active_clients > 1 {
            return Err("tmux buffer is unavailable in the current window state".into());
        }
        let temporary = self.private_temp(value.as_bytes())?;
        let guard = and(
            core_guard(expected_core_boot_id, &pane.generation),
            and(
                client_scope_guard(&client.name),
                "#{==:0,#{pane_dead}}".into(),
            ),
        );
        let commands = vec![
            vec![
                "load-buffer".into(),
                "-b".into(),
                "rmux-agent-id".into(),
                path_text(&temporary.path, "temporary buffer")?.into(),
            ],
            vec!["display-message".into(), "-p".into(), COPIED.into()],
        ];
        let output = self
            .guarded_script(&pane.pane_id, &guard, &commands, REJECTED)
            .await?;
        if output_line(&output)? == COPIED {
            Ok(())
        } else {
            Err("tmux buffer guard rejected changed state".into())
        }
    }

    async fn cleanup_dead_sidebar(
        &self,
        expected_boot: &str,
        manager_marker: &str,
        pane_id: &str,
        generation: &str,
        window_id: &str,
        client: &str,
    ) -> Result<(), String> {
        let guard = and(
            owned_guard(expected_boot, generation, manager_marker),
            and(
                client_scope_guard(client),
                and(
                    format!("#{{==:{window_id},#{{window_id}}}}"),
                    "#{==:1,#{pane_dead}}".into(),
                ),
            ),
        );
        let cleaned = "rmux-native-dead-cleaned";
        let commands = vec![
            vec!["display-message".into(), "-p".into(), cleaned.into()],
            vec!["kill-pane".into(), "-t".into(), pane_id.into()],
        ];
        let output = self
            .guarded_script(pane_id, &guard, &commands, REJECTED)
            .await?;
        if output_line(&output)? == cleaned {
            Ok(())
        } else {
            Err(format!(
                "dead native sidebar {pane_id} changed before guarded cleanup"
            ))
        }
    }

    async fn resolve_client(&self) -> Result<ClientInfo, String> {
        let name = if let Some(name) = self.client.as_deref() {
            require_client_name(name)?;
            name.to_owned()
        } else {
            let output = self
                .tmux(
                    [
                        OsString::from("list-clients"),
                        OsString::from("-F"),
                        OsString::from(CLIENT_FORMAT),
                    ],
                    None,
                )
                .await?;
            let text = utf8_stdout(&output)?;
            let mut eligible = Vec::new();
            for line in text.lines() {
                let fields: Vec<_> = line.split('\t').collect();
                if fields.len() != 5 {
                    return Err("native client list contains an unsafe client name".into());
                }
                if fields[1] == "0" && fields[2] == "0" {
                    require_client_name(fields[0])?;
                    eligible.push(fields[0].to_owned());
                }
            }
            if eligible.len() != 1 {
                return Err("native action needs one explicit eligible client".into());
            }
            eligible.pop().unwrap()
        };
        let output = self
            .tmux(
                [
                    OsString::from("display-message"),
                    OsString::from("-p"),
                    OsString::from("-c"),
                    OsString::from(&name),
                    OsString::from("-F"),
                    OsString::from(CLIENT_FORMAT),
                ],
                None,
            )
            .await?;
        let line = output_line(&output)?;
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 5
            || fields[0] != name
            || fields[1] != "0"
            || fields[2] != "0"
            || !valid_pane_id(fields[3])
            || !valid_window_id(fields[4])
        {
            return Err("native client is detached, read-only, or ineligible".into());
        }
        Ok(ClientInfo {
            name,
            pane_id: fields[3].to_owned(),
        })
    }

    async fn target_info(&self, target: &str) -> Result<TargetInfo, String> {
        if target.is_empty() || target.contains(['\0', '\r', '\n']) {
            return Err("invalid native pane target".into());
        }
        let output = self
            .tmux(
                [
                    OsString::from("display-message"),
                    OsString::from("-p"),
                    OsString::from("-t"),
                    OsString::from(target),
                    OsString::from("-F"),
                    OsString::from(TARGET_FORMAT),
                ],
                None,
            )
            .await?;
        parse_target(output_line(&output)?)
    }

    async fn owned_info(
        &self,
        expected_boot: &str,
        manager_socket: &Path,
        pane_id: &str,
    ) -> Result<OwnedInfo, String> {
        require_boot(expected_boot)?;
        require_pane(pane_id)?;
        let output = self
            .tmux(
                [
                    OsString::from("display-message"),
                    OsString::from("-p"),
                    OsString::from("-t"),
                    OsString::from(pane_id),
                    OsString::from("-F"),
                    OsString::from(OWNED_FORMAT),
                ],
                None,
            )
            .await?;
        parse_owned(
            output_line(&output)?,
            expected_boot,
            &marker_for_path(manager_socket),
        )
    }

    async fn find_reusable_sidebar(
        &self,
        window_id: &str,
        expected_boot: &str,
        manager_marker: &str,
    ) -> Result<Option<(String, String)>, String> {
        require_window(window_id)?;
        let filter = and(
            format!("#{{==:1,#{{{OPT_OWNED}}}}}"),
            and(
                format!("#{{==:{expected_boot},#{{{OPT_BOOT}}}}}"),
                format!("#{{==:{manager_marker},#{{{OPT_MANAGER}}}}}"),
            ),
        );
        let format = format!(
            "#{{pane_id}}\t#{{rmux_pty_generation}}\t#{{{OPT_GENERATION}}}\t#{{pane_dead}}"
        );
        let output = self
            .tmux(
                [
                    OsString::from("list-panes"),
                    OsString::from("-t"),
                    OsString::from(window_id),
                    OsString::from("-f"),
                    OsString::from(filter),
                    OsString::from("-F"),
                    OsString::from(format),
                ],
                None,
            )
            .await?;
        let text = utf8_stdout(&output)?;
        let mut matches = Vec::new();
        for line in text.lines() {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 4
                || !valid_pane_id(fields[0])
                || !valid_generation(fields[1])
                || !valid_generation(fields[2])
                || !matches!(fields[3], "0" | "1")
            {
                return Err("native sidebar marker output is invalid".into());
            }
            if fields[1] == fields[2] && fields[3] == "0" {
                matches.push((fields[0].to_owned(), fields[1].to_owned()));
            }
        }
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.pop()),
            _ => Err("multiple live native sidebars claim this manager".into()),
        }
    }

    async fn guarded_script(
        &self,
        target: &str,
        guard: &str,
        commands: &[Vec<String>],
        rejected: &str,
    ) -> Result<ProcessOutput, String> {
        let mut script = String::new();
        write_if(&mut script, target, guard, commands, rejected);
        self.source_script(script).await
    }

    async fn source_script(&self, script: String) -> Result<ProcessOutput, String> {
        if script.len() > MAX_SCRIPT {
            return Err("native tmux command exceeds script limit".into());
        }
        self.tmux(
            [OsString::from("source-file"), OsString::from("-")],
            Some(script.into_bytes()),
        )
        .await
    }

    async fn tmux<I>(&self, args: I, input: Option<Vec<u8>>) -> Result<ProcessOutput, String>
    where
        I: IntoIterator<Item = OsString>,
    {
        let executable = native_executable()?;
        let mut all = vec![OsString::from("-S"), self.socket.as_os_str().to_owned()];
        all.extend(args);
        run_process(executable.as_os_str(), all, input).await
    }

    fn native_lock(&self, window_id: &str) -> Result<NativeLock, String> {
        require_window(window_id)?;
        let directory = private_parent(&self.socket)?;
        let path = directory.join(format!(
            ".rmux-sidebar-{}.lock",
            window_id.trim_start_matches('@')
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|error| format!("native sidebar lock: {error}"))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("native sidebar lock: {error}"))?;
        if !metadata.file_type().is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err("native sidebar lock is not a private owner file".into());
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("another native sidebar operation is in progress".into());
        }
        Ok(NativeLock { _file: file })
    }

    fn private_temp(&self, value: &[u8]) -> Result<TempFile, String> {
        let directory = private_parent(&self.socket)?;
        for _ in 0..32 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(".rmux-buffer-{}-{sequence}", std::process::id()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(&path)
            {
                Ok(mut file) => {
                    file.write_all(value)
                        .and_then(|()| file.sync_all())
                        .map_err(|error| format!("temporary tmux buffer: {error}"))?;
                    return Ok(TempFile { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("temporary tmux buffer: {error}")),
            }
        }
        Err("could not allocate a private tmux buffer file".into())
    }
}

use std::os::fd::AsRawFd;

async fn manager_boot_id(executable: &Path, socket: &Path) -> Result<String, String> {
    let output = run_process(
        executable.as_os_str(),
        [
            OsString::from("--socket"),
            socket.as_os_str().to_owned(),
            OsString::from("status"),
        ],
        None,
    )
    .await?;
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "manager status is not valid JSON".to_string())?;
    if value.get("v").and_then(Value::as_u64) != Some(1)
        || value.get("kind").and_then(Value::as_str) != Some("status")
        || value.get("status").and_then(Value::as_str) != Some("running")
        || value.pointer("/core/freshness").and_then(Value::as_str) != Some("fresh")
    {
        return Err("manager does not have fresh native core identity".into());
    }
    let boot = value
        .pointer("/core/core_boot_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "manager status has no native core boot ID".to_string())?;
    require_boot(boot)?;
    Ok(boot.to_owned())
}

async fn run_process<I>(
    program: &OsStr,
    args: I,
    input: Option<Vec<u8>>,
) -> Result<ProcessOutput, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut child = Command::new(program)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("starting native command: {error}"))?;
    let stdout = child.stdout.take().ok_or("native command has no stdout")?;
    let stderr = child.stderr.take().ok_or("native command has no stderr")?;
    let mut stdin = child.stdin.take();
    let joined = timeout(COMMAND_DEADLINE, async move {
        tokio::join!(
            read_limited(stdout),
            read_limited(stderr),
            async move {
                if let Some(bytes) = input {
                    let pipe = stdin.as_mut().ok_or("native command has no stdin")?;
                    pipe.write_all(&bytes)
                        .await
                        .map_err(|error| format!("writing native command: {error}"))?;
                    pipe.shutdown()
                        .await
                        .map_err(|error| format!("closing native command input: {error}"))?;
                }
                Ok::<(), String>(())
            },
            child.wait()
        )
    })
    .await
    .map_err(|_| "native command timed out".to_string())?;
    let stdout = joined.0?;
    let stderr = joined.1?;
    joined.2?;
    let status = joined
        .3
        .map_err(|error| format!("waiting for native command: {error}"))?;
    if !status.success() || !stderr.is_empty() {
        let message = String::from_utf8_lossy(&stderr);
        return Err(if message.trim().is_empty() {
            format!("native command exited with {status}")
        } else {
            format!("native command failed: {}", message.trim())
        });
    }
    Ok(ProcessOutput { stdout, stderr })
}

async fn read_limited<R>(reader: R) -> Result<Vec<u8>, String>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take((MAX_OUTPUT + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("reading native command: {error}"))?;
    if bytes.len() > MAX_OUTPUT {
        return Err("native command output exceeds limit".into());
    }
    Ok(bytes)
}

pub(crate) fn native_executable() -> Result<PathBuf, String> {
    let current =
        std::env::current_exe().map_err(|error| format!("locating native executable: {error}"))?;
    let directory = current
        .parent()
        .ok_or_else(|| "current executable has no parent directory".to_string())?;
    Ok(directory.join("rmux"))
}

fn private_parent(socket: &Path) -> Result<PathBuf, String> {
    let directory = socket
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| "native socket must have a private parent directory".to_string())?;
    let metadata =
        std::fs::metadata(directory).map_err(|error| format!("native socket parent: {error}"))?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err("native socket parent must be a private owner directory".into());
    }
    Ok(directory.to_owned())
}

fn write_if(
    script: &mut String,
    target: &str,
    guard: &str,
    commands: &[Vec<String>],
    rejected: &str,
) {
    writeln!(
        script,
        "if-shell -F -t {} {} {{",
        tmux_quote(target),
        tmux_quote(guard)
    )
    .unwrap();
    for command in commands {
        write_command(script, command);
    }
    writeln!(
        script,
        "}} {{ display-message -p {}; }}",
        tmux_quote(rejected)
    )
    .unwrap();
}

fn write_command(script: &mut String, command: &[String]) {
    for (index, argument) in command.iter().enumerate() {
        if index != 0 {
            script.push(' ');
        }
        script.push_str(&tmux_quote(argument));
    }
    script.push('\n');
}

fn tmux_quote(value: &str) -> String {
    let mut quoted = String::from("'");
    for character in value.chars() {
        match character {
            '\'' => quoted.push_str("'\\''"),
            '\n' => quoted.push_str("'\\012'"),
            _ => quoted.push(character),
        }
    }
    quoted.push('\'');
    quoted
}

fn and(left: String, right: String) -> String {
    format!("#{{&&:{left},{right}}}")
}

fn core_guard(boot: &str, generation: &str) -> String {
    and(
        format!("#{{==:{boot},#{{rmux_core_boot_id}}}}"),
        and(
            format!("#{{==:{generation},#{{rmux_pty_generation}}}}"),
            and(
                "#{<=:#{window_active_clients},1}".into(),
                "#{==:,#{window_modal_pane}}".into(),
            ),
        ),
    )
}

fn client_guard(client: &str) -> String {
    let eligible = and(
        format!("#{{==:{client},#{{client_name}}}}"),
        and(
            "#{==:0,#{client_readonly}}".into(),
            and(
                "#{==:0,#{client_control_mode}}".into(),
                "#{!=:,#{client_session}}".into(),
            ),
        ),
    );
    format!("#{{==:x,#{{L:#{{?{eligible},x,}}}}}}")
}

fn client_scope_guard(client: &str) -> String {
    and(
        client_guard(client),
        and(
            format!(
                "#{{||:#{{==:0,#{{window_active_clients}}}},#{{==:{client},#{{window_active_clients_list}}}}}}"
            ),
            // switch-client also changes the target session's current window.
            // A hidden target window can have zero viewers while the session
            // still has other clients that would be moved by that operation.
            format!(
                "#{{||:#{{==:0,#{{session_attached}}}},#{{==:{client},#{{session_attached_list}}}}}}"
            ),
        ),
    )
}

fn owned_guard(boot: &str, generation: &str, manager_marker: &str) -> String {
    and(
        core_guard(boot, generation),
        and(
            format!("#{{==:1,#{{{OPT_OWNED}}}}}"),
            and(
                format!("#{{==:{boot},#{{{OPT_BOOT}}}}}"),
                and(
                    format!("#{{==:{manager_marker},#{{{OPT_MANAGER}}}}}"),
                    format!("#{{==:{generation},#{{{OPT_GENERATION}}}}}"),
                ),
            ),
        ),
    )
}

fn set_marker(pane_id: &str, option: &str, value: &str) -> Vec<String> {
    vec![
        "set-option".into(),
        "-p".into(),
        "-t".into(),
        pane_id.into(),
        option.into(),
        value.into(),
    ]
}

fn parse_target(line: &str) -> Result<TargetInfo, String> {
    let fields: Vec<_> = line.split('\t').collect();
    if fields.len() != 10
        || !valid_boot_id(fields[0])
        || !valid_generation(fields[1])
        || !valid_pane_id(fields[2])
        || !valid_window_id(fields[3])
        || !matches!(fields[5], "0" | "1")
        || !matches!(fields[6], "0" | "1")
        || !(fields[7].is_empty() || valid_pane_id(fields[7]))
        || !matches!(fields[9], "0" | "1")
    {
        return Err("native target identity is invalid".into());
    }
    let width = fields[4]
        .parse::<u32>()
        .map_err(|_| "native target width is invalid")?;
    let active_clients = fields[8]
        .parse::<u32>()
        .map_err(|_| "native active client count is invalid")?;
    Ok(TargetInfo {
        boot: fields[0].into(),
        generation: fields[1].into(),
        pane_id: fields[2].into(),
        window_id: fields[3].into(),
        width,
        window_zoomed: fields[5] == "1",
        modal: !fields[7].is_empty(),
        active_clients,
        dead: fields[9] == "1",
    })
}

fn parse_owned(line: &str, boot: &str, manager_marker: &str) -> Result<OwnedInfo, String> {
    let fields: Vec<_> = line.split('\t').collect();
    if fields.len() != 13
        || fields[0] != boot
        || !valid_generation(fields[1])
        || !valid_pane_id(fields[2])
        || !valid_window_id(fields[3])
        || !matches!(fields[4], "0" | "1")
        || !matches!(fields[5], "0" | "1")
        || !(fields[6].is_empty() || valid_pane_id(fields[6]))
        || !matches!(fields[8], "0" | "1")
        || fields[9] != "1"
        || fields[10] != boot
        || fields[11] != manager_marker
        || fields[12] != fields[1]
    {
        return Err("pane is not the live sidebar owned by this manager".into());
    }
    let active_clients = fields[7]
        .parse::<u32>()
        .map_err(|_| "native active client count is invalid")?;
    Ok(OwnedInfo {
        generation: fields[1].into(),
        window_zoomed: fields[4] == "1",
        pane_zoomed: fields[5] == "1",
        modal: !fields[6].is_empty(),
        active_clients,
        dead: fields[8] == "1",
    })
}

fn output_line(output: &ProcessOutput) -> Result<&str, String> {
    let text = utf8_stdout(output)?;
    let line = text.strip_suffix('\n').unwrap_or(text);
    if line.is_empty() || line.contains(['\n', '\r', '\0']) {
        return Err("native command returned an invalid response".into());
    }
    Ok(line)
}

fn utf8_stdout(output: &ProcessOutput) -> Result<&str, String> {
    if !output.stderr.is_empty() {
        return Err("native command wrote unexpected stderr".into());
    }
    std::str::from_utf8(&output.stdout)
        .map_err(|_| "native command returned non-UTF-8 output".into())
}

fn require_matching_boot(target: &TargetInfo, expected: &str) -> Result<(), String> {
    require_boot(expected)?;
    if target.boot == expected {
        Ok(())
    } else {
        Err("manager and native core boot identities do not match".into())
    }
}

fn require_boot(value: &str) -> Result<(), String> {
    if valid_boot_id(value) {
        Ok(())
    } else {
        Err("invalid native core boot ID".into())
    }
}

fn require_generation(value: &str) -> Result<(), String> {
    if valid_generation(value) {
        Ok(())
    } else {
        Err("invalid PTY generation".into())
    }
}

fn require_pane(value: &str) -> Result<(), String> {
    if valid_pane_id(value) {
        Ok(())
    } else {
        Err("invalid pane ID".into())
    }
}

fn require_window(value: &str) -> Result<(), String> {
    if valid_window_id(value) {
        Ok(())
    } else {
        Err("invalid window ID".into())
    }
}

fn require_client_name(value: &str) -> Result<(), String> {
    if !value.is_empty()
        && value.len() <= 1024
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':')
        })
    {
        Ok(())
    } else {
        Err("native client name contains unsupported characters".into())
    }
}

fn valid_boot_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn valid_generation(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 20
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u64>().is_ok()
}

fn valid_pane_id(value: &str) -> bool {
    valid_prefixed_decimal(value, '%')
}

fn valid_window_id(value: &str) -> bool {
    valid_prefixed_decimal(value, '@')
}

fn valid_prefixed_decimal(value: &str, prefix: char) -> bool {
    let Some(number) = value.strip_prefix(prefix) else {
        return false;
    };
    !number.is_empty()
        && number.len() <= 10
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && number.parse::<u32>().is_ok()
}

fn validate_optional_label(value: Option<&str>, field: &str) -> Result<(), String> {
    if value.is_none_or(|value| {
        !value.is_empty()
            && value.len() <= 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    }) {
        Ok(())
    } else {
        Err(format!("invalid {field}"))
    }
}

fn marker_for_path(path: &Path) -> String {
    let bytes = path.as_os_str().as_bytes();
    let mut marker = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(marker, "{byte:02x}").unwrap();
    }
    marker
}

fn path_text<'a>(path: &'a Path, field: &str) -> Result<&'a str, String> {
    path.to_str()
        .filter(|value| !value.contains('\0'))
        .ok_or_else(|| format!("{field} path is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOT: &str = "01234567-89ab-4def-8123-456789abcdef";

    #[test]
    fn tmux_quoting_preserves_parser_boundaries() {
        assert_eq!(tmux_quote(""), "''");
        assert_eq!(tmux_quote("a b;#{x}$HOME"), "'a b;#{x}$HOME'");
        assert_eq!(tmux_quote("a'b"), "'a'\\''b'");
        assert_eq!(tmux_quote("a\nb"), "'a'\\012'b'");
    }

    #[test]
    fn core_guard_is_nested_and_checks_all_boundary_evidence() {
        let guard = core_guard(BOOT, "42");
        assert_eq!(guard.matches("#{&&:").count(), 3);
        assert!(guard.contains("#{==:01234567-89ab-4def-8123-456789abcdef,#{rmux_core_boot_id}}"));
        assert!(guard.contains("#{==:42,#{rmux_pty_generation}}"));
        assert!(guard.contains("#{<=:#{window_active_clients},1}"));
        assert!(guard.contains("#{==:,#{window_modal_pane}}"));
    }

    #[test]
    fn client_guard_rechecks_readonly_state_in_the_command_queue() {
        let guard = client_scope_guard("/dev/ttys001");
        assert!(guard.contains("#{L:"));
        assert!(guard.contains("#{==:/dev/ttys001,#{client_name}}"));
        assert!(guard.contains("#{==:0,#{client_readonly}}"));
        assert!(guard.contains("#{==:0,#{client_control_mode}}"));
        assert!(guard.contains("#{!=:,#{client_session}}"));
        assert!(guard.contains("#{==:/dev/ttys001,#{window_active_clients_list}}"));
    }

    #[test]
    fn owned_guard_binds_manager_and_captured_generation() {
        let guard = owned_guard(BOOT, "7", "2f746d702f6d616e61676572");
        assert!(guard.contains("#{==:1,#{@rmux-sidebar-owned}}"));
        assert!(guard.contains(&format!("#{{==:{BOOT},#{{@rmux-sidebar-core-boot}}}}")));
        assert!(guard.contains("#{==:2f746d702f6d616e61676572,#{@rmux-sidebar-manager}}"));
        assert!(guard.contains("#{==:7,#{@rmux-sidebar-pty-generation}}"));
    }

    #[test]
    fn target_parser_rejects_empty_bridge_identity_and_bad_counts() {
        assert!(parse_target("\t1\t%1\t@1\t120\t0\t0\t\t1\t0").is_err());
        assert!(parse_target(&format!("{BOOT}\t1\t%1\t@1\t120\t0\t0\t\tmany\t0")).is_err());
        let target = parse_target(&format!("{BOOT}\t1\t%1\t@1\t120\t0\t0\t\t1\t0")).unwrap();
        assert_eq!(target.pane_id, "%1");
        assert_eq!(target.active_clients, 1);
    }

    #[test]
    fn owned_parser_rejects_respawned_or_other_manager_panes() {
        let manager = marker_for_path(Path::new("/tmp/private/manager.sock"));
        let valid = format!("{BOOT}\t9\t%3\t@2\t0\t0\t\t1\t0\t1\t{BOOT}\t{manager}\t9");
        assert!(parse_owned(&valid, BOOT, &manager).is_ok());
        let respawned = format!("{BOOT}\t10\t%3\t@2\t0\t0\t\t1\t0\t1\t{BOOT}\t{manager}\t9");
        assert!(parse_owned(&respawned, BOOT, &manager).is_err());
        assert!(parse_owned(&valid, BOOT, "00").is_err());
    }

    #[test]
    fn generated_guarded_script_quotes_every_argument() {
        let mut script = String::new();
        write_if(
            &mut script,
            "%1",
            &core_guard(BOOT, "2"),
            &[vec![
                "split-window".into(),
                "--".into(),
                "/tmp/a b/rmux-agent".into(),
                "label'; display-message hacked".into(),
            ]],
            REJECTED,
        );
        assert!(script.contains("'/tmp/a b/rmux-agent'"));
        assert!(script.contains("'label'\\''; display-message hacked'"));
        assert_eq!(script.matches("display-message").count(), 2);
    }

    #[test]
    fn manager_marker_uses_path_bytes_without_raw_separators() {
        assert_eq!(
            marker_for_path(Path::new("/tmp/m.sock")),
            "2f746d702f6d2e736f636b"
        );
    }
}
