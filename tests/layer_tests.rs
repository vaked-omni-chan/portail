#[cfg(test)]
mod layer_tests {
    use portail::config::Config;
    use portail::*;
    use std::sync::{Arc, RwLock};

    // ── Test helpers ─────────────────────────────────────────────

    fn global_metrics() -> metrics_exporter_prometheus::PrometheusHandle {
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .install_recorder()
            .expect("install metrics recorder")
    }

    fn test_state() -> AppState {
        AppState {
            config: RwLock::new(Config::default()),
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
            metrics_handle: global_metrics(),
            rate_limiter: None,
            auth_state: None,
            event_store: None,
            session_store: portail::sessions::SessionStore::new(20),
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

    // ── Layer 1: Core types exist and are constructible ──────────

    #[test]
    fn layer1_events_constructible() {
        let log = events::EventLog::new(100);
        log.publish(events::AgentEvent {
            agent_id: "test".into(),
            event_type: "ping".into(),
            severity: "info".into(),
            timestamp: 0,
            metadata: portail::types::BoundedMeta::default(),
        });
        assert_eq!(log.recent(1).len(), 1);
    }

    #[test]
    fn layer1_hooks_constructible() {
        let store = hooks::HookStore::new();
        store.add(hooks::Hook {
            id: "h1".into(),
            match_agent: None,
            match_path: Some("/test".into()),
            match_event_type: None,
            inject: hooks::InjectMode::Prepend,
            content: "test".into(),
            enabled: true,
            priority: 0,
        });
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn layer1_a2a_constructible() {
        let store = a2a::TaskStore::new();
        let task = store.create("t1".into());
        assert_eq!(task.status.state, a2a::TaskState::Submitted);
    }

    #[test]
    fn layer1_dns_constructible() {
        let store = dns::DnsStore::new();
        store.add_record(
            "example.com".into(),
            dns::DnsAnswer {
                name: "example.com".into(),
                record_type: dns::DnsRecordType::A,
                data: "1.2.3.4".into(),
                ttl: 300,
            },
        );
        let answers = store.query("example.com", dns::DnsRecordType::A);
        assert_eq!(answers.len(), 1);
    }

    // ── Layer 2: Modules don't depend on each other incorrectly ─

    #[test]
    fn layer2_events_independent() {
        // Events module should work without hooks
        let log = events::EventLog::new(100);
        log.publish(events::AgentEvent {
            agent_id: "test".into(),
            event_type: "ping".into(),
            severity: "info".into(),
            timestamp: 0,
            metadata: portail::types::BoundedMeta::default(),
        });

        let rx = log.subscribe();
        // Events module is self-contained
        assert_eq!(log.recent(1).len(), 1);
        drop(rx);
    }

    #[test]
    fn layer2_hooks_independent() {
        // Hooks module should work without events
        let store = hooks::HookStore::new();
        store.add(hooks::Hook {
            id: "h1".into(),
            match_agent: None,
            match_path: Some("/test".into()),
            match_event_type: None,
            inject: hooks::InjectMode::Prepend,
            content: "test".into(),
            enabled: true,
            priority: 0,
        });

        let matched = store.match_message("/test");
        assert_eq!(matched.len(), 1);
    }

    #[test]
    fn layer2_dns_independent() {
        // DNS module should work without other modules
        let store = dns::DnsStore::new();
        store.add_hook(dns::DnsHook {
            id: "h1".into(),
            name: "block".into(),
            pattern: "ads.example.com".into(),
            action: dns::DnsHookAction::Block,
            enabled: true,
        });

        let query = dns::DnsQuery {
            name: "ads.example.com".into(),
            record_type: dns::DnsRecordType::A,
            source: "127.0.0.1".parse().unwrap(),
        };

        let action = store.apply_hooks(&query);
        assert!(matches!(action, Some(dns::DnsHookAction::Block)));
    }

    // ── Layer 3: AppState integrates all modules ─────────────────

    #[test]
    fn layer3_appstate_integrates_all() {
        let state = test_state();

        // All modules should be accessible
        assert!(state.cdn_cache.is_none());
        assert!(state.doh_client.is_none());
        assert!(!state.network_isolation.enabled);

        // Event log works
        state.event_log.publish(events::AgentEvent {
            agent_id: "test".into(),
            event_type: "ping".into(),
            severity: "info".into(),
            timestamp: 0,
            metadata: portail::types::BoundedMeta::default(),
        });
        assert_eq!(state.event_log.recent(1).len(), 1);

        // Hooks work
        state.hooks.add(hooks::Hook {
            id: "h1".into(),
            match_agent: None,
            match_path: Some("/test".into()),
            match_event_type: None,
            inject: hooks::InjectMode::Prepend,
            content: "test".into(),
            enabled: true,
            priority: 0,
        });
        assert_eq!(state.hooks.list().len(), 1);

        // A2A tasks work
        let task = state.a2a_tasks.create("t1".into());
        assert_eq!(task.status.state, a2a::TaskState::Submitted);

        // DNS works
        state.dns_store.add_record(
            "example.com".into(),
            dns::DnsAnswer {
                name: "example.com".into(),
                record_type: dns::DnsRecordType::A,
                data: "1.2.3.4".into(),
                ttl: 300,
            },
        );
        assert_eq!(
            state
                .dns_store
                .query("example.com", dns::DnsRecordType::A)
                .len(),
            1
        );
    }

    // ── Layer 4: Network isolation works ─────────────────────────

    #[test]
    fn layer4_network_isolation_allow() {
        let iso = dns::NetworkIsolation {
            enabled: true,
            allowed_domains: vec!["example.com".into()],
            ..Default::default()
        };

        assert!(iso.is_allowed("api.example.com", None));
        assert!(!iso.is_allowed("evil.com", None));
    }

    #[test]
    fn layer4_network_isolation_block() {
        let iso = dns::NetworkIsolation {
            enabled: true,
            blocked_domains: vec!["ads.example.com".into()],
            ..Default::default()
        };

        assert!(iso.is_allowed("api.example.com", None));
        assert!(!iso.is_allowed("ads.example.com", None));
    }

    #[test]
    fn layer4_network_isolation_disabled() {
        let iso = dns::NetworkIsolation {
            enabled: false,
            blocked_domains: vec!["evil.com".into()],
            ..Default::default()
        };

        // When disabled, everything is allowed
        assert!(iso.is_allowed("evil.com", None));
    }

    // ── Layer 5: Hook injection works across modules ─────────────

    #[test]
    fn layer5_message_hooks() {
        let body = serde_json::json!({
            "messages": [
                { "role": "user", "content": "hello" }
            ]
        });

        let hooks = vec![hooks::Hook {
            id: "h1".into(),
            match_agent: None,
            match_path: None,
            match_event_type: None,
            inject: hooks::InjectMode::Prepend,
            content: "be helpful".into(),
            enabled: true,
            priority: 0,
        }];

        let modified = hooks::apply_message_hooks(&body, &hooks).unwrap();
        let msgs = modified["messages"].as_array().unwrap();

        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["content"], "be helpful");
        assert_eq!(msgs[1]["content"], "hello");
    }

    #[test]
    fn layer5_event_hooks() {
        let store = hooks::HookStore::new();
        store.add(hooks::Hook {
            id: "h1".into(),
            match_agent: Some("test-agent".into()),
            match_path: None,
            match_event_type: Some("started".into()),
            inject: hooks::InjectMode::Prepend,
            content: "injected".into(),
            enabled: true,
            priority: 0,
        });

        let matched = store.match_event("test-agent", "started");
        assert_eq!(matched.len(), 1);
    }

    #[test]
    fn layer5_dns_hooks() {
        let store = dns::DnsStore::new();
        store.add_hook(dns::DnsHook {
            id: "h1".into(),
            name: "redirect".into(),
            pattern: "old.example.com".into(),
            action: dns::DnsHookAction::Redirect("new.example.com".into()),
            enabled: true,
        });

        let query = dns::DnsQuery {
            name: "old.example.com".into(),
            record_type: dns::DnsRecordType::A,
            source: "127.0.0.1".parse().unwrap(),
        };

        let action = store.apply_hooks(&query);
        assert!(matches!(action, Some(dns::DnsHookAction::Redirect(_))));
    }

    // ── Layer 6: CLI types are valid ─────────────────────────────

    #[test]
    fn layer6_cli_types_valid() {
        use portail::cli::*;

        // OutputFormat variants
        let _ = OutputFormat::Text;
        let _ = OutputFormat::Json;

        // InstallMethod variants
        let _ = InstallMethod::Auto;
        let _ = InstallMethod::Cargo;
        let _ = InstallMethod::Nix;
        let _ = InstallMethod::Binary;
    }
}
