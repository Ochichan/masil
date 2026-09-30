//! Read-only OpenCode native-session observation.

use crate::observation::{self, NativeSession, SourceConfig, SourceState};
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::{Client, Response, StatusCode, Url, header};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::future::{Future, pending};
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::{Instant, sleep, sleep_until, timeout};

const FINITE_BODY_LIMIT: usize = 256 * 1024;
const SSE_EVENT_LIMIT: usize = 64 * 1024;
const SSE_BATCH_LIMIT: usize = 256 * 1024;
const MAX_JSON_DEPTH: usize = 32;
const MAX_JSON_COLLECTION: usize = 256;
const MAX_JSON_NODES: usize = 8192;
const MAX_PENDING_REQUESTS: usize = 256;
const MAX_STORED_REQUEST_IDS: usize = 16;
const SESSION_INFO_CONCURRENCY: usize = 4;
const SSE_YIELD_BATCH: usize = 32;
const SSE_STREAM_BUDGET: usize = 32;
const FINITE_TIMEOUT: Duration = Duration::from_secs(5);
const SSE_INACTIVITY: Duration = Duration::from_secs(45);
const SSE_PARTIAL_TIMEOUT: Duration = Duration::from_secs(5);
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const HEALTHY_CONNECTION: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct Auth {
    username: String,
    password: String,
}

impl Auth {
    fn apply(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.basic_auth(&self.username, Some(&self.password))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ObserveError(&'static str);

impl ObserveError {
    const AUTH_REQUIRED: Self = Self("auth_required");
    const HTTP_ERROR: Self = Self("http_error");
    const TRANSPORT: Self = Self("transport_error");
    const PROTOCOL: Self = Self("provider_protocol_error");
    const BODY_OVERSIZE: Self = Self("provider_body_oversize");
    const SNAPSHOT_TIMEOUT: Self = Self("snapshot_timeout");
    const SSE_EOF: Self = Self("sse_eof");
    const SSE_INACTIVE: Self = Self("sse_inactive");
    const SSE_PARTIAL_TIMEOUT: Self = Self("sse_partial_timeout");
    const SSE_OVERSIZE: Self = Self("sse_event_oversize");
    const SSE_MALFORMED: Self = Self("sse_malformed");
    const INSTANCE_DISPOSED: Self = Self("instance_disposed");
    const PUBLICATION_FENCE_TIMEOUT: Self = Self("publication_fence_timeout");
}

enum JsonResponse {
    Value(Value),
    Missing,
}

struct Snapshot {
    sessions: Vec<NativeSession>,
    provider_version: Option<String>,
}

#[derive(Default)]
struct PendingTracker {
    sessions: HashMap<String, PendingIdentity>,
}

#[derive(Default)]
struct PendingIdentity {
    observation: Option<PendingObservation>,
    generation: u64,
}

#[derive(PartialEq, Eq)]
enum PendingObservation {
    Missing,
    Present(Vec<String>),
}

impl PendingTracker {
    fn observe(&mut self, sessions: &mut [NativeSession]) {
        for session in sessions {
            let identity = self.sessions.entry(session.id.clone()).or_default();
            let observation = if session.exists == Some(true) {
                let mut canonical = session.pending_keys.clone();
                canonical.sort_unstable();
                canonical.dedup();
                PendingObservation::Present(canonical)
            } else {
                PendingObservation::Missing
            };
            if identity.observation.as_ref() != Some(&observation) {
                identity.generation = identity
                    .generation
                    .checked_add(1)
                    .expect("pending generation exhausted");
                identity.observation = Some(observation);
            }
            session.pending_generation = identity.generation;
        }
    }
}

struct SessionInfoProjection {
    id: String,
    directory: String,
    parent_session_id: Option<String>,
}

type SnapshotFuture =
    Pin<Box<dyn Future<Output = (u64, Result<Snapshot, ObserveError>)> + Send + 'static>>;

/// Observe one configured OpenCode source until the task is aborted or its runtime is dropped.
///
/// The source shares one HTTP client and one SSE connection across all configured sessions.
pub async fn observe_source(config: SourceConfig, output: watch::Sender<SourceState>) {
    let auth = match source_auth(&config) {
        Ok(auth) => auth,
        Err(error) => {
            lose(&output, "stale", error.0);
            pending::<()>().await;
            return;
        }
    };
    let client = match Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(FINITE_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            lose(&output, "stale", ObserveError::TRANSPORT.0);
            pending::<()>().await;
            return;
        }
    };

    let mut backoff = BACKOFF_MIN;
    let mut pending_tracker = PendingTracker::default();
    while !output.is_closed() {
        let connected_at = Instant::now();
        let mut response = match connect_sse(&client, &config, auth.as_ref()).await {
            Ok(response) => response,
            Err(error) => {
                lose(&output, "stale", error.0);
                sleep(backoff).await;
                backoff = next_backoff(backoff);
                continue;
            }
        };

        let mut dirty_revision = 0_u64;
        let mut decoder = SseDecoder::default();
        let mut last_bytes = Instant::now();
        // Poll a new snapshot before accepting a ready stream chunk so its
        // five-second deadline starts immediately.
        let mut stream_budget: usize = 0;
        output.send_modify(|state| {
            state.epoch = state.epoch.saturating_add(1);
            state.lose("syncing", "reconciling");
        });
        let mut snapshot = Some(start_snapshot(
            client.clone(),
            config.clone(),
            auth.clone(),
            dirty_revision,
        ));
        let failure =
            loop {
                if output.is_closed() {
                    return;
                }
                let input = next_connection_input(
                    &mut response,
                    &mut snapshot,
                    last_bytes,
                    decoder.partial_deadline(),
                    stream_budget == 0,
                )
                .await;
                match input {
                    ConnectionInput::Chunk(chunk) => {
                        stream_budget = stream_budget.saturating_sub(1);
                        let relevant =
                            match process_sse_chunk(chunk, &mut decoder, &config, &mut last_bytes)
                                .await
                            {
                                Ok(relevant) => relevant,
                                Err(error) => break error,
                            };
                        if relevant != 0 {
                            dirty_revision = dirty_revision.saturating_add(relevant);
                            output.send_modify(|state| {
                                state.native_events = state.native_events.saturating_add(relevant);
                                state.lose("syncing", "reconciling");
                            });
                            if snapshot.is_none() {
                                snapshot = Some(start_snapshot(
                                    client.clone(),
                                    config.clone(),
                                    auth.clone(),
                                    dirty_revision,
                                ));
                                stream_budget = 0;
                            }
                        }
                    }
                    ConnectionInput::Snapshot(result) => {
                        snapshot = None;
                        stream_budget = SSE_STREAM_BUDGET;
                        let (revision, result) = result;
                        output.send_modify(|state| {
                            state.reconciliations = state.reconciliations.saturating_add(1);
                        });
                        let mut next = match result {
                            Ok(next) => next,
                            Err(error) => break error,
                        };
                        let relevant = match drain_publication_fence(
                            &mut response,
                            &mut decoder,
                            &config,
                            &mut last_bytes,
                        )
                        .await
                        {
                            Ok(relevant) => relevant,
                            Err(error) => break error,
                        };
                        if relevant != 0 {
                            dirty_revision = dirty_revision.saturating_add(relevant);
                            output.send_modify(|state| {
                                state.native_events = state.native_events.saturating_add(relevant);
                                state.lose("syncing", "reconciling");
                            });
                        }
                        if revision != dirty_revision {
                            snapshot = Some(start_snapshot(
                                client.clone(),
                                config.clone(),
                                auth.clone(),
                                dirty_revision,
                            ));
                            stream_budget = 0;
                            continue;
                        }
                        pending_tracker.observe(&mut next.sessions);
                        output.send_modify(|state| {
                            state.sessions = next.sessions;
                            state.provider_version = next.provider_version;
                            state.freshness = "fresh".into();
                            state.reason = None;
                        });
                    }
                    ConnectionInput::Inactive => break ObserveError::SSE_INACTIVE,
                    ConnectionInput::PartialTimeout => break ObserveError::SSE_PARTIAL_TIMEOUT,
                }
            };

        lose(&output, "stale", failure.0);
        if connected_at.elapsed() >= HEALTHY_CONNECTION {
            backoff = BACKOFF_MIN;
        }
        sleep(backoff).await;
        backoff = next_backoff(backoff);
    }
}

fn source_auth(config: &SourceConfig) -> Result<Option<Auth>, ObserveError> {
    let Some(name) = &config.password_env else {
        return Ok(None);
    };
    let password = std::env::var(name).map_err(|_| ObserveError::AUTH_REQUIRED)?;
    Ok(Some(Auth {
        username: config.username.clone().unwrap_or_else(|| "opencode".into()),
        password,
    }))
}

fn lose(output: &watch::Sender<SourceState>, freshness: &str, reason: &str) {
    output.send_modify(|state| state.lose(freshness, reason));
}

fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(BACKOFF_MAX)
}

async fn connect_sse(
    client: &Client,
    config: &SourceConfig,
    auth: Option<&Auth>,
) -> Result<Response, ObserveError> {
    let url = source_url(config, "/event")?;
    let mut request = client.get(url).header(header::ACCEPT, "text/event-stream");
    if let Some(auth) = auth {
        request = auth.apply(request);
    }
    let response = timeout(FINITE_TIMEOUT, request.send())
        .await
        .map_err(|_| ObserveError::TRANSPORT)?
        .map_err(|_| ObserveError::TRANSPORT)?;
    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(ObserveError::AUTH_REQUIRED);
    }
    if response.status() != StatusCode::OK {
        return Err(ObserveError::HTTP_ERROR);
    }
    if !content_type_is(&response, "text/event-stream") {
        return Err(ObserveError::PROTOCOL);
    }
    Ok(response)
}

