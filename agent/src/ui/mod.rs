/*
THESIS: Agent attention stays beside native terminals, with evidence before action.
OWN-WORLD: Charcoal cell grid, quiet borders, teal selection, text status labels.
STORY: Find a waiting agent, inspect its evidence, mark it seen, reach its pane.
FIRST VIEWPORT: A 34-column sidebar expands through native zoom into a list and
inspector. Filters and search sit above rows; safe actions sit below them.
FORM: User-pinned persistent sidebar and full management view, seed e2f9e8dc.
FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
*/
mod i18n;
mod input;
mod managed;
pub(crate) mod model;
mod network;
mod preferences;
pub(crate) mod settings;
mod terminal;
mod view;

use futures_util::StreamExt;
use model::{App, Effect, Language, Theme};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Instant};
use tokio::{
    sync::{Notify, watch},
    task::JoinSet,
};

// Keep the terminal build's direction reference auditable in the stripped
// executable through an explicit CLI, without putting design metadata in UI.
pub(crate) const DESIGN_CONTRACT: &str = "masil agent desk, Operate; user-pinned sidebar plus native zoom; seed e2f9e8dc; docs/ui/agent-desk.md";

pub(crate) fn run_managed(socket: &str, args: &[String], sidebar: bool) -> Result<i32, String> {
    managed::run(socket, args, sidebar)
}

#[derive(Default)]
struct Options {
    native: Option<PathBuf>,
    client: Option<String>,
    target: Option<String>,
    compact: bool,
    language: Option<Language>,
    theme: Option<Theme>,
}

impl Options {
    fn parse(args: &[String], sidebar: bool) -> Result<Self, String> {
        let mut result = Self::default();
        let mut seen = std::collections::HashSet::new();
        let mut index = 0;
        while index < args.len() {
            let key = args[index].as_str();
            if !seen.insert(key) {
                return Err(format!("repeated UI option: {key}"));
            }
            if key == "--compact" && !sidebar {
                result.compact = true;
                index += 1;
                continue;
            }
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("missing value for {key}"))?;
            match key {
                "--core-native" => result.native = Some(value.into()),
                "--client" => result.client = Some(value.clone()),
                "--target" if sidebar => result.target = Some(value.clone()),
                "--lang" => result.language = Some(value.parse()?),
                "--theme" => result.theme = Some(value.parse()?),
                _ => return Err(format!("unknown UI option: {key}")),
            }
            index += 2;
        }
        if result.client.is_some() && result.native.is_none() {
            return Err("--client requires --core-native".into());
        }
        if sidebar && result.native.is_none() {
            return Err("sidebar requires --core-native PATH".into());
        }
        Ok(result)
    }
}

pub(crate) fn run(socket: &str, args: &[String], sidebar: bool) -> Result<i32, String> {
    let options = Options::parse(args, sidebar)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(async {
            if sidebar {
                open_sidebar(socket, options).await
            } else {
                desk(socket.into(), options).await
            }
        })
}

async fn open_sidebar(socket: &str, options: Options) -> Result<i32, String> {
    let context = crate::native_ui::Context {
        socket: options.native.expect("validated native path"),
        client: options.client,
    };
    let result = context
        .sidebar(
            std::path::Path::new(socket),
            &std::env::current_exe().map_err(|e| e.to_string())?,
            options.target.as_deref(),
            options.language.map(Language::as_str),
            options.theme.map(Theme::as_str),
        )
        .await?;
    match result {
        crate::native_ui::SidebarOutcome::Created { pane_id, .. } => println!(
            "Created agent sidebar {pane_id}. Shared window layout; select it with tmux pane navigation or the mouse."
        ),
        crate::native_ui::SidebarOutcome::Reused { pane_id, .. } => {
            println!("Agent sidebar already open in {pane_id}.")
        }
    }
    Ok(0)
}

struct ResultMessage {
    epoch: String,
    kind: ResultKind,
    result: Result<Value, String>,
}
enum ResultKind {
    Ack { id: String, revision: String },
    Inspect(String),
    Native,
}

