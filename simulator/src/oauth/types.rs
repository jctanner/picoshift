use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tracing::info;

use crate::AuthMode;

use super::byoidc::{ByoidcConfig, ExternalJwksCache};

pub(crate) const OAUTH_PORT: u16 = 9443;
pub(crate) const DOMAIN: &str = "apps.ocp-sim.test";
pub(crate) const OAUTH_HOST: &str = "oauth-openshift";
pub(crate) const OAUTH_NS: &str = "openshift-authentication";
pub(crate) const CODE_TTL: Duration = Duration::from_secs(300);
pub(crate) const TOKEN_TTL: Duration = Duration::from_secs(86400);

// ---------------------------------------------------------------------------
// User store
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, serde::Deserialize)]
pub struct UserEntry {
    pub username: String,
    pub password: String,
    pub email: Option<String>,
    pub groups: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize)]
struct UsersConfig {
    users: Vec<UserEntry>,
}

pub struct UserStore {
    pub(crate) users: Vec<UserEntry>,
}

impl UserStore {
    pub fn load(path: Option<&str>) -> Self {
        if let Some(p) = path {
            match std::fs::read_to_string(p) {
                Ok(contents) => match serde_yaml::from_str::<UsersConfig>(&contents) {
                    Ok(config) => {
                        info!(path = p, count = config.users.len(), "loaded users file");
                        return Self { users: config.users };
                    }
                    Err(e) => tracing::warn!(path = p, %e, "failed to parse users file, using default"),
                },
                Err(e) => tracing::warn!(path = p, %e, "failed to read users file, using default"),
            }
        }
        info!("using default user: admin/admin");
        Self {
            users: vec![UserEntry {
                username: "admin".into(),
                password: "admin".into(),
                email: Some("admin@ocp-sim.test".into()),
                groups: Some(vec![
                    "system:cluster-admins".into(),
                    "system:authenticated".into(),
                ]),
            }],
        }
    }

    pub(crate) fn authenticate(&self, username: &str, password: &str) -> Option<&UserEntry> {
        self.users
            .iter()
            .find(|u| u.username == username && u.password == password)
    }

    pub(crate) fn get(&self, username: &str) -> Option<&UserEntry> {
        self.users.iter().find(|u| u.username == username)
    }
}

// ---------------------------------------------------------------------------
// JWT keys (OIDC mode)
// ---------------------------------------------------------------------------

pub(crate) struct JwtKeys {
    pub(crate) encoding_key: jsonwebtoken::EncodingKey,
    pub(crate) kid: String,
    pub(crate) n_b64: String,
    pub(crate) e_b64: String,
}

impl JwtKeys {
    pub(crate) fn generate() -> Self {
        use base64::Engine;
        use jsonwebtoken::EncodingKey;

        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256)
            .expect("failed to generate RSA key pair");
        let private_pem = key_pair.serialize_pem();
        let encoding_key = EncodingKey::from_rsa_pem(private_pem.as_bytes())
            .expect("failed to create encoding key from PEM");

        let raw = key_pair.public_key_raw();
        let (n_bytes, e_bytes) = Self::parse_rsa_integers(raw);

        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let n_b64 = engine.encode(&n_bytes);
        let e_b64 = engine.encode(&e_bytes);

        let kid = super::helpers::generate_random_string(12);

        info!(kid = %kid, "generated RSA signing key for OIDC mode");

        Self { encoding_key, kid, n_b64, e_b64 }
    }

    fn parse_rsa_integers(der: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut pos = 0;
        pos += 1; // SEQUENCE tag 0x30
        pos += Self::der_len_size(&der[pos..]);

        pos += 1; // INTEGER tag 0x02
        let n_len = Self::read_der_len(&der[pos..]);
        pos += Self::der_len_size(&der[pos..]);
        let mut n = der[pos..pos + n_len].to_vec();
        if !n.is_empty() && n[0] == 0 { n.remove(0); }
        pos += n_len;

        pos += 1; // INTEGER tag 0x02
        let e_len = Self::read_der_len(&der[pos..]);
        pos += Self::der_len_size(&der[pos..]);
        let e = der[pos..pos + e_len].to_vec();

        (n, e)
    }

    fn der_len_size(der: &[u8]) -> usize {
        if der[0] < 0x80 { 1 } else { 1 + (der[0] & 0x7f) as usize }
    }

    fn read_der_len(der: &[u8]) -> usize {
        let first = der[0];
        if first < 0x80 {
            first as usize
        } else {
            let num = (first & 0x7f) as usize;
            let mut len = 0usize;
            for i in 0..num {
                len = (len << 8) | der[1 + i] as usize;
            }
            len
        }
    }
}

// ---------------------------------------------------------------------------
// OAuth state
// ---------------------------------------------------------------------------

pub(crate) struct AuthCode {
    pub(crate) username: String,
    pub(crate) client_id: String,
    pub(crate) _redirect_uri: String,
    pub(crate) created: Instant,
}

pub(crate) struct TokenInfo {
    pub(crate) username: String,
    pub(crate) _client_id: String,
    pub(crate) created: Instant,
}

pub(crate) struct OAuthState {
    pub(crate) codes: RwLock<HashMap<String, AuthCode>>,
    pub(crate) tokens: RwLock<HashMap<String, TokenInfo>>,
    pub(crate) user_store: Arc<UserStore>,
    pub(crate) auth_mode: AuthMode,
    pub(crate) jwt_keys: Option<Arc<JwtKeys>>,
    pub(crate) byoidc: Option<Arc<ByoidcConfig>>,
    pub(crate) external_jwks: Option<Arc<ExternalJwksCache>>,
}

impl OAuthState {
    pub(crate) async fn new(user_store: Arc<UserStore>, auth_mode: AuthMode, byoidc: Option<ByoidcConfig>) -> Self {
        let jwt_keys = if matches!(auth_mode, AuthMode::Oidc) {
            Some(Arc::new(JwtKeys::generate()))
        } else {
            None
        };
        let (byoidc_arc, external_jwks) = if let Some(cfg) = byoidc {
            let cache = Arc::new(ExternalJwksCache::new(&cfg.issuer_url).await);
            (Some(Arc::new(cfg)), Some(cache))
        } else {
            (None, None)
        };
        Self {
            codes: RwLock::new(HashMap::new()),
            tokens: RwLock::new(HashMap::new()),
            user_store,
            auth_mode,
            jwt_keys,
            byoidc: byoidc_arc,
            external_jwks,
        }
    }
}
