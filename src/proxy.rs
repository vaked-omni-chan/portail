use crate::AppState;
use crate::cdn;
use crate::gateway;
use crate::hooks;
use crate::mcp;
use crate::types::BoundedMeta;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, delete, get};
use axum::{Json, Router};
pub use cdn::CacheManager;
use metrics::{counter, histogram};
use serde_json::json;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tower_http::cors::{Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;
use utoipa_axum::{router::OpenApiRouter, routes};

const MAX_BODY_BYTES: usize = 10 * 1024 * 1024; // 10MB

static API_SPEC_JSON: OnceLock<serde_json::Value> = OnceLock::new();

pub fn build_router(state: Arc<AppState>) -> Router {
    // ── OpenApiRouter: collects utoipa-annotated routes for auto-documentation ──
    let (api_router, api) = OpenApiRouter::new()
        .routes(routes!(healthz))
        .routes(routes!(readyz))
        .routes(routes!(metrics_handler))
        .routes(routes!(stats_handler))
        .routes(routes!(dashboard_handler))
        .routes(routes!(supervisor_handler))
        .routes(routes!(sessions_handler))
        .routes(routes!(session_detail_handler))
        .routes(routes!(loop_status_handler))
        .routes(routes!(loop_run_handler))
        .routes(routes!(pkg_ctx_search_handler))
        .routes(routes!(pkg_ctx_list_handler))
        .split_for_parts();

    // Store OpenAPI spec for the /api-docs endpoint
    let mut api_json = serde_json::to_value(&api).unwrap();
    // Override default metadata (utoipa-axum emits its own info)
    if let Some(ref mut info) = api_json.get_mut("info") {
        info["title"] = serde_json::json!("Portail API");
        info["description"] = serde_json::json!(
            "Unified proxy/gateway: AI Gateway + MCP Gateway + CDN cache + \
             Agent protocol + DNS + Observability"
        );
        info["version"] = serde_json::json!(env!("CARGO_PKG_VERSION"));
        info["contact"] = serde_json::json!(null);
        info["license"] = serde_json::json!(null);
    }
    let _ = API_SPEC_JSON.set(api_json);

    // ── Build the full axum::Router with all routes + middleware ──
    let mut router = api_router
        // Non-annotated routes (no OpenAPI docs — yet, can be added incrementally)
        .route("/livez", get(healthz))
        .route("/v1/messages", any(route_to_ai_gateway))
        .route("/v1/chat/completions", any(route_to_ai_gateway))
        .route("/v1/responses", any(route_to_ai_gateway))
        .route("/v1/embeddings", any(route_to_ai_gateway))
        .route("/v1/audio/{*path}", any(route_to_ai_gateway))
        .route("/v1/images/{*path}", any(route_to_ai_gateway))
        .route("/v1beta/{*path}", any(route_to_ai_gateway))
        .route("/cdn/{*path}", any(route_cdn))
        .route("/mcp/{*path}", any(route_mcp))
        .route("/mcp-rest/{*path}", any(route_mcp))
        .route(
            "/events",
            get(crate::events::handle_recent).post(crate::events::handle_publish),
        )
        .route("/events/stream", get(crate::events::handle_stream))
        .route(
            "/hooks",
            get(crate::hooks::handle_list).post(crate::hooks::handle_create),
        )
        .route("/hooks/{id}", delete(crate::hooks::handle_delete))
        .route(
            "/.well-known/agent.json",
            get(crate::a2a::handle_agent_card),
        )
        .route("/a2a", axum::routing::post(crate::a2a::handle_rpc))
        .route(
            "/a2a/agents",
            axum::routing::get(crate::a2a::registry::handle_list),
        )
        .route(
            "/a2a/agents",
            axum::routing::post(crate::a2a::registry::handle_register),
        )
        .route("/a2a/agents/{id}", get(crate::a2a::registry::handle_get))
        .route(
            "/a2a/agents/{id}",
            delete(crate::a2a::registry::handle_deregister),
        )
        // Task creation is handled via JSON-RPC `tasks/send` at POST /a2a.
        .route("/a2c/chat", axum::routing::post(crate::a2c::handle_chat))
        // ── v4: local inference (OpenAI-compatible) ──
        .route(
            "/v1/models",
            axum::routing::get(crate::local_inference::handle_list_models),
        )
        .route(
            "/v1/health",
            axum::routing::get(crate::local_inference::handle_health),
        );

    if state.inference_engine.is_some() {
        router = router.route(
            "/v1/chat/completions",
            axum::routing::post(crate::local_inference::handle_chat_completions),
        );
    }

    router = router
        // ── v0.2: plugin & diagnostics routers ──
        .merge(crate::ci::router())
        .merge(crate::discovery::router())
        .merge(crate::dns::router())
        .merge(crate::plugins::tinyurl::router())
        .merge(crate::plugins::tracer::router())
        .merge(crate::plugins::redis_cache::router())
        .merge(crate::godfather::router())
        .merge(crate::graphql::router())
        .merge(crate::file_cache::router_with_state())
        .fallback(route_to_ai_gateway)
        // Decorating middleware (inner → outer)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            session_middleware,
        ))
        .layer(middleware::from_fn(security_headers_middleware))
        .layer(middleware::from_fn(request_id_middleware))
        .layer(middleware::from_fn(metrics_middleware))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(
            TraceLayer::new_for_http()
                .on_request(|req: &Request<Body>, _span: &tracing::Span| {
                    tracing::debug!(method = %req.method(), uri = %req.uri(), "request");
                })
                .on_response(
                    |resp: &Response, latency: std::time::Duration, _span: &tracing::Span| {
                        tracing::info!(
                            status = resp.status().as_u16(),
                            latency_us = latency.as_micros() as u64,
                            "request completed"
                        );
                    },
                ),
        );

    // ── v0.2: auth (before rate limit so per-key limits work) ──
    if state.auth_state.is_some() {
        router = router.layer(middleware::from_fn_with_state(
            state.auth_state.clone().unwrap(),
            crate::auth::auth_middleware,
        ));
    }

    // ── v0.2: rate limit ──
    if state.rate_limiter.is_some() {
        router = router.layer(middleware::from_fn_with_state(
            state.rate_limiter.clone().unwrap(),
            crate::rate_limit::rate_limit_middleware,
        ));
    }

    router
        .route("/api-docs/openapi.json", get(openapi_json))
        .route("/api-docs/", get(scalar_ui_html))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
                .allow_headers(Any),
        )
        .with_state(state)
}

