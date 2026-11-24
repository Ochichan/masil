use std::env;

#[derive(Debug, Clone)]
pub struct ApnsConfig {
    pub key_pem: String,
    pub key_id: String,
    pub team_id: String,
    pub bundle_id: String,
    pub sandbox: bool,
}

#[derive(Debug, Clone)]
pub struct FcmConfig {
    pub project_id: String,
    pub client_email: String,
    pub private_key_pem: String,
}

#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub listen: String,
    pub max_hosts: usize,
    pub max_clients_per_host: usize,
    pub max_frame_bytes: usize,
    pub apns: Option<ApnsConfig>,
    pub fcm: Option<FcmConfig>,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8787".into(),
            max_hosts: 256,
            max_clients_per_host: 16,
            max_frame_bytes: 4 * 1024 * 1024,
            apns: None,
            fcm: None,
        }
    }
}

impl RelayConfig {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = env::var("MASIL_RELAY_LISTEN")
            && !v.trim().is_empty()
        {
            cfg.listen = v;
        }
        if let Ok(v) = env::var("MASIL_RELAY_MAX_HOSTS")
            && let Ok(n) = v.parse::<usize>()
            && n > 0
        {
            cfg.max_hosts = n;
        }
        if let Ok(v) = env::var("MASIL_RELAY_MAX_CLIENTS_PER_HOST")
            && let Ok(n) = v.parse::<usize>()
            && n > 0
        {
            cfg.max_clients_per_host = n;
        }

        if let (Ok(key_pem), Ok(key_id), Ok(team_id), Ok(bundle_id)) = (
            env::var("MASIL_PUSH_APNS_KEY_PEM"),
            env::var("MASIL_PUSH_APNS_KEY_ID"),
            env::var("MASIL_PUSH_APNS_TEAM_ID"),
            env::var("MASIL_PUSH_APNS_BUNDLE_ID"),
        ) && !key_pem.is_empty()
            && !key_id.is_empty()
            && !team_id.is_empty()
            && !bundle_id.is_empty()
        {
            let sandbox = env::var("MASIL_PUSH_APNS_SANDBOX")
                .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(false);
            cfg.apns = Some(ApnsConfig {
                key_pem,
                key_id,
                team_id,
                bundle_id,
                sandbox,
            });
        }

        if let (Ok(project_id), Ok(client_email), Ok(private_key_pem)) = (
            env::var("MASIL_PUSH_FCM_PROJECT_ID"),
            env::var("MASIL_PUSH_FCM_CLIENT_EMAIL"),
            env::var("MASIL_PUSH_FCM_PRIVATE_KEY_PEM"),
        ) && !project_id.is_empty()
            && !client_email.is_empty()
            && !private_key_pem.is_empty()
        {
            cfg.fcm = Some(FcmConfig {
                project_id,
                client_email,
                private_key_pem,
            });
        }

        cfg
    }
}
