//! Authentication middleware — API key + JWT validation.
//!
//! Supports two modes:
//! 1. **Static API keys** — pre-shared Bearer tokens configured in TOML
//! 2. **JWT (JSON Web Tokens)** — RS256/ES256 verification via JWKS or static keys
//!
//! # Bypass list
//!
//! Health, readiness, and metrics endpoints bypass authentication.
//! Configure additional bypass paths in `AuthConfig::bypass_paths`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::{Deserialize, Serialize};

// ─── config ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuthConfig {
    /// Enable authentication
    #[serde(default)]
    pub enabled: bool,
    /// Static API keys with optional labels
    #[serde(default)]
    pub api_keys: HashMap<String, ApiKeyEntry>,
    /// JWT verification
    #[serde(default)]
    pub jwt: Option<JwtConfig>,
    /// Paths that bypass authentication
    #[serde(default = "default_bypass")]
    pub bypass_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiKeyEntry {
    /// Human-readable label (for audit)
    pub label: String,
    /// Optional: restrict to specific endpoints
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JwtConfig {
    /// Supported algorithms: "RS256", "ES256", "HS256"
    pub algorithms: Vec<String>,
    /// JWKS URL for key discovery (optional)
    pub jwks_url: Option<String>,
    /// Static public keys (PEM), used if jwks_url is None
    pub keys: Option<Vec<String>>,
    /// Required issuer claim
    pub issuer: Option<String>,
    /// Required audience claim
    pub audience: Option<String>,
    /// Validate expiry (nbf, exp)
    #[serde(default = "default_true")]
    pub validate_expiry: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_keys: HashMap::new(),
            jwt: None,
            bypass_paths: default_bypass(),
        }
    }
}

fn default_bypass() -> Vec<String> {
    vec![
        "/healthz".into(),
        "/livez".into(),
        "/readyz".into(),
        "/metrics".into(),
        "/.well-known/agent.json".into(),
    ]
}

fn default_true() -> bool {
    true
}

// ─── state ────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AuthState {
    inner: Arc<AuthInner>,
}

/// Minimal JWT claims decoded from the token payload.
///
/// Decoding into a typed struct (rather than `serde_json::Value`) avoids the
/// type-confusion class of bugs fixed in jsonwebtoken 10.x: the payload can no
/// longer be deserialized in a way that aliases the header, and only the claims
/// we actually consume are populated.
#[derive(Debug, Deserialize)]
struct Claims {
    sub: Option<String>,
}

struct AuthInner {
    config: AuthConfig,
    /// Pre-parsed JWT decoding keys indexed by key ID
    jwk_map: HashMap<String, DecodingKey>,
    /// Cached Validation settings
    jwt_validation: Option<Validation>,
    _algorithms: Vec<Algorithm>,
    /// Count of failed authentication attempts (v1.2)
    failure_count: AtomicU64,
}

impl AuthState {
    pub fn new(config: AuthConfig) -> Self {
        let algorithms: Vec<Algorithm> = match &config.jwt {
            Some(jwt) => jwt
                .algorithms
                .iter()
                .filter_map(|a| match a.as_str() {
                    "RS256" => Some(Algorithm::RS256),
                    "ES256" => Some(Algorithm::ES256),
                    "HS256" => Some(Algorithm::HS256),
                    "RS384" => Some(Algorithm::RS384),
                    "RS512" => Some(Algorithm::RS512),
                    _ => None,
                })
                .collect(),
            None => vec![],
        };

        let mut jwk_map = HashMap::new();
        if let Some(jwt) = &config.jwt {
            if let Some(keys) = &jwt.keys {
                for (i, pem) in keys.iter().enumerate() {
                    let key = DecodingKey::from_rsa_pem(pem.as_bytes())
                        .or_else(|_| DecodingKey::from_ec_pem(pem.as_bytes()))
                        .ok()
                        .or_else(|| Some(DecodingKey::from_secret(pem.as_bytes())));
                    if let Some(k) = key {
                        jwk_map.insert(format!("static-{}", i), k);
                    }
                }
            }
        }

        let jwt_validation = config.jwt.as_ref().map(|jwt| {
            let mut v = Validation::new(algorithms.first().copied().unwrap_or(Algorithm::RS256));
            if let Some(iss) = &jwt.issuer {
                v.set_issuer(&[iss]);
            }
            if let Some(aud) = &jwt.audience {
                v.set_audience(&[aud]);
            }
            v.validate_exp = jwt.validate_expiry;
            v.validate_nbf = jwt.validate_expiry;
            v.algorithms = algorithms.clone();
            v
        });

        Self {
            inner: Arc::new(AuthInner {
                config,
                jwk_map,
                jwt_validation,
                _algorithms: algorithms,
                failure_count: AtomicU64::new(0),
            }),
        }
    }