async fn request_id_middleware(mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok().map(String::from))
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    req.extensions_mut().insert(id.clone());
    let mut resp = next.run(req).await;
    resp.headers_mut().insert(
        "x-request-id",
        axum::http::HeaderValue::from_str(&id).unwrap(),
    );
    resp
}

async fn metrics_middleware(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let start = Instant::now();
    let resp = next.run(req).await;
    let latency = start.elapsed();
    let status = resp.status();
    let path_normalized = normalize_path(&path);

    counter!("http_requests_total",
        "method" => method.to_string(),
        "path" => path_normalized.clone(),
        "status" => status.as_u16().to_string(),
    )
    .increment(1);

    histogram!("http_request_duration_seconds",
        "method" => method.to_string(),
        "path" => path_normalized,
        "status" => status.as_u16().to_string(),
    )
    .record(latency.as_secs_f64());

    resp
}

// ── Session recording middleware ──────────────────────────────────

async fn session_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let session_id = req
        .headers()
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let start = Instant::now();

    let resp = next.run(req).await;

    let total_latency = start.elapsed();
    let portail_overhead = Duration::from_micros(500);
    let status = resp.status().as_u16();

    if !session_id.is_empty() {
        let input_tokens: u64 = resp
            .headers()
            .get("x-tokens-input")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let output_tokens: u64 = resp
            .headers()
            .get("x-tokens-output")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let cache_hit = resp
            .headers()
            .get("x-cache-status")
            .map(|v| v.as_bytes() == b"HIT")
            .unwrap_or(false);
        let hooks_applied: u64 = resp
            .headers()
            .get("x-hooks-applied")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        state.session_store.record_request(
            &session_id,
            &method,
            &path,
            status,
            total_latency,
            portail_overhead,
            input_tokens,
            output_tokens,
            cache_hit,
            hooks_applied,
        );
    }

    resp
}

