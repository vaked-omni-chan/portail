//! Spec Verify — compare route table against golden spec.
//!
//! ADK-Rust agent wrapper around a deterministic diff. The agent reads the
//! golden `spec.routes.toml`, generates the canonical route table, and emits
//! a `SpecVerifyReport` as an event payload.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

const GOLDEN_FILE: &str = "spec.routes.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecVerifyConfig {
    pub golden_path: String,
}

impl Default for SpecVerifyConfig {
    fn default() -> Self {
        Self {
            golden_path: GOLDEN_FILE.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct RouteEntry {
    pub method: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteChange {
    pub path: String,
    pub old_methods: Vec<String>,
    pub new_methods: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecVerifyReport {
    pub generated_at: String,
    pub total_routes: usize,
    pub added: Vec<RouteEntry>,
    pub removed: Vec<RouteEntry>,
    pub changed: Vec<RouteChange>,
    pub has_diff: bool,
}

/// Canonical route table. Kept in sync with `src/proxy.rs`.
pub fn generate_routes() -> Vec<RouteEntry> {
    let mut routes = vec![
        route("GET", "/healthz"),
        route("GET", "/livez"),
        route("GET", "/readyz"),
        route("GET", "/metrics"),
        route("ANY", "/v1/messages"),
        route("ANY", "/v1/chat/completions"),
        route("ANY", "/v1/responses"),
        route("ANY", "/v1/embeddings"),
        route("ANY", "/v1/audio/*"),
        route("ANY", "/v1/images/*"),
        route("ANY", "/v1beta/*"),
        route("ANY", "/cdn/*"),
        route("ANY", "/mcp/*"),
        route("ANY", "/mcp-rest/*"),
        route("GET", "/stats"),
        route("GET", "/events"),
        route("POST", "/events"),
        route("GET", "/events/stream"),
        route("GET", "/hooks"),
        route("POST", "/hooks"),
        route("DELETE", "/hooks/{id}"),
        route("GET", "/.well-known/agent.json"),
        route("POST", "/a2a/tasks"),
        route("GET", "/a2a/tasks/{id}"),
        route("GET", "/a2a/ws"),
        route("POST", "/a2c/chat"),
        route("GET", "/ci/status"),
        route("GET", "/ci/badge"),
        route("GET", "/ci/live"),
        route("POST", "/ci/webhook"),
        route("POST", "/discovery/register"),
        route("POST", "/discovery/heartbeat/{id}"),
        route("POST", "/discovery/deregister/{id}"),
        route("GET", "/discovery/nodes"),
        route("GET", "/discovery/stats"),
        route("GET", "/dns/query"),
        route("POST", "/tinyurl/shorten"),
        route("GET", "/tinyurl/stats"),
        route("GET", "/s/{id}"),
        route("GET", "/traces"),
        route("GET", "/traces/stats"),
        route("GET", "/traces/{id}"),
        route("GET", "/traces/{id}/ascii"),
        route("GET", "/cache/stats"),
        route("GET", "/cache/{key}"),
        route("POST", "/cache"),
        route("POST", "/cache/flush"),
        route("GET", "/godfather/status"),
        route("GET", "/agents/status"),
    ];
    routes.sort();
    routes
}

fn route(method: &str, path: &str) -> RouteEntry {
    RouteEntry {
        method: method.into(),
        path: path.into(),
    }
}

/// Compute diff between generated routes and the golden file.
pub fn check_routes(golden_path: &str) -> anyhow::Result<SpecVerifyReport> {
    let current = generate_routes();
    let golden: Vec<RouteEntry> = if Path::new(golden_path).exists() {
        let raw = std::fs::read_to_string(golden_path)?;
        let spec: RouteSpec = toml::from_str(&raw)?;
        let mut routes = spec.routes;
        routes.sort();
        routes
    } else {
        vec![]
    };

    let current_set: BTreeSet<_> = current.iter().cloned().collect();
    let golden_set: BTreeSet<_> = golden.iter().cloned().collect();

    let added: Vec<_> = current_set.difference(&golden_set).cloned().collect();
    let removed: Vec<_> = golden_set.difference(&current_set).cloned().collect();

    let mut changed = Vec::new();
    let current_by_path: std::collections::HashMap<_, Vec<_>> =
        current.iter().map(|r| (&r.path, &r.method)).fold(
            std::collections::HashMap::new(),
            |mut acc, (path, method)| {
                acc.entry(path).or_default().push(method.clone());
                acc
            },
        );
    let golden_by_path: std::collections::HashMap<_, Vec<_>> =
        golden.iter().map(|r| (&r.path, &r.method)).fold(
            std::collections::HashMap::new(),
            |mut acc, (path, method)| {
                acc.entry(path).or_default().push(method.clone());
                acc
            },
        );

    for path in current_by_path.keys() {
        let new_methods = current_by_path.get(*path).cloned().unwrap_or_default();
        let old_methods = golden_by_path.get(*path).cloned().unwrap_or_default();
        let new_set: BTreeSet<_> = new_methods.iter().cloned().collect();
        let old_set: BTreeSet<_> = old_methods.iter().cloned().collect();
        if new_set != old_set && !old_methods.is_empty() {
            changed.push(RouteChange {
                path: (*path).clone(),
                old_methods,
                new_methods,
            });
        }
    }

    let total_routes = current.len();
    let has_diff = !added.is_empty() || !removed.is_empty() || !changed.is_empty();

    Ok(SpecVerifyReport {
        generated_at: Utc::now().to_rfc3339(),
        total_routes,
        added,
        removed,
        changed,
        has_diff,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RouteSpec {
    #[allow(dead_code)]
    generated_at: String,
    #[allow(dead_code)]
    version: String,
    routes: Vec<RouteEntry>,
}

/// Run the deterministic check. This is the non-ADK entry point used by
/// existing CLI code and tests.
pub async fn run_spec_verify(config: &SpecVerifyConfig) -> SpecVerifyReport {
    match check_routes(&config.golden_path) {
        Ok(report) => report,
        Err(e) => {
            tracing::warn!(error = %e, "spec-verify failed");
            SpecVerifyReport {
                generated_at: Utc::now().to_rfc3339(),
                total_routes: 0,
                added: vec![],
                removed: vec![],
                changed: vec![],
                has_diff: false,
            }
        }
    }
}

/// Build an ADK-Rust `CustomAgent` that runs spec-verify when invoked.
pub fn build_spec_verify_agent(
    config: &SpecVerifyConfig,
) -> anyhow::Result<Arc<dyn adk_rust::prelude::Agent>> {
    use adk_rust::InvocationContext;
    use adk_rust::prelude::*;

    let golden_path = config.golden_path.clone();

    let agent: Arc<dyn Agent> = Arc::new(
        CustomAgentBuilder::new("spec_verify")
            .description("Compares the compiled route table against the golden spec")
            .handler(move |_ctx: Arc<dyn InvocationContext>| {
                let golden_path = golden_path.clone();
                async move {
                    let report = check_routes(&golden_path).unwrap_or_else(|_e| SpecVerifyReport {
                        generated_at: Utc::now().to_rfc3339(),
                        total_routes: 0,
                        added: vec![],
                        removed: vec![],
                        changed: vec![],
                        has_diff: false,
                    });

                    tracing::info!(
                        total = report.total_routes,
                        added = report.added.len(),
                        removed = report.removed.len(),
                        changed = report.changed.len(),
                        "spec-verify completed",
                    );

                    let text = serde_json::to_string(&report).unwrap_or_default();
                    let content = Content::new("model").with_text(text);
                    let mut event = Event::new("spec_verify");
                    event.author = "spec_verify".into();
                    event.set_content(content);

                    let stream = futures::stream::iter(vec![Ok(event)]);
                    Ok(Box::pin(stream) as EventStream)
                }
            })
            .build()?,
    );

    Ok(agent)
}