fn start_snapshot(
    client: Client,
    config: SourceConfig,
    auth: Option<Auth>,
    revision: u64,
) -> SnapshotFuture {
    Box::pin(async move {
        let result = timeout(
            FINITE_TIMEOUT,
            snapshot_inner(&client, &config, auth.as_ref()),
        )
        .await
        .unwrap_or(Err(ObserveError::SNAPSHOT_TIMEOUT));
        (revision, result)
    })
}

async fn wait_snapshot(
    snapshot: &mut Option<SnapshotFuture>,
) -> (u64, Result<Snapshot, ObserveError>) {
    match snapshot {
        Some(snapshot) => snapshot.as_mut().await,
        None => pending().await,
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => pending().await,
    }
}

enum PairInput<C, S> {
    Chunk(C),
    Snapshot(S),
}

async fn select_pair<C, S, ChunkFuture, SnapshotWait>(
    chunk: ChunkFuture,
    snapshot: SnapshotWait,
    snapshot_first: bool,
) -> PairInput<C, S>
where
    ChunkFuture: Future<Output = C>,
    SnapshotWait: Future<Output = S>,
{
    if snapshot_first {
        tokio::select! {
            biased;
            result = snapshot => PairInput::Snapshot(result),
            chunk = chunk => PairInput::Chunk(chunk),
        }
    } else {
        tokio::select! {
            biased;
            chunk = chunk => PairInput::Chunk(chunk),
            result = snapshot => PairInput::Snapshot(result),
        }
    }
}

type SseChunk = Result<Option<(Vec<u8>, Instant)>, ObserveError>;

enum ConnectionInput {
    Chunk(SseChunk),
    Snapshot((u64, Result<Snapshot, ObserveError>)),
    Inactive,
    PartialTimeout,
}

async fn next_connection_input(
    response: &mut Response,
    snapshot: &mut Option<SnapshotFuture>,
    last_bytes: Instant,
    partial_deadline: Option<Instant>,
    snapshot_first: bool,
) -> ConnectionInput {
    let pair = select_pair(
        read_sse_chunk(response),
        wait_snapshot(snapshot),
        snapshot_first,
    );
    tokio::select! {
        biased;
        input = pair => match input {
            PairInput::Chunk(chunk) => ConnectionInput::Chunk(chunk),
            PairInput::Snapshot(snapshot) => ConnectionInput::Snapshot(snapshot),
        },
        _ = sleep_until(last_bytes + SSE_INACTIVITY) => ConnectionInput::Inactive,
        _ = wait_until(partial_deadline) => ConnectionInput::PartialTimeout,
    }
}

async fn read_sse_chunk(response: &mut Response) -> SseChunk {
    match response.chunk().await {
        Ok(Some(chunk)) if chunk.len() > SSE_BATCH_LIMIT => Err(ObserveError::SSE_OVERSIZE),
        Ok(Some(chunk)) => Ok(Some((chunk.to_vec(), Instant::now()))),
        Ok(None) => Ok(None),
        Err(_) => Err(ObserveError::TRANSPORT),
    }
}