async fn security_headers_middleware(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();

    // HSTS: Force HTTPS for 1 year
    headers.insert(
        "strict-transport-security",
        axum::http::HeaderValue::from_static("max-age=31536000; includeSubDomains; preload"),
    );

    // Security headers
    headers.insert(
        "x-content-type-options",
        axum::http::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "x-frame-options",
        axum::http::HeaderValue::from_static("DENY"),
    );
    headers.insert(
        "x-xss-protection",
        axum::http::HeaderValue::from_static("1; mode=block"),
    );
    headers.insert(
        "referrer-policy",
        axum::http::HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    headers.insert(
        "permissions-policy",
        axum::http::HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );

    resp
}

fn normalize_path(path: &str) -> String {
    // Normalize dynamic path segments to reduce cardinality
    let segments: Vec<&str> = path.split('/').collect();
    let normalized: Vec<String> = segments
        .iter()
        .map(|s| {
            // Replace UUIDs and long IDs with placeholders
            if s.len() > 20 && s.contains('-') && s.chars().all(|c| c.is_alphanumeric() || c == '-')
            {
                "{id}".to_string()
            } else if s.len() > 32 && s.chars().all(|c| c.is_alphanumeric()) {
                "{hash}".to_string()
            } else {
                s.to_string()
            }
        })
        .collect();
    normalized.join("/")
}

#[utoipa::path(
    get,
    path = "/healthz",
    tag = "health",
    responses((status = 200, description = "Liveness check passes"))
)]
async fn healthz() -> &'static str {
    counter!("health_checks").increment(1);
    "ok"
}

