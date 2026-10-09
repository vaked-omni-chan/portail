//! Cost Attribution — per-model, per-user, per-request billing.
//!
//! Tracks costs across three dimensions:
//! - **Model**: pricing per token for each AI provider/model
//! - **User**: aggregate costs per API key / session
//! - **Request**: individual request cost with full breakdown
//!
//! Costs are computed from token counts using configurable pricing tables.
//! All monetary values in USD cents (u64) for precision.

use chrono::Utc;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, RwLock};

/// Pricing per 1M tokens (in USD cents)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    pub model: String,
    pub input_per_1m: u64,
    pub output_per_1m: u64,
    pub cache_read_per_1m: u64,
    pub cache_write_per_1m: u64,
}

impl ModelPricing {
    pub fn compute_cost(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
    ) -> u64 {
        let input_cost = (input_tokens * self.input_per_1m) / 1_000_000;
        let output_cost = (output_tokens * self.output_per_1m) / 1_000_000;
        let cache_read_cost = (cache_read_tokens * self.cache_read_per_1m) / 1_000_000;
        let cache_write_cost = (cache_write_tokens * self.cache_write_per_1m) / 1_000_000;
        input_cost + output_cost + cache_read_cost + cache_write_cost
    }
}

/// Individual request cost record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestCost {
    pub id: String,
    pub session_id: String,
    pub user_id: String,
    pub model: String,
    pub provider: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_cents: u64,
    pub latency_ms: u64,
    pub timestamp: String,
}

/// User cost summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCostSummary {
    pub user_id: String,
    pub total_cost_cents: u64,
    pub total_requests: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub period_start: String,
    pub period_end: String,
}

/// Model cost summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCostSummary {
    pub model: String,
    pub total_cost_cents: u64,
    pub total_requests: u64,
    pub total_tokens: u64,
}

pub struct CostStore {
    conn: Arc<RwLock<Connection>>,
    pricing: Arc<RwLock<Vec<ModelPricing>>>,
}

impl CostStore {
    // rusqlite's `Connection` is `!Sync`; it is shared behind an `RwLock` for
    // interior mutability inside this store and never sent across threads.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new(db_path: &Path) -> anyhow::Result<Self> {
        // Open the database and ensure the schema exists. The migration runs
        // unconditionally: an existing but empty file (a fresh `touch`, or a
        // crash before the first write) would otherwise leave the tables
        // missing, and every query would fail with "no such table".
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS request_costs (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                model TEXT NOT NULL,
                provider TEXT NOT NULL,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                cost_cents INTEGER NOT NULL,
                latency_ms INTEGER NOT NULL,
                timestamp TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_costs_session ON request_costs(session_id);
            CREATE INDEX IF NOT EXISTS idx_costs_user ON request_costs(user_id);
            CREATE INDEX IF NOT EXISTS idx_costs_model ON request_costs(model);
            CREATE INDEX IF NOT EXISTS idx_costs_timestamp ON request_costs(timestamp);

            CREATE TABLE IF NOT EXISTS model_pricing (
                model TEXT PRIMARY KEY,
                input_per_1m INTEGER NOT NULL,
                output_per_1m INTEGER NOT NULL,
                cache_read_per_1m INTEGER NOT NULL DEFAULT 0,
                cache_write_per_1m INTEGER NOT NULL DEFAULT 0
            );",
        )?;

        // Load pricing
        let pricing = Self::load_pricing(&conn)?;

