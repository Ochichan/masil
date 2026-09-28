//! Read-only source configuration and bounded native-session projections.
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::IpAddr;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_SOURCES: usize = 8;
pub const MAX_OBSERVATIONS: usize = 64;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub sources: Vec<SourceConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    pub id: String,
    pub endpoint: String,
    pub directory: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password_env: Option<String>,
    pub sessions: Vec<SessionConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    pub id: String,
    pub pane_id: String,
    pub session_id: String,
}

#[derive(Clone, Serialize, PartialEq, Eq)]
pub struct NativeSession {
    pub id: String,
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub exists: Option<bool>,
    pub activity: String,
    pub last_activity: Option<String>,
    pub attention: String,
    pub permission_ids: Vec<String>,
    pub question_ids: Vec<String>,
    pub permission_count: usize,
    pub question_count: usize,
    pub request_ids_truncated: bool,
    pub observed_at_ms: u64,
}

impl NativeSession {
    pub fn unknown(config: &SessionConfig) -> Self {
        Self {
            id: config.id.clone(),
            session_id: config.session_id.clone(),
            parent_session_id: None,
            exists: None,
            activity: "unknown".into(),
            last_activity: None,
            attention: "unknown".into(),
            permission_ids: vec![],
            question_ids: vec![],
            permission_count: 0,
            question_count: 0,
            request_ids_truncated: false,
            observed_at_ms: 0,
        }
    }
}

#[derive(Clone, Serialize)]
pub struct SourceState {
    pub source_id: String,
    pub epoch: u64,
    pub freshness: String,
    pub reason: Option<String>,
    pub provider_version: Option<String>,
    pub sessions: Vec<NativeSession>,
    pub native_events: u64,
    pub reconciliations: u64,
}

impl SourceState {
    pub fn initial(config: &SourceConfig) -> Self {
        Self {
            source_id: config.id.clone(),
            epoch: 0,
            freshness: "syncing".into(),
            reason: None,
            provider_version: None,
            sessions: config.sessions.iter().map(NativeSession::unknown).collect(),
            native_events: 0,
            reconciliations: 0,
        }
    }