#[utoipa::path(
    get,
    path = "/readyz",
    tag = "health",
    responses(
        (status = 200, description = "Readiness check passes — upstream AI gateway is reachable"),
        (status = 503, description = "AI gateway not ready"),
    )
)]
async fn readyz(State(state): State<Arc<AppState>>) -> (StatusCode, &'static str) {
    let upstream = {
        let c = state.config.read().unwrap();
        c.ai_gateway
            .as_ref()
            .filter(|g| g.enabled)
            .map(|g| g.upstream.clone())
    };
    let ready = match upstream {
        Some(url) => {
            let ok = reqwest::get(format!("{url}/healthz"))
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if !ok {
                return (StatusCode::SERVICE_UNAVAILABLE, "ai gateway not ready");
            }
            true
        }
        None => true,
    };
    if ready {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

async fn route_to_ai_gateway(State(state): State<Arc<AppState>>, req: Request) -> Response {
    // Resolve target from config
    let (upstream, provider, target_api_key) = {
        let c = state.config.read().unwrap();
        let cfg = c.ai_gateway.as_ref().filter(|g| g.enabled);
        let provider_header = req
            .headers()
            .get("x-provider")
            .and_then(|v| v.to_str().ok());
        let body: Option<&serde_json::Value> = None; // lazy — only parse if needed for model routing

        match cfg {
            Some(g) => {
                // Try target template routing
                let targets = &c.targets;
                let resolved = if targets.is_empty() {
                    crate::target_router::ResolvedTarget::NotFound
                } else {
                    crate::target_router::resolve_upstream(
                        targets,
                        g.default_provider.as_deref(),
                        provider_header,
                        body,
                    )
                };
                match resolved {
                    crate::target_router::ResolvedTarget::Found(t) => (
                        t.base_url.clone(),
                        Some(t.provider.clone()),
                        t.api_key.clone(),
                    ),
                    crate::target_router::ResolvedTarget::NotFound => {
                        (g.upstream.clone(), None, None)
                    }
                }
            }
            None => {
                return (StatusCode::NOT_IMPLEMENTED, "ai gateway disabled").into_response();
            }
        }
    };

    let mut req = req;
    if let Some(ref key) = target_api_key {
        let resolved_key = if let Some(var_name) = key.strip_prefix('$') {
            std::env::var(var_name).unwrap_or_else(|_| key.clone())
        } else {
            key.clone()
        };
        if let Ok(hv) = axum::http::HeaderValue::from_str(&format!("Bearer {resolved_key}")) {
            req.headers_mut()
                .insert(axum::http::header::AUTHORIZATION, hv);
        }
    }

    let path = req.uri().path().to_string();
    let matching_hooks = state.hooks.match_message(&path);

    // Call .vaked plugin hooks (pre_request)
    let plugin_result = crate::plugin_hooks::call_plugin_hooks(
        &state.plugin_registry,
        portail_plugin_sdk::HookPoint::PreRequest,
        &path,
        provider.as_deref(),
        None,
        None,
        std::collections::HashMap::new(),
    );
    if let Some(status) = plugin_result.abort_status {
        return (
            StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN),
            plugin_result.abort_message.unwrap_or_default(),
        )
            .into_response();
    }

    let provider_name = provider.as_deref().unwrap_or("openai");
    let is_openai_compat = matches!(provider_name, "openai" | "deepseek" | "");

    let result = if matching_hooks.is_empty() {
        if is_openai_compat {
            gateway::forward(&upstream, req).await
        } else {
            // Provider schema adaptation needed
            let (parts, body) = req.into_parts();
            let body_bytes = axum::body::to_bytes(body, 10_000_000)
                .await
                .unwrap_or_default();
            gateway::forward_adapted(&upstream, provider_name, parts, body_bytes).await
        }
    } else {
        let (parts, body) = req.into_parts();
        let body_bytes = axum::body::to_bytes(body, 10_000_000)
            .await
            .unwrap_or_default();

        let modified = serde_json::from_slice::<serde_json::Value>(&body_bytes)
            .ok()
            .and_then(|v| hooks::apply_message_hooks(&v, &matching_hooks))
            .and_then(|v| serde_json::to_vec(&v).ok())
            .unwrap_or(body_bytes.to_vec());

        counter!("hook_injections").increment(matching_hooks.len() as u64);
        state.event_log.publish(crate::events::AgentEvent {
            agent_id: "hooks".into(),
            event_type: "injected".into(),
            severity: "info".into(),
            timestamp: 0,
            metadata: BoundedMeta::from_iter([
                ("path".into(), path),
                ("count".into(), matching_hooks.len().to_string()),
            ]),
        });

        if is_openai_compat {
            gateway::forward_with_body(&upstream, parts, modified.into()).await
        } else {
            gateway::forward_adapted(&upstream, provider_name, parts, modified.into()).await
        }
    };

    // Tag response with provider info
    if let Some(p) = provider {
        let _ = p; // future: inject x-provider response header
    }
    result
}

async fn route_cdn(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let Some(cache) = &state.cdn_cache else {
        return (StatusCode::NOT_IMPLEMENTED, "cdn disabled").into_response();
    };
    let origin = {
        let c = state.config.read().unwrap();
        c.cdn.as_ref().map(|c| c.origin.clone())
    };
    cdn::handle(req, Arc::clone(cache), origin).await
}

async fn route_mcp(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let socket = {
        let c = state.config.read().unwrap();
        c.mcp
            .as_ref()
            .filter(|m| m.enabled)
            .map(|m| m.socket_path.clone())
    };
    let Some(socket_path) = socket else {
        return (StatusCode::NOT_IMPLEMENTED, "mcp disabled").into_response();
    };
    mcp::proxy_to_sidecar(&socket_path, req).await
}

#[utoipa::path(
    get,
    path = "/metrics",
    tag = "observability",
    responses((status = 200, description = "Prometheus metrics in text/plain format"))
)]
async fn metrics_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let body = state.metrics_handle.render();
    (
        StatusCode::OK,
        [("content-type", "text/plain; charset=utf-8")],
        body,
    )
}

