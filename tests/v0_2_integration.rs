#[cfg(test)]
mod v0_2_integration {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::RwLock;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use portail::*;

    // ── Test helpers ─────────────────────────────────────────────

    fn base_app_state() -> AppState {
        AppState {
            config: RwLock::new(config::Config::default()),
            event_log: Arc::new(events::EventLog::new(100)),
            cdn_cache: None,
            hooks: Arc::new(hooks::HookStore::new()),
            a2a_tasks: Arc::new(a2a::TaskStore::new()),
            dns_store: Arc::new(dns::DnsStore::new()),
            doh_client: None,
            network_isolation: Arc::new(dns::NetworkIsolation::default()),
            tinyurl: Arc::new(plugins::TinyUrlStore::new(plugins::TinyUrlConfig::default())),
            trace_store: Arc::new(plugins::TraceStore::new(100)),
            redis_cache: Arc::new(plugins::RedisCache::new(
                plugins::RedisCacheConfig::default(),
            )),
            discovery: Arc::new(discovery::DiscoveryStore::new(
                discovery::DiscoveryConfig::default(),
            )),
            ci_status: Arc::new(ci::CiStatusStore::new(100, None)),
            metrics_handle: test_utils::global_metrics().clone(),
            rate_limiter: None,
            auth_state: None,
            event_store: None,
            session_store: sessions::SessionStore::new(20),
            file_cache: portail::file_cache::FileCache::new(
                &portail::file_cache::FileCacheConfig {
                    path: "/tmp/portail-test-cache".into(),
                    ..Default::default()
                },
            ),
            config_watcher: portail::config_watcher::ConfigWatcher::new(std::path::PathBuf::from(
                "portail.toml",
            )),
            supervisor: std::sync::Arc::new(portail::supervisor::Supervisor::new(
                std::sync::Arc::new(portail::events::EventLog::new(100)),
            )),
            plugin_registry: portail::plugin_hooks::init_plugin_registry(std::path::Path::new(
                "vaked",
            )),
            loop_manager: std::sync::Arc::new(loop_state_manager::LoopStateManager::new("3.0.0")),
            loop_runner: loopeng::SharedLoopEngine::new(loopeng::LoopEngineConfig::default()),
            inference_engine: None,
            pkg_ctx_memory: tokio::sync::Mutex::new(pkg_ctx::memory::PkgCtxMemory::new().unwrap()),
            a2a_registry: Arc::new(a2a::registry::AgentRegistry::new()),
        }
    }

    fn state_with_rate_limiter(enabled: bool, burst: u32, per_second: f64) -> Arc<AppState> {
        let mut state = base_app_state();
        state.rate_limiter = if enabled {
            Some(rate_limit::RateLimiter::new(rate_limit::RateLimitConfig {
                enabled: true,
                burst,
                per_second,
                ..Default::default()
            }))
        } else {
            None
        };
        Arc::new(state)
    }

    fn state_with_auth(api_keys: HashMap<String, auth::ApiKeyEntry>) -> Arc<AppState> {
        let mut state = base_app_state();
        state.auth_state = Some(auth::AuthState::new(auth::AuthConfig {
            enabled: true,
            api_keys,
            ..Default::default()
        }));
        Arc::new(state)
    }

    // ── 1. Rate limiting integration test ────────────────────────

    #[tokio::test]
    async fn rate_limit_blocks_after_burst() {
        let state = state_with_rate_limiter(true, 3, 100.0);
        let app = proxy::build_router(state);

        // First 3 requests should pass rate limit (handler returns 501 for disabled gateway)
        for _ in 0..3 {
            let req = Request::builder()
                .uri("/v1/chat/completions")
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_ne!(
                resp.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "request within burst should not be rate limited"
            );
        }

        // 4th request should be rate limited with Retry-After header
        let req = Request::builder()
            .uri("/v1/chat/completions")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            resp.headers().contains_key("retry-after"),
            "429 response must include retry-after header"
        );