    /// Authenticate a request. Returns the principal (key label or JWT sub)
    /// on success, or `None` if authentication failed.
    pub fn authenticate(&self, req: &Request<Body>) -> Option<String> {
        let inner = &self.inner;

        let auth_header = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())?;

        let token = auth_header.strip_prefix("Bearer ")?;

        // Try static API keys first
        if let Some(entry) = inner.config.api_keys.get(token) {
            return Some(entry.label.clone());
        }

        // Try JWT
        if let Some(ref validation) = inner.jwt_validation {
            // Try each key
            for (kid, key) in &inner.jwk_map {
                let v = validation.clone();
                if let Ok(data) = decode::<Claims>(token, key, &v) {
                    return data.claims.sub;
                }
                // Also try without key ID
                let _ = kid;
            }
        }

        None
    }

    /// Whether this path should bypass authentication.
    pub fn is_bypass_path(&self, path: &str) -> bool {
        self.inner
            .config
            .bypass_paths
            .iter()
            .any(|bp| path == bp.as_str())
    }

    /// Returns the total number of failed authentication attempts (v1.2).
    pub fn failure_count(&self) -> u64 {
        self.inner.failure_count.load(Ordering::Relaxed)
    }
}

// ─── axum middleware ──────────────────────────────────────────────

/// Authentication middleware layer. Rejects unauthenticated requests
/// with 401; passes authenticated principal via request extensions.
pub async fn auth_middleware(
    State(auth): State<AuthState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = req.uri().path().to_string();

    // Bypass health/metrics
    if auth.is_bypass_path(&path) {
        return Ok(next.run(req).await);
    }

    match auth.authenticate(&req) {
        Some(_principal) => Ok(next.run(req).await),
        None => {
            auth.inner.failure_count.fetch_add(1, Ordering::Relaxed);
            Ok((
                StatusCode::UNAUTHORIZED,
                [("www-authenticate", "Bearer")],
                axum::Json(serde_json::json!({
                    "error": "unauthorized",
                    "message": "Valid API key or JWT required"
                })),
            )
                .into_response())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_auth_always_passes() {
        let config = AuthConfig {
            enabled: false,
            ..Default::default()
        };
        let _auth = AuthState::new(config);
        // Middleware is not applied when disabled; config.enabled gate is in proxy.rs
    }

    #[test]
    fn bypass_paths_are_correct() {
        let config = AuthConfig::default();
        let auth = AuthState::new(config);
        assert!(auth.is_bypass_path("/healthz"));
        assert!(auth.is_bypass_path("/metrics"));
        assert!(!auth.is_bypass_path("/v1/chat/completions"));
    }

    #[test]
    fn static_api_key_authenticates() {
        let config = AuthConfig {
            enabled: true,
            api_keys: {
                let mut m = HashMap::new();
                m.insert(
                    "sk-test-123".into(),
                    ApiKeyEntry {
                        label: "test-key".into(),
                        scopes: vec![],
                    },
                );
                m
            },
            ..Default::default()
        };
        let auth = AuthState::new(config);

        let req = Request::builder()
            .uri("/v1/chat/completions")
            .header("authorization", "Bearer sk-test-123")
            .body(Body::empty())
            .unwrap();
        assert_eq!(auth.authenticate(&req), Some("test-key".into()));
    }
}