#[utoipa::path(
    get,
    path = "/stats",
    tag = "observability",
    responses((status = 200, description = "Server statistics including CDN cache and version"))
)]
async fn stats_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cdn_stats: serde_json::Value = state
        .cdn_cache
        .as_ref()
        .map(|c| serde_json::to_value(c.stats()).unwrap_or_default())
        .unwrap_or_default();
    json!({ "cdn": cdn_stats, "version": env!("CARGO_PKG_VERSION") }).into()
}

// ── v1.2: dashboard health snapshot ────────────────────────────────

#[utoipa::path(
    get,
    path = "/dashboard",
    tag = "observability",
    responses((status = 200, description = "Live health snapshot as JSON"))
)]
async fn dashboard_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let rate_denied = state
        .rate_limiter
        .as_ref()
        .map(|r| r.denied_count())
        .unwrap_or(0);
    let auth_failures = state
        .auth_state
        .as_ref()
        .map(|a| a.failure_count())
        .unwrap_or(0);
    let config_healthy = state.config_watcher.is_healthy();
    let config_error = state.config_watcher.last_error.read().await.clone();

    let cdn_stats: serde_json::Value = state
        .cdn_cache
        .as_ref()
        .map(|c| serde_json::to_value(c.stats()).unwrap_or_default())
        .unwrap_or_default();

    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "config_healthy": config_healthy,
        "config_error": config_error,
        "rate_limit_denied": rate_denied,
        "auth_failures": auth_failures,
        "cdn": cdn_stats,
    })
    .into()
}

// ── Supervisor handler (v2.0) ──────────────────────────────────────