        // 5th request should also be rate limited
        let req = Request::builder()
            .uri("/v1/chat/completions")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn disabled_rate_limit_passes_all() {
        let state = state_with_rate_limiter(false, 3, 100.0);
        let app = proxy::build_router(state);

        for _ in 0..20 {
            let req = Request::builder()
                .uri("/v1/chat/completions")
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_ne!(
                resp.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "disabled rate limiter should never return 429"
            );
        }
    }

    // ── 2. Auth integration test ─────────────────────────────────

    #[tokio::test]
    async fn auth_no_header_returns_401() {
        let mut api_keys = HashMap::new();
        api_keys.insert(
            "sk-test-123".into(),
            auth::ApiKeyEntry {
                label: "test-key".into(),
                scopes: vec![],
            },
        );
        let state = state_with_auth(api_keys);
        let app = proxy::build_router(state);

        let req = Request::builder()
            .uri("/v1/chat/completions")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(
            resp.headers().contains_key("www-authenticate"),
            "401 response must include www-authenticate header"
        );
    }

    #[tokio::test]
    async fn auth_wrong_key_returns_401() {
        let mut api_keys = HashMap::new();
        api_keys.insert(
            "sk-test-123".into(),
            auth::ApiKeyEntry {
                label: "test-key".into(),
                scopes: vec![],
            },
        );
        let state = state_with_auth(api_keys);
        let app = proxy::build_router(state);

        let req = Request::builder()
            .uri("/v1/chat/completions")
            .header("authorization", "Bearer sk-wrong-key")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_correct_key_passes() {
        let mut api_keys = HashMap::new();
        api_keys.insert(
            "sk-test-123".into(),
            auth::ApiKeyEntry {
                label: "test-key".into(),
                scopes: vec![],
            },
        );
        let state = state_with_auth(api_keys);
        let app = proxy::build_router(state);

        let req = Request::builder()
            .uri("/v1/chat/completions")
            .header("authorization", "Bearer sk-test-123")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        // Auth passes, handler returns 501 (ai gateway disabled) — but NOT 401
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn healthz_bypasses_auth() {
        let mut api_keys = HashMap::new();
        api_keys.insert(
            "sk-test-123".into(),
            auth::ApiKeyEntry {
                label: "test-key".into(),
                scopes: vec![],
            },
        );
        let state = state_with_auth(api_keys);
        let app = proxy::build_router(state);

        // /healthz without any auth header should return 200 (bypasses auth)
        let req = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn metrics_bypasses_auth() {
        let mut api_keys = HashMap::new();
        api_keys.insert(
            "sk-test-123".into(),
            auth::ApiKeyEntry {
                label: "test-key".into(),
                scopes: vec![],
            },
        );
        let state = state_with_auth(api_keys);
        let app = proxy::build_router(state);

        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── 3. Event store integration test ──────────────────────────

    fn in_memory_store() -> store::EventStore {
        use portail::store::RusqliteBackend;
        let config = store::StoreConfig {
            enabled: true,
            db_path: ":memory:".into(),
            retention_days: 0,
            provider: "rusqlite".into(),
        };
        let backend =
            std::sync::Arc::new(RusqliteBackend::open(&config).expect("open in-memory store"));
        store::EventStore::from_backend(backend, config)
    }

    fn make_event(agent_id: &str, event_type: &str, timestamp: i64) -> store::StoredEvent {
        store::StoredEvent {
            id: None,
            agent_id: agent_id.into(),
            event_type: event_type.into(),
            severity: "info".into(),
            timestamp,
            metadata_json: "{}".into(),
        }
    }

    #[tokio::test]
    async fn event_store_insert_5_and_count() {
        let store = in_memory_store();
        for i in 0..5 {
            store
                .insert(&make_event(
                    &format!("agent-{}", i % 2),
                    if i < 3 {
                        "task.started"
                    } else {
                        "task.completed"
                    },
                    1700000000 + i,
                ))
                .unwrap();
        }
        assert_eq!(store.count().unwrap(), 5);
    }

    #[tokio::test]
    async fn event_store_query_by_agent_id() {
        let store = in_memory_store();
        store
            .insert(&make_event("agent-a", "task.started", 1700000000))
            .unwrap();
        store
            .insert(&make_event("agent-a", "task.completed", 1700000001))
            .unwrap();
        store
            .insert(&make_event("agent-b", "task.started", 1700000002))
            .unwrap();

        let a_events = store.query(Some("agent-a"), None, None, Some(10)).unwrap();
        assert_eq!(a_events.len(), 2);
        assert!(a_events.iter().all(|e| e.agent_id == "agent-a"));

        let b_events = store.query(Some("agent-b"), None, None, Some(10)).unwrap();
        assert_eq!(b_events.len(), 1);
        assert_eq!(b_events[0].agent_id, "agent-b");
    }

    #[tokio::test]
    async fn event_store_query_by_event_type() {
        let store = in_memory_store();
        store
            .insert(&make_event("agent-1", "task.started", 1700000000))
            .unwrap();
        store
            .insert(&make_event("agent-2", "task.started", 1700000001))
            .unwrap();
        store
            .insert(&make_event("agent-3", "task.failed", 1700000002))
            .unwrap();

        let started = store
            .query(None, Some("task.started"), None, Some(10))
            .unwrap();
        assert_eq!(started.len(), 2);

        let failed = store
            .query(None, Some("task.failed"), None, Some(10))
            .unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].severity, "info");
    }

    #[tokio::test]
    async fn event_store_export_json() {
        let store = in_memory_store();
        store
            .insert(&make_event("agent-x", "test.run", 1700000000))
            .unwrap();
        store
            .insert(&make_event("agent-y", "test.pass", 1700000001))
            .unwrap();

        let json = store.export_json(None).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["agent_id"], "agent-y"); // newest first
        assert_eq!(parsed[1]["event_type"], "test.run");
    }

    #[tokio::test]
    async fn event_store_empty_returns_zero() {
        let store = in_memory_store();
        assert_eq!(store.count().unwrap(), 0);
        let json = store.export_json(None).unwrap();
        assert_eq!(json, "[]");
    }

    // ── 4. Full pipeline integration test ────────────────────────

    #[tokio::test]
    async fn full_pipeline_health_bypass_auth_required_rate_limit_exhausted_recovery() {
        // Setup: burst=2 rate limit + auth with one valid key
        let mut state = base_app_state();

        let mut api_keys = HashMap::new();
        api_keys.insert(
            "sk-test-123".into(),
            auth::ApiKeyEntry {
                label: "test-key".into(),
                scopes: vec![],
            },
        );
        state.auth_state = Some(auth::AuthState::new(auth::AuthConfig {
            enabled: true,
            api_keys,
            ..Default::default()
        }));

        state.rate_limiter = Some(rate_limit::RateLimiter::new(rate_limit::RateLimitConfig {
            enabled: true,
            burst: 2,
            per_second: 100.0,
            ..Default::default()
        }));

        let state = Arc::new(state);
        let app = proxy::build_router(state);

        // 1. Health bypass: /healthz bypasses auth, passes rate limit → 200
        let req = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "/healthz should bypass auth and return 200"
        );

        // 2. Auth required: no auth header, rate limit passes → 401
        let req = Request::builder()
            .uri("/v1/chat/completions")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "missing auth should return 401"
        );