async fn poll_ready_sse_chunk(response: &mut Response) -> Option<SseChunk> {
    tokio::select! {
        biased;
        chunk = read_sse_chunk(response) => Some(chunk),
        _ = tokio::task::yield_now() => None,
    }
}

struct PublicationFence {
    deadline: Instant,
    observed_pending: bool,
}

impl PublicationFence {
    fn new(now: Instant) -> Self {
        Self {
            deadline: now + FINITE_TIMEOUT,
            observed_pending: false,
        }
    }

    fn record_ready(&mut self) {
        self.observed_pending = false;
    }

    fn record_pending(&mut self) {
        self.observed_pending = true;
    }

    fn can_publish(&self, decoder: &SseDecoder) -> bool {
        self.observed_pending && decoder.partial_deadline().is_none()
    }
}

async fn drain_publication_fence(
    response: &mut Response,
    decoder: &mut SseDecoder,
    config: &SourceConfig,
    last_bytes: &mut Instant,
) -> Result<u64, ObserveError> {
    let mut fence = PublicationFence::new(Instant::now());
    let mut relevant = 0_u64;
    let mut chunks = 0_usize;
    loop {
        if Instant::now() >= fence.deadline {
            return Err(ObserveError::PUBLICATION_FENCE_TIMEOUT);
        }
        let chunk = if let Some(partial_deadline) = decoder.partial_deadline() {
            Some(tokio::select! {
                biased;
                chunk = read_sse_chunk(response) => chunk,
                _ = sleep_until(*last_bytes + SSE_INACTIVITY) => Err(ObserveError::SSE_INACTIVE),
                _ = sleep_until(partial_deadline) => Err(ObserveError::SSE_PARTIAL_TIMEOUT),
                _ = sleep_until(fence.deadline) => Err(ObserveError::PUBLICATION_FENCE_TIMEOUT),
            })
        } else {
            poll_ready_sse_chunk(response).await
        };
        let Some(chunk) = chunk else {
            fence.record_pending();
            if fence.can_publish(decoder) {
                return Ok(relevant);
            }
            continue;
        };
        fence.record_ready();
        relevant =
            relevant.saturating_add(process_sse_chunk(chunk, decoder, config, last_bytes).await?);
        chunks = chunks.saturating_add(1);
        if chunks.is_multiple_of(SSE_YIELD_BATCH) {
            tokio::task::yield_now().await;
        }
    }
}

async fn process_sse_chunk(
    chunk: SseChunk,
    decoder: &mut SseDecoder,
    config: &SourceConfig,
    last_bytes: &mut Instant,
) -> Result<u64, ObserveError> {
    let Some((chunk, received_at)) = chunk? else {
        return Err(ObserveError::SSE_EOF);
    };
    if received_at >= *last_bytes + SSE_INACTIVITY {
        return Err(ObserveError::SSE_INACTIVE);
    }
    if decoder
        .partial_deadline()
        .is_some_and(|deadline| received_at >= deadline)
    {
        return Err(ObserveError::SSE_PARTIAL_TIMEOUT);
    }
    if !chunk.is_empty() {
        *last_bytes = received_at;
    }
    let events = decoder.push(&chunk, received_at).await?;
    let mut relevant = 0_u64;
    for (index, event) in events.into_iter().enumerate() {
        if index != 0 && index.is_multiple_of(SSE_YIELD_BATCH) {
            tokio::task::yield_now().await;
        }
        match classify_event(&event, config) {
            EventAction::Ignore => {}
            EventAction::Reconcile => relevant = relevant.saturating_add(1),
            EventAction::Disconnect => return Err(ObserveError::INSTANCE_DISPOSED),
            EventAction::Malformed => return Err(ObserveError::PROTOCOL),
        }
    }
    Ok(relevant)
}

async fn snapshot_inner(
    client: &Client,
    config: &SourceConfig,
    auth: Option<&Auth>,
) -> Result<Snapshot, ObserveError> {
    let health = required_json_at(client, config, auth, "/global/health", false).await?;
    let provider_version = parse_health(&health)?.to_owned();
    let status = required_json(client, config, auth, "/session/status").await?;
    let infos = stream::iter(0..config.sessions.len())
        .map(|index| async move {
            let session = &config.sessions[index];
            let path = format!("/session/{}", session.session_id);
            Ok(
                match request_json(client, config, auth, &path, true, true).await? {
                    JsonResponse::Value(value) => Some(project_session_info(
                        value,
                        &session.session_id,
                        &config.directory,
                    )?),
                    JsonResponse::Missing => None,
                },
            )
        })
        .buffered(SESSION_INFO_CONCURRENCY)
        .try_collect()
        .await?;
    let permissions = required_json(client, config, auth, "/permission").await?;
    let questions = required_json(client, config, auth, "/question").await?;
    let mut snapshot = project_snapshot(config, status, infos, permissions, questions)?;
    snapshot.provider_version = Some(provider_version);
    Ok(snapshot)
}

async fn required_json(
    client: &Client,
    config: &SourceConfig,
    auth: Option<&Auth>,
    path: &str,
) -> Result<Value, ObserveError> {
    required_json_at(client, config, auth, path, true).await
}

async fn required_json_at(
    client: &Client,
    config: &SourceConfig,
    auth: Option<&Auth>,
    path: &str,
    include_directory: bool,
) -> Result<Value, ObserveError> {
    match request_json(client, config, auth, path, false, include_directory).await? {
        JsonResponse::Value(value) => Ok(value),
        JsonResponse::Missing => Err(ObserveError::PROTOCOL),
    }
}

async fn request_json(
    client: &Client,
    config: &SourceConfig,
    auth: Option<&Auth>,
    path: &str,
    allow_missing: bool,
    include_directory: bool,
) -> Result<JsonResponse, ObserveError> {
    let url = endpoint_url(config, path, include_directory)?;
    let mut request = client.get(url).header(header::ACCEPT, "application/json");
    if let Some(auth) = auth {
        request = auth.apply(request);
    }
    let mut response = request.send().await.map_err(|_| ObserveError::TRANSPORT)?;
    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(ObserveError::AUTH_REQUIRED);
    }
    if allow_missing && response.status() == StatusCode::NOT_FOUND {
        return Ok(JsonResponse::Missing);
    }
    if response.status() != StatusCode::OK {
        return Err(ObserveError::HTTP_ERROR);
    }
    if !content_type_is(&response, "application/json") {
        return Err(ObserveError::PROTOCOL);
    }
    if response
        .content_length()
        .is_some_and(|length| length > FINITE_BODY_LIMIT as u64)
    {
        return Err(ObserveError::BODY_OVERSIZE);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ObserveError::TRANSPORT)?
    {
        if body.len().saturating_add(chunk.len()) > FINITE_BODY_LIMIT {
            return Err(ObserveError::BODY_OVERSIZE);
        }
        body.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&body).map_err(|_| ObserveError::PROTOCOL)?;
    validate_json_limits(&value)?;
    Ok(JsonResponse::Value(value))
}