#[utoipa::path(
    get,
    path = "/supervisor/status",
    tag = "observability",
    responses((status = 200, description = "List of supervised background tasks and their status"))
)]
async fn supervisor_handler(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<crate::supervisor::TaskStatus>> {
    Json(state.supervisor.status())
}

// ── Session handlers ──────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/sessions",
    tag = "sessions",
    responses((status = 200, description = "List of active API sessions"))
)]
async fn sessions_handler(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<crate::sessions::SessionSummary>> {
    Json(state.session_store.list_sessions())
}

#[utoipa::path(
    get,
    path = "/sessions/{id}",
    tag = "sessions",
    responses(
        (status = 200, description = "Session stats for the given session ID"),
        (status = 404, description = "Session not found"),
    )
)]
async fn session_detail_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<crate::sessions::SessionStats>, StatusCode> {
    state
        .session_store
        .get_session(&id)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use std::sync::RwLock;
    use tower::ServiceExt;

    fn test_state() -> Arc<AppState> {
        Arc::new(AppState {
            config: RwLock::new(crate::config::Config::default()),
            event_log: Arc::new(crate::events::EventLog::new(100)),
            cdn_cache: None,
            hooks: Arc::new(crate::hooks::HookStore::new()),
            a2a_tasks: Arc::new(crate::a2a::TaskStore::new()),
            dns_store: Arc::new(crate::dns::DnsStore::new()),
            doh_client: None,
            network_isolation: Arc::new(crate::dns::NetworkIsolation::default()),
            tinyurl: Arc::new(crate::plugins::TinyUrlStore::new(
                crate::plugins::TinyUrlConfig::default(),
            )),
            trace_store: Arc::new(crate::plugins::TraceStore::new(100)),
            redis_cache: Arc::new(crate::plugins::RedisCache::new(
                crate::plugins::RedisCacheConfig::default(),
            )),
            discovery: Arc::new(crate::discovery::DiscoveryStore::new(
                crate::discovery::DiscoveryConfig::default(),
            )),
            ci_status: Arc::new(crate::ci::CiStatusStore::new(100, None)),
            metrics_handle: crate::test_utils::global_metrics().clone(),
            rate_limiter: None,
            auth_state: None,
            event_store: None,
            session_store: crate::sessions::SessionStore::new(20),
            file_cache: crate::file_cache::FileCache::new(&crate::file_cache::FileCacheConfig {
                path: "/tmp/portail-test-cache".into(),
                ..Default::default()
            }),
            config_watcher: crate::config_watcher::ConfigWatcher::new(std::path::PathBuf::from(
                "portail.toml",
            )),
            supervisor: std::sync::Arc::new(crate::supervisor::Supervisor::new(
                std::sync::Arc::new(crate::events::EventLog::new(100)),
            )),
            plugin_registry: crate::plugin_hooks::init_plugin_registry(&std::path::Path::new(
                "vaked",
            )),
            loop_manager: std::sync::Arc::new(loop_state_manager::LoopStateManager::new("3.0.0")),
            loop_runner: loopeng::SharedLoopEngine::new(loopeng::LoopEngineConfig::default()),
            inference_engine: None,
            pkg_ctx_memory: tokio::sync::Mutex::new(pkg_ctx::memory::PkgCtxMemory::new().unwrap()),
            a2a_registry: Arc::new(crate::a2a::registry::AgentRegistry::new()),
        })
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn livez_returns_ok() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/livez")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_no_upstream() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/readyz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn stats_returns_json() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/stats")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cdn_disabled_returns_not_implemented() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/cdn/foo")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn ai_gateway_disabled_returns_not_implemented() {
        let state = test_state();
        state.config.write().unwrap().ai_gateway = None;
        let app = build_router(state);
        let req = Request::builder()
            .uri("/v1/chat/completions")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn mcp_disabled_returns_not_implemented() {
        let state = test_state();
        state.config.write().unwrap().mcp = None;
        let app = build_router(state);
        let req = Request::builder()
            .uri("/mcp/tools/list")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn request_id_injected() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(resp.headers().get("x-request-id").is_some());
    }

    #[tokio::test]
    async fn request_id_preserved() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/healthz")
            .header("x-request-id", "test-id-123")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.headers()
                .get("x-request-id")
                .unwrap()
                .to_str()
                .unwrap(),
            "test-id-123"
        );
    }

    #[tokio::test]
    async fn metrics_returns_prometheus() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await;
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 100_000)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("health_checks") || text.contains("http_requests"));
    }

    #[tokio::test]
    async fn metrics_records_counter() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await;
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 100_000)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("health_checks") || text.contains("http_requests"));
    }

    #[tokio::test]
    async fn hooks_create_list_delete() {
        let app = build_router(test_state());
        let hook = serde_json::json!({
            "id": "test-hook",
            "match_path": "/chat",
            "inject": "prepend",
            "content": "be helpful",
            "enabled": true
        });
        let req = Request::builder()
            .uri("/hooks")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&hook).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn event_stream_returns_ok() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/events/stream")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn events_recent_accepts_event() {
        let state = test_state();
        state.event_log.publish(crate::events::AgentEvent {
            agent_id: "test".into(),
            event_type: "ping".into(),
            severity: "info".into(),
            timestamp: 1,
            metadata: BoundedMeta::default(),
        });
        let app = build_router(state);
        let req = Request::builder()
            .uri("/events")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 100_000)
            .await
            .unwrap();
        let events: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(!events.as_array().unwrap().is_empty());
    }

    // ── A2A integration tests ────────────────────────────────────

    #[tokio::test]
    async fn a2a_agent_card_returns_valid_json() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/.well-known/agent.json")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 100_000)
            .await
            .unwrap();
        let card: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(card["name"], "portail");
        assert!(card["capabilities"]["streaming"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn a2a_rpc_tasks_send_returns_ok() {
        let app = build_router(test_state());
        let rpc = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tasks/send",
            "params": {
                "message": {"role": "user", "parts": [{"type": "text", "text": "hello"}]}
            }
        });
        let req = Request::builder()
            .uri("/a2a")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&rpc).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 100_000)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val["jsonrpc"], "2.0");
        assert_eq!(val["id"], 1);
        assert!(val["result"].is_object());
    }

    #[tokio::test]
    async fn a2a_rpc_task_not_found() {
        let app = build_router(test_state());
        let rpc = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tasks/get",
            "params": {"id": "nonexistent"}
        });
        let req = Request::builder()
            .uri("/a2a")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&rpc).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 100_000)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(val["error"].is_object());
        assert_eq!(val["error"]["code"], -32001);
    }

    #[tokio::test]
    async fn a2a_rpc_method_not_found() {
        let app = build_router(test_state());
        let rpc = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "bogus/method",
            "params": {}
        });
        let req = Request::builder()
            .uri("/a2a")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&rpc).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 100_000)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val["error"]["code"], -32601);
    }
}

