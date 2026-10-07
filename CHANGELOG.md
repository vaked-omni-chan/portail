# Changelog

All notable changes to this project will be documented in this file.

## [3.0.0] — GALACTIC EXPANSION — 2026-10-08

### Changed
- The repository now lives under the **vaked-omni-chan** organisation — the
  constellation's new home. Same binary, wider orbit: install URLs, the GHCR
  image, and the crate repository all point at `vaked-omni-chan/portail`.

### Notes
- No source changes in this release; it is the move and the version it carries.

## [1.4.0] - 2026-06-26

### Added
- Chore-bot CI agent — mechanical Rust cleanup automation
- NATS event bridge — distributed publish/subscribe (`portail.events.*`)
- `/dashboard` HTTP endpoint — config health, rate/auth/cdn counters
- TUI config health indicator (green/red dot) + `c` clear alerts
- `portail init` interactive config wizard
- Config versioning with persisted history (`portail config rollback`)
- Self-healing config watcher (auto-reload on file change)
- BoundedMeta type hardening — replaced unbounded FxHashMap on hot paths
- A2A WebSocket route wired (`/a2a/ws`)
- A2A JSON serialization + HTTP integration tests (6 new)
- AgentGateway interop (A2A spec compliance)
- Rate limit enabled by default (burst=30, tokens=10/s)
- Auth failure counter, rate limit denied counter
- 156 tests (+49 since v0.1.0), 0 compiler warnings

## [0.2.0–0.6.0] - 2026-06-26

### Added
- Rate limiting (token bucket, 429 + Retry-After)
- Authentication middleware (API key + JWT, bypass list)
- Persistent event store (SQLite, retention, JSON export)
- OTLP trace export (gRPC to Jaeger/Tempo)
- Complexity bot (advisory CI agent, daily-once)
- Drift detect (capture/replay smoke tests)
- Spec verify (route table vs golden OpenAPI spec)
- Fuzz route (crash detector, panic-free property)
- WebSocket A2A + GraphQL API
- Godfather system info monitoring + webhook alerts
- Sessions analytics, supervisor, file-cache
- 12 ghost routers wired, 28 orphaned endpoints recovered
- 138 tests

## [0.1.0] - 2026-06-26

### Added
- AI Gateway (OpenAI/Anthropic/LiteLLM proxy)
- MCP Gateway (Unix socket sidecar)
- CDN Cache (Moka + blake3 filesystem)
- A2A Protocol (Agent-to-Agent)
- A2C Interface (Agent-to-Consumer)
- Hook Injection (per-message/per-event)
- Event Log (ring buffer + SSE)
- Sentinel (health monitoring)
- NullClaw (network-native agent)
- Godfather (service monitor)
- Discovery (self-service network discovery)
- CI Status Webhook (live badge)
- TUI Dashboard (network visualization)
- DNS (DoH + network isolation)
- TinyURL (auto URL shortening)
- Tracer (request/response E2E)
- Redis Cache (app-level)
- eBPF Observability, io_uring, DPDK, Hyper engines
- HSTS + security headers
- Cosign-signed releases
- Docker multi-arch builds
- NixOS module with hardening