fn source_url(config: &SourceConfig, path: &str) -> Result<Url, ObserveError> {
    endpoint_url(config, path, true)
}

fn endpoint_url(
    config: &SourceConfig,
    path: &str,
    include_directory: bool,
) -> Result<Url, ObserveError> {
    let mut url = Url::parse(&config.endpoint).map_err(|_| ObserveError::PROTOCOL)?;
    url.set_path(path);
    url.set_query(None);
    if include_directory {
        url.query_pairs_mut()
            .append_pair("directory", &config.directory);
    }
    Ok(url)
}

fn parse_health(value: &Value) -> Result<&str, ObserveError> {
    let health = value.as_object().ok_or(ObserveError::PROTOCOL)?;
    if health.get("healthy") != Some(&Value::Bool(true)) {
        return Err(ObserveError::PROTOCOL);
    }
    let version = string_field(health, "version")?;
    if version.is_empty() || version.len() > 64 {
        return Err(ObserveError::PROTOCOL);
    }
    Ok(version)
}

fn content_type_is(response: &Response, expected: &str) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}

fn validate_json_limits(value: &Value) -> Result<(), ObserveError> {
    fn walk(value: &Value, depth: usize, nodes: &mut usize) -> Result<(), ObserveError> {
        if depth > MAX_JSON_DEPTH {
            return Err(ObserveError::PROTOCOL);
        }
        *nodes = nodes.saturating_add(1);
        if *nodes > MAX_JSON_NODES {
            return Err(ObserveError::PROTOCOL);
        }
        match value {
            Value::Array(values) => {
                if values.len() > MAX_JSON_COLLECTION {
                    return Err(ObserveError::PROTOCOL);
                }
                for value in values {
                    walk(value, depth + 1, nodes)?;
                }
            }
            Value::Object(values) => {
                if values.len() > MAX_JSON_COLLECTION {
                    return Err(ObserveError::PROTOCOL);
                }
                for value in values.values() {
                    walk(value, depth + 1, nodes)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    walk(value, 0, &mut 0)
}

fn project_snapshot(
    config: &SourceConfig,
    status: Value,
    infos: Vec<Option<SessionInfoProjection>>,
    permissions: Value,
    questions: Value,
) -> Result<Snapshot, ObserveError> {
    if infos.len() != config.sessions.len() {
        return Err(ObserveError::PROTOCOL);
    }
    let statuses = status.as_object().ok_or(ObserveError::PROTOCOL)?;
    let observed_at_ms = observation::now_ms();
    let mut sessions = Vec::with_capacity(config.sessions.len());

    for (configured, info) in config.sessions.iter().zip(infos) {
        let Some(info) = info else {
            if statuses.contains_key(&configured.session_id) {
                return Err(ObserveError::PROTOCOL);
            }
            let mut session = NativeSession::unknown(configured);
            session.exists = Some(false);
            session.attention = "none".into();
            session.observed_at_ms = observed_at_ms;
            sessions.push(session);
            continue;
        };
        if info.id != configured.session_id || info.directory != config.directory {
            return Err(ObserveError::PROTOCOL);
        }
        let activity = match statuses.get(&configured.session_id) {
            Some(status) => parse_activity(status)?,
            None => "idle",
        };
        sessions.push(NativeSession {
            id: configured.id.clone(),
            session_id: configured.session_id.clone(),
            parent_session_id: info.parent_session_id,
            exists: Some(true),
            activity: activity.into(),
            last_activity: None,
            attention: "none".into(),
            permission_ids: Vec::new(),
            question_ids: Vec::new(),
            permission_count: 0,
            question_count: 0,
            request_ids_truncated: false,
            observed_at_ms,
            pending_keys: Vec::new(),
            pending_generation: 0,
        });
    }

    let by_session: HashMap<&str, usize> = config
        .sessions
        .iter()
        .enumerate()
        .map(|(index, session)| (session.session_id.as_str(), index))
        .collect();
    let permission_count = project_requests(&permissions, &by_session, &mut sessions, true)?;
    let question_count = project_requests(&questions, &by_session, &mut sessions, false)?;
    if permission_count.saturating_add(question_count) > MAX_PENDING_REQUESTS {
        return Err(ObserveError::PROTOCOL);
    }

    for session in &mut sessions {
        let stored = session.permission_ids.len() + session.question_ids.len();
        session.request_ids_truncated = session.permission_count + session.question_count > stored;
        session.attention = if session.permission_count != 0 {
            "approval"
        } else if session.question_count != 0 {
            "question"
        } else {
            "none"
        }
        .into();
    }

    Ok(Snapshot {
        sessions,
        provider_version: None,
    })
}

fn project_session_info(
    value: Value,
    expected_id: &str,
    expected_directory: &str,
) -> Result<SessionInfoProjection, ObserveError> {
    let info = value.as_object().ok_or(ObserveError::PROTOCOL)?;
    let id = string_field(info, "id")?;
    let directory = string_field(info, "directory")?;
    if id != expected_id || directory != expected_directory {
        return Err(ObserveError::PROTOCOL);
    }
    Ok(SessionInfoProjection {
        id: id.to_owned(),
        directory: directory.to_owned(),
        parent_session_id: optional_string_field(info, "parentID")?,
    })
}

fn project_requests(
    value: &Value,
    by_session: &HashMap<&str, usize>,
    sessions: &mut [NativeSession],
    permission: bool,
) -> Result<usize, ObserveError> {
    let requests = value.as_array().ok_or(ObserveError::PROTOCOL)?;
    if requests.len() > MAX_PENDING_REQUESTS {
        return Err(ObserveError::PROTOCOL);
    }
    let mut seen = HashSet::with_capacity(requests.len());
    for request in requests {
        let request = request.as_object().ok_or(ObserveError::PROTOCOL)?;
        let id = string_field(request, "id")?;
        let session_id = string_field(request, "sessionID")?;
        if !valid_native_id(id) || !valid_native_id(session_id) || !seen.insert(id) {
            return Err(ObserveError::PROTOCOL);
        }
        let Some(&index) = by_session.get(session_id) else {
            continue;
        };
        let session = &mut sessions[index];
        let stored = session.permission_ids.len() + session.question_ids.len();
        if permission {
            session.permission_count = session.permission_count.saturating_add(1);
            session.pending_keys.push(format!("permission:{id}"));
            if stored < MAX_STORED_REQUEST_IDS {
                session.permission_ids.push(id.to_owned());
            }
        } else {
            session.question_count = session.question_count.saturating_add(1);
            session.pending_keys.push(format!("question:{id}"));
            if stored < MAX_STORED_REQUEST_IDS {
                session.question_ids.push(id.to_owned());
            }
        }
    }
    Ok(requests.len())
}

fn string_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a str, ObserveError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or(ObserveError::PROTOCOL)
}

fn optional_string_field(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<String>, ObserveError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if valid_native_id(value) => Ok(Some(value.clone())),
        _ => Err(ObserveError::PROTOCOL),
    }
}

fn valid_native_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn parse_activity(value: &Value) -> Result<&'static str, ObserveError> {
    match value
        .as_object()
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
    {
        Some("busy") => Ok("working"),
        Some("retry") => Ok("retrying"),
        Some("idle") => Ok("idle"),
        _ => Err(ObserveError::PROTOCOL),
    }
}

enum EventAction {
    Ignore,
    Reconcile,
    Disconnect,
    Malformed,
}

fn classify_event(event: &Value, config: &SourceConfig) -> EventAction {
    let Some(event) = event.as_object() else {
        return EventAction::Ignore;
    };
    let Some(event_type) = event.get("type").and_then(Value::as_str) else {
        return EventAction::Ignore;
    };
    let Some(properties) = event.get("properties").and_then(Value::as_object) else {
        return EventAction::Ignore;
    };
    if event_type == "server.instance.disposed" {
        return match properties.get("directory").and_then(Value::as_str) {
            Some(directory) if directory == config.directory => EventAction::Disconnect,
            Some(_) => EventAction::Ignore,
            None => EventAction::Malformed,
        };
    }
    let session_event = matches!(
        event_type,
        "session.created" | "session.updated" | "session.deleted" | "session.status"
    );
    let request_event = matches!(
        event_type,
        "permission.asked"
            | "permission.replied"
            | "permission.v2.asked"
            | "permission.v2.replied"
            | "question.asked"
            | "question.replied"
            | "question.rejected"
            | "question.v2.asked"
            | "question.v2.replied"
            | "question.v2.rejected"
    );
    let error_event = event_type == "session.error";
    if !session_event && !request_event && !error_event {
        return EventAction::Ignore;
    }
    let session_id = match properties.get("sessionID") {
        Some(Value::String(session_id)) if valid_native_id(session_id) => Some(session_id.as_str()),
        None if error_event => None,
        None if session_event => properties
            .get("info")
            .and_then(Value::as_object)
            .and_then(|info| info.get("id"))
            .and_then(Value::as_str)
            .filter(|id| valid_native_id(id)),
        _ => return EventAction::Malformed,
    };
    let Some(session_id) = session_id else {
        return if error_event || !session_event {
            EventAction::Ignore
        } else {
            EventAction::Malformed
        };
    };
    if !config
        .sessions
        .iter()
        .any(|session| session.session_id == session_id)
    {
        return EventAction::Ignore;
    }
    EventAction::Reconcile
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
    partial_since: Option<Instant>,
}

impl SseDecoder {
    fn partial_deadline(&self) -> Option<Instant> {
        self.partial_since
            .map(|started| started + SSE_PARTIAL_TIMEOUT)
    }

    async fn push(&mut self, chunk: &[u8], now: Instant) -> Result<Vec<Value>, ObserveError> {
        if chunk.len() > SSE_BATCH_LIMIT
            || self.buffer.len().saturating_add(chunk.len())
                > SSE_BATCH_LIMIT.saturating_add(SSE_EVENT_LIMIT)
        {
            return Err(ObserveError::SSE_OVERSIZE);
        }
        let had_partial = !self.buffer.is_empty();
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut completed_frame = false;
        let mut consumed = 0;
        while let Some((at, width)) = frame_boundary(&self.buffer[consumed..]) {
            if at > SSE_EVENT_LIMIT {
                return Err(ObserveError::SSE_OVERSIZE);
            }
            let frame = &self.buffer[consumed..consumed + at];
            consumed += at + width;
            completed_frame = true;
            if let Some(event) = parse_sse_frame(frame)? {
                events.push(event);
                if events.len().is_multiple_of(SSE_YIELD_BATCH) {
                    tokio::task::yield_now().await;
                }
            }
        }
        if consumed != 0 {
            self.buffer.drain(..consumed);
        }
        if self.buffer.len() > SSE_EVENT_LIMIT {
            return Err(ObserveError::SSE_OVERSIZE);
        }
        if self.buffer.is_empty() {
            self.partial_since = None;
        } else if !had_partial || completed_frame {
            self.partial_since = Some(now);
        }
        Ok(events)
    }
}

fn frame_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < buffer.len() {
        if buffer[index..].starts_with(b"\r\n\r\n") {
            return Some((index, 4));
        }
        if buffer[index..].starts_with(b"\n\n") || buffer[index..].starts_with(b"\r\r") {
            return Some((index, 2));
        }
        index += 1;
    }
    None
}