fn text(language: Language, en: &str, ko: &str) -> String {
    match language {
        Language::English => en,
        Language::Korean => ko,
    }
    .into()
}

fn native_receipt(language: Language, tag: &str) -> String {
    let (en, ko) = match tag {
        "selected" => ("Selected the configured pane", "설정된 창으로 이동했습니다"),
        "expanded" => (
            "Expanded agent sidebar",
            "에이전트 사이드패널을 확대했습니다",
        ),
        "restored" => ("Restored window layout", "창 분할을 복원했습니다"),
        "copied" => (
            "Copied ID to tmux buffer masil-agent-id",
            "tmux 버퍼 masil-agent-id에 ID를 복사했습니다",
        ),
        _ => (
            "Unknown native action response",
            "창 조작의 응답을 확인할 수 없습니다",
        ),
    };
    text(language, en, ko)
}

async fn desk(socket: PathBuf, options: Options) -> Result<i32, String> {
    let preferences = preferences::Preferences::load();
    let mut app = App::new(
        options.compact,
        options.language.unwrap_or(preferences.language),
        options.theme.unwrap_or(preferences.theme),
    );
    if let Some(warning) = &preferences.warning {
        app.notify(warning.clone());
    }
    let native = options.native.map(|socket| crate::native_ui::Context {
        socket,
        client: options.client,
    });
    let owned_pane = if options.compact {
        std::env::var("TMUX_PANE").ok()
    } else {
        None
    };
    let (tx, mut rx) = watch::channel(network::Update::Disconnected(
        "Connecting to observer".into(),
    ));
    let retry = Arc::new(Notify::new());
    let reader = tokio::spawn(network::watch(socket.clone(), tx, retry.clone()));
    let mut tasks = JoinSet::<ResultMessage>::new();
    let mut boot: Option<String> = None;
    let mut native_busy = false;
    // Register handled signals before entering raw/alternate-screen modes.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|e| e.to_string())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(|e| e.to_string())?;
    let mut terminal = terminal::Session::open()?;
    let mut events = crossterm::event::EventStream::new();
    let result = async {
        loop {
            if app.dirty {
                terminal.terminal().draw(|frame| app.draw(frame)).map_err(|e| e.to_string())?;
                app.dirty = false;
            }
            let effects = tokio::select! {
                event = events.next() => match event {
                    Some(Ok(crossterm::event::Event::Key(key))) if key.code == crossterm::event::KeyCode::Char('c') && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => break,
                    Some(Ok(event)) => app.handle(event, Instant::now()),
                    Some(Err(error)) => return Err(error.to_string()),
                    None => break,
                },
                changed = rx.changed() => {
                    if changed.is_err() { return Err("observer reader stopped".into()); }
                    match rx.borrow_and_update().clone() {
                        network::Update::Snapshot {value, boot: next_boot} => {
                            boot = next_boot;
                            app.apply_snapshot(value);
                            app.set_connection(true, None);
                            app.set_native_available(native.is_some() && boot.is_some());
                        },
                        network::Update::Disconnected(message) => {
                            boot = None;
                            app.set_connection(false, Some(message));
                            app.set_native_available(false);
                        },
                    }
                    Vec::new()
                },
                Some(completed) = tasks.join_next(), if !tasks.is_empty() => {
                    match completed {
                        Ok(message) => {
                            if matches!(message.kind, ResultKind::Native) { native_busy = false; }
                            if let ResultKind::Ack {id, revision} = &message.kind
                                && message.epoch == app.epoch
                                && app.ack_in_flight.as_ref() == Some(&(id.clone(), revision.clone())) {
                                app.finish_action();
                            }
                            if message.epoch == app.epoch && app.connected {
                                match message.result {
                                    Ok(value) => match message.kind {
                                        ResultKind::Inspect(id) => {
                                            // Detail responses are supplemental evidence. Never
                                            // let older data overwrite a newer complete row.
                                            if value["epoch"] == app.epoch && value["revision"] == app.revision {
                                                app.set_details(&id, value["observation"].clone());
                                            }
                                        },
                                        ResultKind::Ack {id, revision} => {
                                            let unchanged = app.rows.iter().any(|row| row.id == id && row.attention_revision == revision);
                                            app.notify(if unchanged {
                                                text(app.language, &format!("Marked {id} as seen"), &format!("{id} 확인 표시 완료"))
                                            } else {
                                                text(app.language, "Earlier request marked seen; newer evidence is unchanged", "이전 요청을 확인했습니다. 새 요청의 상태는 바꾸지 않았습니다")
                                            });
                                        },
                                        ResultKind::Native => app.notify(native_receipt(app.language, value.as_str().unwrap_or_default())),
                                    },
                                    Err(message) => app.notify(message),
                                }
                            }
                        },
                        Err(error) => { native_busy = false; app.finish_action(); app.notify(format!("Action failed: {error}")); },
                    }
                    Vec::new()
                },
                _ = terminate.recv() => break,
                _ = interrupt.recv() => break,
                _ = hangup.recv() => break,
            };
            for effect in effects {
                match effect {
                    Effect::Quit => return Ok(0),
                    Effect::Retry => {
                        app.set_connection(false, Some(text(app.language, "Reconnecting to observer", "관찰자에 다시 연결하는 중")));
                        app.set_native_available(false); boot = None; retry.notify_one();
                    },
                    Effect::Preferences { language, theme } => {
                        if let Err(message) = preferences.save(language, theme) { app.notify(message); }
                    },
                    effect => {
                        if tasks.len() >= 8 {
                            if matches!(effect, Effect::Ack {..}) { app.finish_action(); }
                            app.notify(text(app.language, "An action is pending; try again shortly", "작업을 처리하고 있습니다. 잠시 후 다시 시도하세요"));
                            continue;
                        }
                        let socket = socket.clone();
                        match effect {
                            Effect::Ack {id, epoch, revision} => {
                                tasks.spawn(async move {
                                    let result = network::query(&socket, json!({"v":1,"kind":"ack","request_id":"ack","id":id,"epoch":epoch,"revision":revision})).await;
                                    ResultMessage {epoch, kind:ResultKind::Ack {id, revision}, result}
                                });
                            },
                            Effect::Inspect {id, epoch} => {
                                tasks.spawn(async move {
                                    let result = network::query(&socket, json!({"v":1,"kind":"inspect","request_id":"inspect","id":id})).await;
                                    ResultMessage {epoch, kind:ResultKind::Inspect(id), result}
                                });
                            },
                            effect => {
                                if native_busy {
                                    app.notify(text(app.language, "A pane action is still running", "창 조작을 처리하고 있습니다"));
                                    continue;
                                }
                                let (Some(context), Some(expected_boot)) = (native.clone(), boot.clone()) else { continue; };
                                native_busy = true;
                                let epoch = app.epoch.clone();
                                let owned = owned_pane.clone();
                                tasks.spawn(async move {
                                    let result = match effect {
                                        Effect::Navigate {pane_id, pty_generation, ..} => context.navigate(&expected_boot, &pane_id, &pty_generation, owned.as_deref(), &socket).await.map(|_| "selected".to_owned()),
                                        Effect::Expand => match owned {
                                            Some(pane) => context.toggle_zoom(&expected_boot, &pane, &socket).await.map(|outcome| match outcome {
                                                crate::native_ui::ZoomOutcome::Expanded => "expanded".into(),
                                                crate::native_ui::ZoomOutcome::Restored => "restored".into(),
                                            }),
                                            None => Err("Expand is available from the native sidebar".into()),
                                        },
                                        Effect::CopyId {value, ..} => context.copy_to_buffer(&expected_boot, &value).await.map(|_| "copied".to_owned()),
                                        _ => unreachable!(),
                                    }.map(Value::String);
                                    ResultMessage {epoch, kind:ResultKind::Native, result}
                                });
                            },
                        }
                    },
                }
            }
        }
        Ok(0)
    }.await;
    tasks.abort_all();
    reader.abort();
    // Restore before returning an error to main's stderr reporting.
    drop(events);
    drop(terminal);
    result
}