        // 3. Rate limit exhausted: burst was 2, both tokens consumed → 429
        //    (rate limit middleware is outermost, blocks before auth)
        let req = Request::builder()
            .uri("/v1/chat/completions")
            .header("authorization", "Bearer sk-test-123")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "rate limit exhausted should return 429 before auth"
        );

        // 4. Recovery: sleep to replenish tokens, then request should pass
        //    both rate limit and auth, reaching the handler
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let req = Request::builder()
            .uri("/v1/chat/completions")
            .header("authorization", "Bearer sk-test-123")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert!(
            resp.status() != StatusCode::TOO_MANY_REQUESTS
                && resp.status() != StatusCode::UNAUTHORIZED,
            "after recovery, request should pass both rate limit and auth (got {})",
            resp.status()
        );
    }

    // ── 5. Config roundtrip test ─────────────────────────────────

    #[test]
    fn config_roundtrip_all_v0_2_sections() {
        let toml_str = r#"
[rate_limit]
enabled = true
burst = 50
per_second = 20.0

[auth]
enabled = true

[auth.api_keys.sk-test-123]
label = "test-key"
scopes = ["chat", "embeddings"]

[store]
enabled = true
db_path = "/tmp/portail-test.db"
retention_days = 7

[telemetry]
enabled = true
endpoint = "http://otel-collector:4317"
service_name = "portail-test"
sampling_ratio = 0.5
"#;

        let cfg: config::Config = toml::from_str(toml_str).expect("parse v0.2 TOML config");

        // rate_limit
        assert!(cfg.rate_limit.enabled);
        assert_eq!(cfg.rate_limit.burst, 50);
        assert_eq!(cfg.rate_limit.per_second, 20.0);

        // auth
        assert!(cfg.auth.enabled);
        assert_eq!(cfg.auth.api_keys.len(), 1);
        let entry = cfg.auth.api_keys.get("sk-test-123").unwrap();
        assert_eq!(entry.label, "test-key");
        assert_eq!(entry.scopes, vec!["chat", "embeddings"]);

        // store
        assert!(cfg.store.enabled);
        assert_eq!(cfg.store.db_path, "/tmp/portail-test.db");
        assert_eq!(cfg.store.retention_days, 7);

        // telemetry
        assert!(cfg.telemetry.enabled);
        assert_eq!(cfg.telemetry.endpoint, "http://otel-collector:4317");
        assert_eq!(cfg.telemetry.service_name, "portail-test");
        assert_eq!(cfg.telemetry.sampling_ratio, 0.5);
    }

    #[test]
    fn config_roundtrip_disabled_defaults() {
        let toml_str = r#"
[rate_limit]
enabled = false
burst = 5

[auth]

[store]

[telemetry]
"#;

        let cfg: config::Config = toml::from_str(toml_str).expect("parse minimal v0.2 TOML");

        // Rate limit defaults to enabled (v1.0); explicit disable tested here
        assert!(!cfg.rate_limit.enabled);
        assert!(!cfg.auth.enabled);
        assert!(!cfg.store.enabled);
        assert!(!cfg.telemetry.enabled);

        // But explicit overrides should still work
        assert_eq!(cfg.rate_limit.burst, 5);

        // Default values for non-overridden fields
        assert_eq!(cfg.rate_limit.per_second, 10.0);
        assert_eq!(cfg.store.retention_days, 30);
        assert_eq!(cfg.telemetry.sampling_ratio, 0.1);
        assert_eq!(cfg.telemetry.service_name, "portail");
    }

    #[tokio::test]
    async fn sentinel_hello_test() {
        let state = Arc::new(base_app_state());
        let el = state.event_log.clone();

        // Spawn sentinel
        tokio::spawn(async move {
            portail::sentinel::run_sentinel(el, None).await;
        });

        // Wait a bit for sentinel to start and publish its started event
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Publish a HELLO event
        state.event_log.publish(events::AgentEvent {
            agent_id: "test".into(),
            event_type: "HELLO".into(),
            severity: "info".into(),
            timestamp: 0,
            metadata: portail::types::BoundedMeta::default(),
        });

        // Wait for sentinel_hello_success event
        let mut success = false;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let events = state.event_log.recent(50);
            if events
                .iter()
                .any(|e| e.event_type == "sentinel_hello_success")
            {
                success = true;
                break;
            }
        }
        assert!(success, "Sentinel did not publish sentinel_hello_success");
    }
}