fn parse_sse_frame(frame: &[u8]) -> Result<Option<Value>, ObserveError> {
    let text = std::str::from_utf8(frame).map_err(|_| ObserveError::SSE_MALFORMED)?;
    let mut data = String::new();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(value) = line.strip_prefix("data:") else {
            continue;
        };
        if !data.is_empty() {
            data.push('\n');
        }
        data.push_str(value.strip_prefix(' ').unwrap_or(value));
    }
    if data.is_empty() {
        return Ok(None);
    }
    let event = serde_json::from_str(&data).map_err(|_| ObserveError::SSE_MALFORMED)?;
    validate_json_limits(&event).map_err(|_| ObserveError::SSE_MALFORMED)?;
    let object = event.as_object().ok_or(ObserveError::SSE_MALFORMED)?;
    if object.get("id").and_then(Value::as_str).is_none()
        || object.get("type").and_then(Value::as_str).is_none()
        || !object.get("properties").is_some_and(Value::is_object)
    {
        return Err(ObserveError::SSE_MALFORMED);
    }
    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::SessionConfig;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{Mutex, Notify};

    fn config() -> SourceConfig {
        SourceConfig {
            id: "source".into(),
            endpoint: "http://127.0.0.1:4096/".into(),
            directory: "/work/project".into(),
            username: None,
            password_env: None,
            sessions: vec![
                SessionConfig {
                    id: "parent".into(),
                    pane_id: "%1".into(),
                    session_id: "ses_parent".into(),
                },
                SessionConfig {
                    id: "child".into(),
                    pane_id: "%2".into(),
                    session_id: "ses_child".into(),
                },
            ],
        }
    }

    fn raw_info(id: &str, parent_id: Option<&str>) -> Value {
        let mut value = json!({
            "id": id,
            "directory": "/work/project",
            "version": "1.18.32"
        });
        if let Some(parent_id) = parent_id {
            value["parentID"] = json!(parent_id);
        }
        value
    }

    fn info(id: &str, parent_id: Option<&str>) -> SessionInfoProjection {
        project_session_info(raw_info(id, parent_id), id, "/work/project").unwrap()
    }

    #[derive(Default)]
    struct SnapshotFixtureState {
        request_starts: Vec<String>,
        session_completions: Vec<String>,
        active_session_requests: usize,
        max_active_session_requests: usize,
    }

    async fn serve_snapshot_request(
        mut stream: TcpStream,
        state: Arc<Mutex<SnapshotFixtureState>>,
        first_batch_ready: Arc<Notify>,
    ) {
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0, "request ended before its headers");
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            assert!(request.len() <= 16 * 1024, "fixture request is too large");
        }
        let request = std::str::from_utf8(&request).unwrap();
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap();
        let path = target.split('?').next().unwrap();

        let (status, body) = match path {
            "/global/health" => {
                state.lock().await.request_starts.push("health".into());
                (
                    "200 OK",
                    json!({"healthy": true, "version": "1.18.32"}).to_string(),
                )
            }
            "/session/status" => {
                state.lock().await.request_starts.push("status".into());
                ("200 OK", json!({}).to_string())
            }
            "/permission" => {
                state.lock().await.request_starts.push("permission".into());
                ("200 OK", json!([]).to_string())
            }
            "/question" => {
                state.lock().await.request_starts.push("question".into());
                ("200 OK", json!([]).to_string())
            }
            path if path.starts_with("/session/") => {
                let session_id = path.strip_prefix("/session/").unwrap().to_owned();
                let index: usize = session_id.strip_prefix("ses_").unwrap().parse().unwrap();
                let reached_limit = {
                    let mut state = state.lock().await;
                    state.request_starts.push(format!("session:{session_id}"));
                    state.active_session_requests += 1;
                    state.max_active_session_requests = state
                        .max_active_session_requests
                        .max(state.active_session_requests);
                    state.active_session_requests == SESSION_INFO_CONCURRENCY
                };
                if reached_limit {
                    first_batch_ready.notify_one();
                }
                if index == 0 {
                    first_batch_ready.notified().await;
                    sleep(Duration::from_millis(80)).await;
                } else {
                    sleep(Duration::from_millis((6 - index) as u64 * 10)).await;
                }
                {
                    let mut state = state.lock().await;
                    state.active_session_requests -= 1;
                    state.session_completions.push(session_id.clone());
                }
                if index == 2 {
                    ("404 Not Found", String::new())
                } else {
                    (
                        "200 OK",
                        json!({"id": session_id, "directory": "/work/project"}).to_string(),
                    )
                }
            }
            _ => panic!("unexpected fixture path: {path}"),
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn snapshot_fetches_session_info_four_at_a_time_without_reordering_results() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(SnapshotFixtureState::default()));
        let server_state = Arc::clone(&state);
        let first_batch_ready = Arc::new(Notify::new());
        let server = tokio::spawn(async move {
            let mut requests = tokio::task::JoinSet::new();
            for _ in 0..10 {
                let (stream, _) = listener.accept().await.unwrap();
                requests.spawn(serve_snapshot_request(
                    stream,
                    Arc::clone(&server_state),
                    Arc::clone(&first_batch_ready),
                ));
            }
            while let Some(result) = requests.join_next().await {
                result.unwrap();
            }
        });
        let config = SourceConfig {
            id: "source".into(),
            endpoint: format!("http://{address}/"),
            directory: "/work/project".into(),
            username: None,
            password_env: None,
            sessions: (0..6)
                .map(|index| SessionConfig {
                    id: format!("configured_{index}"),
                    pane_id: format!("%{index}"),
                    session_id: format!("ses_{index}"),
                })
                .collect(),
        };
        let client = Client::builder().no_proxy().build().unwrap();
        let snapshot = match timeout(
            Duration::from_secs(3),
            snapshot_inner(&client, &config, None),
        )
        .await
        {
            Ok(result) => result.unwrap(),
            Err(_) => {
                server.abort();
                panic!("snapshot fixture timed out");
            }
        };
        server.await.unwrap();

        assert_eq!(snapshot.provider_version.as_deref(), Some("1.18.32"));
        assert_eq!(
            snapshot
                .sessions
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            ["ses_0", "ses_1", "ses_2", "ses_3", "ses_4", "ses_5"]
        );
        assert_eq!(
            snapshot
                .sessions
                .iter()
                .map(|session| session.exists)
                .collect::<Vec<_>>(),
            [
                Some(true),
                Some(true),
                Some(false),
                Some(true),
                Some(true),
                Some(true)
            ]
        );

        let state = state.lock().await;
        assert_eq!(state.max_active_session_requests, SESSION_INFO_CONCURRENCY);
        assert_ne!(state.session_completions[0], "ses_0");
        assert_eq!(&state.request_starts[..2], ["health", "status"]);
        assert!(
            state.request_starts[2..8]
                .iter()
                .all(|request| request.starts_with("session:"))
        );
        assert_eq!(&state.request_starts[8..], ["permission", "question"]);
    }

    #[test]
    fn snapshot_maps_status_and_filters_requests_by_exact_session() {
        let config = config();
        let mut older_child = raw_info("ses_child", Some("ses_parent"));
        older_child["version"] = json!("1.12.0");
        let older_child = project_session_info(older_child, "ses_child", "/work/project").unwrap();
        let snapshot = project_snapshot(
            &config,
            json!({
                "ses_parent": {"type": "busy"},
                "ses_child": {"type": "retry", "attempt": 2, "next": 42},
                "ses_unrelated": {"type": "busy"}
            }),
            vec![Some(info("ses_parent", None)), Some(older_child)],
            json!([
                {"id": "per_parent", "sessionID": "ses_parent"},
                {"id": "per_unrelated", "sessionID": "ses_unrelated"}
            ]),
            json!([
                {"id": "que_parent", "sessionID": "ses_parent"},
                {"id": "que_child", "sessionID": "ses_child"}
            ]),
        )
        .unwrap();

        assert_eq!(snapshot.provider_version, None);
        assert_eq!(snapshot.sessions[0].activity, "working");
        assert_eq!(snapshot.sessions[0].attention, "approval");
        assert_eq!(snapshot.sessions[0].permission_ids, ["per_parent"]);
        assert_eq!(snapshot.sessions[0].question_ids, ["que_parent"]);
        assert_eq!(snapshot.sessions[1].activity, "retrying");
        assert_eq!(snapshot.sessions[1].attention, "question");
        assert_eq!(
            snapshot.sessions[1].parent_session_id.as_deref(),
            Some("ses_parent")
        );
    }

    #[test]
    fn existing_session_absent_from_sparse_status_is_idle_not_done() {
        let config = config();
        let snapshot = project_snapshot(
            &config,
            json!({}),
            vec![Some(info("ses_parent", None)), None],
            json!([]),
            json!([]),
        )
        .unwrap();

        assert_eq!(snapshot.sessions[0].activity, "idle");
        assert_eq!(snapshot.sessions[0].exists, Some(true));
        assert_eq!(snapshot.sessions[1].activity, "unknown");
        assert_eq!(snapshot.sessions[1].exists, Some(false));
    }

    #[test]
    fn request_ids_are_bounded_per_row_while_counts_are_exact() {
        let config = config();
        let permissions: Vec<Value> = (0..20)
            .map(|index| json!({"id": format!("per_{index}"), "sessionID": "ses_parent"}))
            .collect();
        let snapshot = project_snapshot(
            &config,
            json!({}),
            vec![Some(info("ses_parent", None)), None],
            Value::Array(permissions),
            json!([{"id": "que_parent", "sessionID": "ses_parent"}]),
        )
        .unwrap();

        let parent = &snapshot.sessions[0];
        assert_eq!(parent.permission_count, 20);
        assert_eq!(parent.question_count, 1);
        assert_eq!(parent.permission_ids.len() + parent.question_ids.len(), 16);
        assert!(parent.request_ids_truncated);
        assert_eq!(parent.pending_keys.len(), 21);
        assert!(parent.pending_keys.contains(&"permission:per_16".into()));
        assert!(parent.pending_keys.contains(&"question:que_parent".into()));
    }

    #[test]
    fn pending_generation_records_intermediate_aba_snapshots() {
        let config = config();
        let snapshot = |request_id: &str| {
            project_snapshot(
                &config,
                json!({}),
                vec![Some(info("ses_parent", None)), None],
                json!([{"id": request_id, "sessionID": "ses_parent"}]),
                json!([]),
            )
            .unwrap()
        };
        let mut tracker = PendingTracker::default();

        let mut first = snapshot("per_a");
        tracker.observe(&mut first.sessions);
        let first_generation = first.sessions[0].pending_generation;

        let mut intermediate = snapshot("per_b");
        tracker.observe(&mut intermediate.sessions);
        let mut final_snapshot = snapshot("per_a");
        tracker.observe(&mut final_snapshot.sessions);

        assert!(intermediate.sessions[0].pending_generation > first_generation);
        assert!(
            final_snapshot.sessions[0].pending_generation
                > intermediate.sessions[0].pending_generation
        );
        assert_eq!(
            final_snapshot.sessions[0].pending_keys,
            first.sessions[0].pending_keys
        );

        let mut missing =
            project_snapshot(&config, json!({}), vec![None, None], json!([]), json!([])).unwrap();
        tracker.observe(&mut missing.sessions);
        let mut reappeared = snapshot("per_a");
        tracker.observe(&mut reappeared.sessions);
        assert!(
            missing.sessions[0].pending_generation > final_snapshot.sessions[0].pending_generation
        );
        assert!(reappeared.sessions[0].pending_generation > missing.sessions[0].pending_generation);
    }

    #[test]
    fn coalesced_watch_aba_invalidates_acknowledgement() {
        use crate::attention::AttentionBook;

        let config = config();
        let snapshot = |request_id: &str| {
            project_snapshot(
                &config,
                json!({}),
                vec![Some(info("ses_parent", None)), None],
                json!([{"id": request_id, "sessionID": "ses_parent"}]),
                json!([]),
            )
            .unwrap()
        };
        let mut tracker = PendingTracker::default();
        let mut first = snapshot("per_a");
        tracker.observe(&mut first.sessions);
        let first = first.sessions.remove(0);
        let (sender, mut receiver) = watch::channel(first.clone());
        let mut book = AttentionBook::new("daemon".into());
        let acknowledged = book.reconcile(
            &first.id,
            1,
            first.pending_generation,
            true,
            &first.pending_keys,
        );
        book.acknowledge(&first.id, "daemon", &acknowledged.revision)
            .unwrap();

        let mut intermediate = snapshot("per_b");
        tracker.observe(&mut intermediate.sessions);
        sender.send_replace(intermediate.sessions.remove(0));
        let mut final_snapshot = snapshot("per_a");
        tracker.observe(&mut final_snapshot.sessions);
        sender.send_replace(final_snapshot.sessions.remove(0));

        let latest = receiver.borrow_and_update();
        assert_eq!(latest.pending_keys, first.pending_keys);
        let after_aba = book.reconcile(
            &latest.id,
            1,
            latest.pending_generation,
            true,
            &latest.pending_keys,
        );
        assert_ne!(after_aba.revision, acknowledged.revision);
        assert!(!after_aba.acknowledged);
    }

    #[test]
    fn snapshot_rejects_wrong_session_identity_or_directory() {
        let wrong_id =
            project_session_info(raw_info("ses_other", None), "ses_parent", "/work/project");
        assert_eq!(wrong_id.err(), Some(ObserveError::PROTOCOL));

        let mut wrong_directory = raw_info("ses_parent", None);
        wrong_directory["directory"] = json!("/work/other");
        let wrong_directory = project_session_info(wrong_directory, "ses_parent", "/work/project");
        assert_eq!(wrong_directory.err(), Some(ObserveError::PROTOCOL));
    }

    #[test]
    fn events_are_filtered_by_exact_session_and_disposal_directory() {
        let config = config();
        let child = json!({
            "id": "evt_1",
            "type": "session.status",
            "properties": {"sessionID": "ses_child", "status": {"type": "idle"}}
        });
        let unrelated = json!({
            "id": "evt_2",
            "type": "session.deleted",
            "properties": {"sessionID": "ses_unrelated"}
        });
        assert!(matches!(
            classify_event(&child, &config),
            EventAction::Reconcile
        ));
        assert!(matches!(
            classify_event(&unrelated, &config),
            EventAction::Ignore
        ));
        let deleted = json!({
            "id": "evt_3",
            "type": "session.deleted",
            "properties": {"info": {"id": "ses_parent"}}
        });
        assert!(matches!(
            classify_event(&deleted, &config),
            EventAction::Reconcile
        ));
        let disposed = json!({
            "id": "evt_4",
            "type": "server.instance.disposed",
            "properties": {"directory": "/work/project"}
        });
        assert!(matches!(
            classify_event(&disposed, &config),
            EventAction::Disconnect
        ));
        let other_disposed = json!({
            "id": "evt_5",
            "type": "server.instance.disposed",
            "properties": {"directory": "/work/other"}
        });
        assert!(matches!(
            classify_event(&other_disposed, &config),
            EventAction::Ignore
        ));
        let malformed_permission = json!({
            "id": "evt_6",
            "type": "permission.asked",
            "properties": {"sessionID": 7, "id": "per_bad"}
        });
        assert!(matches!(
            classify_event(&malformed_permission, &config),
            EventAction::Malformed
        ));
        let unknown_without_identity = json!({
            "id": "evt_7",
            "type": "message.part.updated",
            "properties": {}
        });
        assert!(matches!(
            classify_event(&unknown_without_identity, &config),
            EventAction::Ignore
        ));
        let global_error = json!({
            "id": "evt_8",
            "type": "session.error",
            "properties": {"error": {"name": "UnknownError"}}
        });
        assert!(matches!(
            classify_event(&global_error, &config),
            EventAction::Ignore
        ));
    }

    #[tokio::test]
    async fn sse_decoder_handles_split_frames_and_rejects_malformed_envelopes() {
        let now = Instant::now();
        let mut decoder = SseDecoder::default();
        assert!(
            decoder
                .push(b"data: {\"id\":\"evt\",\"type\":\"server.heart", now)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(decoder.partial_deadline().is_some());
        let events = decoder
            .push(b"beat\",\"properties\":{}}\n\n", now)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(decoder.partial_deadline().is_none());

        let malformed = decoder
            .push(b"data: {\"type\":\"session.status\"}\n\n", now)
            .await;
        assert_eq!(malformed.err(), Some(ObserveError::SSE_MALFORMED));
    }

    #[tokio::test]
    async fn sse_decoder_resets_partial_deadline_for_a_new_frame_and_bounds_batches() {
        let first = Instant::now();
        let second = first + Duration::from_secs(2);
        let mut decoder = SseDecoder::default();
        decoder
            .push(
                b"data: {\"id\":\"first\",\"type\":\"server.heartbeat\"",
                first,
            )
            .await
            .unwrap();
        decoder
            .push(
                b",\"properties\":{}}\n\ndata: {\"id\":\"next\",\"type\":\"server.heartbeat\"",
                second,
            )
            .await
            .unwrap();
        assert_eq!(
            decoder.partial_deadline(),
            Some(second + SSE_PARTIAL_TIMEOUT)
        );

        let oversized = vec![b'x'; SSE_BATCH_LIMIT + 1];
        assert_eq!(
            decoder.push(&oversized, second).await,
            Err(ObserveError::SSE_OVERSIZE)
        );
    }

    #[tokio::test]
    async fn continuously_ready_irrelevant_stream_cannot_starve_snapshot() {
        let mut snapshot: Option<SnapshotFuture> = Some(Box::pin(async {
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            (7, Err(ObserveError::PROTOCOL))
        }));
        let mut stream_budget = 0_usize;
        let mut chunks = 0_usize;
        let completed = loop {
            match select_pair(
                std::future::ready(()),
                wait_snapshot(&mut snapshot),
                stream_budget == 0,
            )
            .await
            {
                PairInput::Chunk(()) => {
                    chunks += 1;
                    stream_budget = stream_budget.saturating_sub(1);
                }
                PairInput::Snapshot(result) => break result,
            }
        };

        assert_eq!(completed.0, 7);
        assert!(matches!(completed.1, Err(ObserveError::PROTOCOL)));
        assert!(chunks <= 4, "snapshot was starved for {chunks} chunks");
    }

    #[test]
    fn publication_fence_requires_an_observed_pending_boundary() {
        let decoder = SseDecoder::default();
        let mut fence = PublicationFence::new(Instant::now());
        for _ in 0..(SSE_STREAM_BUDGET + 1) {
            fence.record_ready();
            assert!(!fence.can_publish(&decoder));
        }
        fence.record_pending();
        assert!(fence.can_publish(&decoder));
    }

    #[tokio::test]
    async fn publication_fence_blocks_while_an_sse_frame_is_partial() {
        let now = Instant::now();
        let mut decoder = SseDecoder::default();
        decoder
            .push(b"data: {\"id\":\"evt\",\"type\":\"session.status\"", now)
            .await
            .unwrap();
        let mut fence = PublicationFence::new(now);
        fence.record_pending();
        assert!(!fence.can_publish(&decoder));

        fence.record_ready();
        decoder
            .push(
                b",\"properties\":{\"sessionID\":\"ses_parent\",\"status\":{\"type\":\"idle\"}}}\n\n",
                now,
            )
            .await
            .unwrap();
        assert!(!fence.can_publish(&decoder));
        fence.record_pending();
        assert!(fence.can_publish(&decoder));
    }

    #[test]
    fn json_projection_has_explicit_collection_bound() {
        let too_many = Value::Array((0..=MAX_JSON_COLLECTION).map(|_| Value::Null).collect());
        assert_eq!(validate_json_limits(&too_many), Err(ObserveError::PROTOCOL));
    }

    #[test]
    fn health_is_the_runtime_provider_version_source() {
        assert_eq!(
            parse_health(&json!({"healthy": true, "version": "1.18.32"})),
            Ok("1.18.32")
        );
        assert_eq!(
            parse_health(&json!({"healthy": false, "version": "1.18.32"})),
            Err(ObserveError::PROTOCOL)
        );
    }
}