// ── Loop handlers ────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/v1/loop/status",
    tag = "loop",
    responses((status = 200, description = "Loop engine status: schedules, runs, circuit breaker, memory"))
)]
async fn loop_status_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let status = state.loop_runner.with_engine(|e| {
        serde_json::json!({
            "schedules": e.schedules().len(),
            "runs": e.runs().len(),
            "circuit_open": e.is_circuit_open(),
            "consecutive_failures": e.consecutive_failures(),
            "memory_entries": e.memory_entries().len(),
            "sub_agents": e.sub_agents().len(),
            "skills": e.skills().len(),
        })
    });
    Json(status)
}

#[utoipa::path(
    post,
    path = "/v1/loop/run/{schedule}",
    tag = "loop",
    responses(
        (status = 200, description = "Loop iteration completed"),
        (status = 400, description = "Invalid schedule name or run failed"),
    )
)]
async fn loop_run_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(schedule): axum::extract::Path<String>,
) -> Response {
    let result = state.loop_runner.run_iteration(&schedule).await;
    match result {
        Ok(run) => Json(serde_json::json!({
            "id": run.id,
            "status": format!("{:?}", run.status),
            "phase": run.phase.to_string(),
            "token_cost": run.token_cost,
            "artifacts": run.artifacts,
        }))
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ── pkg-ctx handlers ─────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/v1/pkg-ctx/search",
    tag = "pkg-ctx",
    responses(
        (status = 200, description = "Search results from documentation context"),
        (status = 400, description = "Missing library or topic parameter"),
    )
)]
async fn pkg_ctx_search_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let library = body.get("library").and_then(|v| v.as_str()).unwrap_or("");
    let topic = body.get("topic").and_then(|v| v.as_str()).unwrap_or("");
    if library.is_empty() || topic.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing library or topic").into_response();
    }
    let results = {
        let mem = state.pkg_ctx_memory.lock().await;
        mem.search(topic, 10).await
    };
    let results = match results {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let text = pkg_ctx::search::format_search_results(&results, library, topic);
    Json(serde_json::json!({ "result": text, "count": results.len() })).into_response()
}

#[utoipa::path(
    get,
    path = "/v1/pkg-ctx/list",
    tag = "pkg-ctx",
    responses((status = 200, description = "List of indexed packages available for context search"))
)]
async fn pkg_ctx_list_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let summary = state.pkg_ctx_memory.lock().await.summarize();
    Json(serde_json::json!({ "summary": summary }))
}

// ── OpenAPI / Swagger ────────────────────────────────────────────

/// Returns the auto-generated OpenAPI 3.1 spec from utoipa annotations.
async fn openapi_json() -> Json<&'static serde_json::Value> {
    Json(API_SPEC_JSON.get().expect("API_SPEC_JSON not initialized"))
}

/// Serves the Scalar API Reference UI at /api-docs/.
async fn scalar_ui_html() -> impl IntoResponse {
    Html(include_str!("../docs/api-docs/scalar.html"))
}