        Ok(Self {
            conn: Arc::new(RwLock::new(conn)),
            pricing: Arc::new(RwLock::new(pricing)),
        })
    }

    fn load_pricing(conn: &Connection) -> anyhow::Result<Vec<ModelPricing>> {
        let mut stmt = conn.prepare(
            "SELECT model, input_per_1m, output_per_1m, cache_read_per_1m, cache_write_per_1m
             FROM model_pricing",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ModelPricing {
                model: row.get(0)?,
                input_per_1m: row.get(1)?,
                output_per_1m: row.get(2)?,
                cache_read_per_1m: row.get(3)?,
                cache_write_per_1m: row.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Set pricing for a model
    pub fn set_pricing(&self, pricing: ModelPricing) -> anyhow::Result<()> {
        let conn = self.conn.read().unwrap();
        conn.execute(
            "INSERT INTO model_pricing (model, input_per_1m, output_per_1m, cache_read_per_1m, cache_write_per_1m)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(model) DO UPDATE SET
                input_per_1m = ?2, output_per_1m = ?3, cache_read_per_1m = ?4, cache_write_per_1m = ?5",
            params![
                pricing.model,
                pricing.input_per_1m,
                pricing.output_per_1m,
                pricing.cache_read_per_1m,
                pricing.cache_write_per_1m
            ],
        )?;
        drop(conn);

        // Reload pricing
        let conn = self.conn.read().unwrap();
        let new_pricing = Self::load_pricing(&conn)?;
        *self.pricing.write().unwrap() = new_pricing;

        Ok(())
    }

    /// Record a request cost
    pub fn record_request(
        &self,
        session_id: &str,
        user_id: &str,
        model: &str,
        provider: &str,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        latency_ms: u64,
    ) -> anyhow::Result<RequestCost> {
        let pricing = self.pricing.read().unwrap();
        let model_pricing = pricing.iter().find(|p| p.model == model);

        let cost_cents = match model_pricing {
            Some(p) => p.compute_cost(
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
            ),
            None => 0, // Unknown model, no cost tracked
        };

        let id = uuid::Uuid::new_v4().to_string();
        let timestamp = Utc::now().to_rfc3339();

        let cost = RequestCost {
            id: id.clone(),
            session_id: session_id.to_string(),
            user_id: user_id.to_string(),
            model: model.to_string(),
            provider: provider.to_string(),
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            cost_cents,
            latency_ms,
            timestamp: timestamp.clone(),
        };

        let conn = self.conn.read().unwrap();
        conn.execute(
            "INSERT INTO request_costs (id, session_id, user_id, model, provider, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_cents, latency_ms, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                cost.id, cost.session_id, cost.user_id, cost.model, cost.provider,
                cost.input_tokens, cost.output_tokens, cost.cache_read_tokens,
                cost.cache_write_tokens, cost.cost_cents, cost.latency_ms, cost.timestamp
            ],
        )?;

        Ok(cost)
    }

    /// Get user cost summary for a time period
    pub fn user_summary(
        &self,
        user_id: &str,
        start: &str,
        end: &str,
    ) -> anyhow::Result<UserCostSummary> {
        let conn = self.conn.read().unwrap();
        conn.query_row(
            "SELECT COALESCE(SUM(cost_cents), 0), COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
             FROM request_costs
             WHERE user_id = ?1 AND timestamp >= ?2 AND timestamp < ?3",
            params![user_id, start, end],
            |row| {
                Ok(UserCostSummary {
                    user_id: user_id.to_string(),
                    total_cost_cents: row.get(0)?,
                    total_requests: row.get(1)?,
                    total_input_tokens: row.get(2)?,
                    total_output_tokens: row.get(3)?,
                    period_start: start.to_string(),
                    period_end: end.to_string(),
                })
            },
        )
        .map_err(|e| anyhow::anyhow!("query failed: {}", e))
    }

    /// Get model cost summaries
    pub fn model_summaries(&self, start: &str, end: &str) -> anyhow::Result<Vec<ModelCostSummary>> {
        let conn = self.conn.read().unwrap();
        let mut stmt = conn.prepare(
            "SELECT model, SUM(cost_cents), COUNT(*), SUM(input_tokens + output_tokens)
             FROM request_costs
             WHERE timestamp >= ?1 AND timestamp < ?2
             GROUP BY model
             ORDER BY SUM(cost_cents) DESC",
        )?;
        let rows = stmt.query_map(params![start, end], |row| {
            Ok(ModelCostSummary {
                model: row.get(0)?,
                total_cost_cents: row.get(1)?,
                total_requests: row.get(2)?,
                total_tokens: row.get(3)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Get recent request costs
    pub fn recent_costs(&self, limit: usize) -> anyhow::Result<Vec<RequestCost>> {
        let conn = self.conn.read().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, user_id, model, provider, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_cents, latency_ms, timestamp
             FROM request_costs
             ORDER BY timestamp DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(RequestCost {
                id: row.get(0)?,
                session_id: row.get(1)?,
                user_id: row.get(2)?,
                model: row.get(3)?,
                provider: row.get(4)?,
                input_tokens: row.get(5)?,
                output_tokens: row.get(6)?,
                cache_read_tokens: row.get(7)?,
                cache_write_tokens: row.get(8)?,
                cost_cents: row.get(9)?,
                latency_ms: row.get(10)?,
                timestamp: row.get(11)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn test_store() -> CostStore {
        let tmp = NamedTempFile::new().unwrap();
        // Persist the file: a plain NamedTempFile deletes itself on drop, which
        // would remove the database out from under the connection.
        let (_file, path) = tmp.keep().unwrap();
        CostStore::new(&path).unwrap()
    }

    #[test]
    fn pricing_computation() {
        let pricing = ModelPricing {
            model: "gpt-4".into(),
            input_per_1m: 3000,  // $30/1M tokens
            output_per_1m: 6000, // $60/1M tokens
            cache_read_per_1m: 1500,
            cache_write_per_1m: 3750,
        };
        let cost = pricing.compute_cost(1000, 500, 2000, 0);
        assert_eq!(cost, 3 + 3 + 3); // input + output + cache_read; 9 cents
    }

    #[test]
    fn record_and_query() {
        let store = test_store();
        store
            .set_pricing(ModelPricing {
                model: "gpt-4".into(),
                input_per_1m: 3000,
                output_per_1m: 6000,
                cache_read_per_1m: 1500,
                cache_write_per_1m: 3750,
            })
            .unwrap();

        store
            .record_request("sess1", "user1", "gpt-4", "openai", 1000, 500, 0, 0, 100)
            .unwrap();
        store
            .record_request("sess2", "user1", "gpt-4", "openai", 2000, 1000, 0, 0, 200)
            .unwrap();

        let summary = store
            .user_summary("user1", "2020-01-01", "2030-01-01")
            .unwrap();
        assert_eq!(summary.total_requests, 2);
        assert!(summary.total_cost_cents > 0);
    }

    #[test]
    fn model_summaries() {
        let store = test_store();
        store
            .set_pricing(ModelPricing {
                model: "gpt-4".into(),
                input_per_1m: 3000,
                output_per_1m: 6000,
                cache_read_per_1m: 1500,
                cache_write_per_1m: 3750,
            })
            .unwrap();
        store
            .set_pricing(ModelPricing {
                model: "gpt-3.5".into(),
                input_per_1m: 150,
                output_per_1m: 200,
                cache_read_per_1m: 75,
                cache_write_per_1m: 150,
            })
            .unwrap();

        store
            .record_request("s1", "u1", "gpt-4", "openai", 1000, 500, 0, 0, 100)
            .unwrap();
        store
            .record_request("s2", "u1", "gpt-3.5", "openai", 1000, 500, 0, 0, 100)
            .unwrap();

        let summaries = store.model_summaries("2020-01-01", "2030-01-01").unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].model, "gpt-4"); // Higher cost first
    }
}