    pub fn lose(&mut self, freshness: &str, reason: &str) {
        self.freshness = freshness.into();
        self.reason = Some(reason.into());
        for session in &mut self.sessions {
            session.exists = None;
            if session.activity != "unknown" {
                session.last_activity = Some(session.activity.clone());
            }
            session.activity = "unknown".into();
            session.attention = "unknown".into();
            session.permission_ids.clear();
            session.question_ids.clear();
            session.permission_count = 0;
            session.question_count = 0;
            session.request_ids_truncated = false;
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

pub fn validate(config: &Config) -> Result<(), String> {
    if config.sources.is_empty() || config.sources.len() > MAX_SOURCES {
        return Err("config requires 1..8 sources".into());
    }
    let mut sources = HashSet::new();
    let mut endpoints = HashSet::new();
    let mut observations = HashSet::new();
    for source in &config.sources {
        if !label(&source.id) || !sources.insert(source.id.clone()) {
            return Err("source IDs must be distinct 1..64 character labels".into());
        }
        let url = reqwest::Url::parse(&source.endpoint).map_err(|_| "invalid endpoint URL")?;
        let host = url
            .host_str()
            .ok_or("endpoint host missing")?
            .trim_matches(['[', ']']);
        let address = host
            .parse::<IpAddr>()
            .map_err(|_| "endpoint requires a numeric loopback address")?;
        if url.scheme() != "http"
            || !address.is_loopback()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
            || source.endpoint.len() > 256
        {
            return Err("endpoint must be plain HTTP numeric loopback without path, credentials, query or fragment".into());
        }
        if !Path::new(&source.directory).is_absolute()
            || source.directory.len() > 4096
            || source.directory.contains('\0')
        {
            return Err("directory must be an absolute UTF-8 path of at most 4096 bytes".into());
        }
        if !endpoints.insert((url.to_string(), source.directory.clone())) {
            return Err("combine sessions for the same endpoint/directory into one source".into());
        }
        if source
            .username
            .as_ref()
            .is_some_and(|s| s.len() > 128 || s.contains(['\0', '\r', '\n', ':']))
        {
            return Err("invalid Basic auth username".into());
        }
        if let Some(name) = &source.password_env
            && (name.is_empty()
                || name.len() > 128
                || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'))
        {
            return Err("invalid password environment variable name".into());
        }
        if source.sessions.is_empty() {
            return Err("source requires at least one session".into());
        }
        let mut native_ids = HashSet::new();
        for session in &source.sessions {
            if !label(&session.id) || !observations.insert(session.id.clone()) {
                return Err("observation IDs must be globally distinct labels".into());
            }
            let pane = crate::pane_id(&session.pane_id)?;
            if session.pane_id != format!("%{pane}") {
                return Err("pane IDs must use canonical %N spelling".into());
            }
            if session.session_id.is_empty()
                || session.session_id.len() > 128
                || !session
                    .session_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            {
                return Err("invalid native session ID".into());
            }
            if !native_ids.insert(&session.session_id) {
                return Err("native session IDs must be distinct within a source".into());
            }
        }
    }
    if observations.len() > MAX_OBSERVATIONS {
        return Err("at most 64 observations are supported".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            sources: vec![SourceConfig {
                id: "local".into(),
                endpoint: "http://127.0.0.1:4096".into(),
                directory: "/tmp/project".into(),
                username: None,
                password_env: None,
                sessions: vec![SessionConfig {
                    id: "agent".into(),
                    pane_id: "%0".into(),
                    session_id: "ses_one".into(),
                }],
            }],
        }
    }

    #[test]
    fn rejects_remote_ambiguous_or_duplicate_sources() {
        for endpoint in [
            "http://example.com",
            "http://192.0.2.1",
            "https://127.0.0.1",
            "http://localhost",
            "http://u:p@127.0.0.1",
            "http://127.0.0.1/api",
            "http://127.0.0.1?x=1",
        ] {
            let mut value = config();
            value.sources[0].endpoint = endpoint.into();
            assert!(validate(&value).is_err(), "{endpoint}");
        }
        let mut value = config();
        assert!(validate(&value).is_ok());
        value.sources[0].sessions[0].pane_id = "%00".into();
        assert!(validate(&value).is_err());
        value.sources[0].sessions[0].pane_id = "%0".into();
        value.sources[0].endpoint = "http://[::1]:4096".into();
        assert!(validate(&value).is_ok());
        let mut second = value.sources[0].clone();
        second.id = "other".into();
        second.sessions[0].id = "other".into();
        value.sources.push(second);
        assert!(validate(&value).is_err());
        let mut value = config();
        let mut duplicate = value.sources[0].sessions[0].clone();
        duplicate.id = "duplicate".into();
        value.sources[0].sessions.push(duplicate);
        assert!(validate(&value).is_err());
    }

    #[test]
    fn missing_evidence_is_not_false_absence_or_current_activity() {
        let config = config();
        let mut state = SourceState::initial(&config.sources[0]);
        assert_eq!(state.sessions[0].exists, None);
        state.sessions[0].exists = Some(true);
        state.sessions[0].activity = "working".into();
        state.sessions[0].permission_ids.push("per_one".into());
        state.sessions[0].permission_count = 1;
        state.lose("stale", "sse_eof");
        assert_eq!(state.sessions[0].exists, None);
        assert_eq!(state.sessions[0].activity, "unknown");
        assert_eq!(state.sessions[0].last_activity.as_deref(), Some("working"));
        assert!(state.sessions[0].permission_ids.is_empty());
        assert_eq!(state.sessions[0].permission_count, 0);
    }
}
