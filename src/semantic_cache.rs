//! Semantic Cache — embedding-based dedup for AI responses.
//!
//! Caches AI responses by semantic similarity, not exact match.
//! Uses embeddings to compute cosine similarity between prompts.
//! If similarity > threshold, return cached response.
//!
//! Benefits:
//! - Reduce API costs for similar queries
//! - Faster response times for repeated patterns
//! - Configurable similarity threshold

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, RwLock};

/// Cached response with embedding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedResponse {
    pub id: String,
    pub prompt: String,
    pub response: String,
    pub model: String,
    pub embedding: Vec<f32>,
    pub similarity_threshold: f32,
    pub hit_count: u64,
    pub created_at: String,
    pub last_accessed: String,
}

/// Cache lookup result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CacheResult {
    Hit {
        response: String,
        similarity: f32,
        cached_prompt: String,
    },
    Miss,
}

pub struct SemanticCache {
    conn: Arc<RwLock<Connection>>,
    default_threshold: f32,
}

impl SemanticCache {
    // rusqlite's `Connection` is `!Sync`; it is shared behind an `RwLock` for
    // interior mutability inside this store and never sent across threads.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new(db_path: &Path, default_threshold: f32) -> anyhow::Result<Self> {
        // Open the database and ensure the schema exists. Runs unconditionally
        // so an existing-but-empty file still gets its tables (see the same
        // pattern in cost_attribution / prompt_versioning).
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS semantic_cache (
                id TEXT PRIMARY KEY,
                prompt TEXT NOT NULL,
                response TEXT NOT NULL,
                model TEXT NOT NULL,
                embedding BLOB NOT NULL,
                similarity_threshold REAL NOT NULL,
                hit_count INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                last_accessed TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_cache_model ON semantic_cache(model);
            CREATE INDEX IF NOT EXISTS idx_cache_created ON semantic_cache(created_at);",
        )?;

        Ok(Self {
            conn: Arc::new(RwLock::new(conn)),
            default_threshold,
        })
    }

    /// Store a response with its embedding
    pub fn store(
        &self,
        prompt: &str,
        response: &str,
        model: &str,
        embedding: &[f32],
    ) -> anyhow::Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let embedding_blob = serde_json::to_vec(embedding)?;

        let conn = self.conn.read().unwrap();
        conn.execute(
            "INSERT INTO semantic_cache (id, prompt, response, model, embedding, similarity_threshold, hit_count, created_at, last_accessed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?7)",
            params![id, prompt, response, model, embedding_blob, self.default_threshold, now],
        )?;

        Ok(id)
    }

    /// Lookup by embedding similarity
    pub fn lookup(
        &self,
        embedding: &[f32],
        model: &str,
        threshold: Option<f32>,
    ) -> anyhow::Result<CacheResult> {
        let threshold = threshold.unwrap_or(self.default_threshold);
        let conn = self.conn.read().unwrap();

        let mut stmt = conn.prepare(
            "SELECT id, prompt, response, embedding, similarity_threshold
             FROM semantic_cache
             WHERE model = ?1",
        )?;

        let rows = stmt.query_map(params![model], |row| {
            let embedding_blob: Vec<u8> = row.get(3)?;
            let cached_embedding: Vec<f32> =
                serde_json::from_slice(&embedding_blob).unwrap_or_default();
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                cached_embedding,
                row.get::<_, f32>(4)?,
            ))
        })?;

        let mut best_match: Option<(String, String, f32)> = None;

        for row in rows {
            let (id, prompt, _response, cached_embedding, _threshold) = row?;
            let similarity = cosine_similarity(embedding, &cached_embedding);

            if similarity >= threshold {
                if let Some(ref current_best) = best_match {
                    if similarity > current_best.2 {
                        best_match = Some((id, prompt, similarity));
                    }
                } else {
                    best_match = Some((id, prompt, similarity));
                }
            }
        }

        if let Some((id, prompt, similarity)) = best_match {
            // Update hit count and last accessed
            let now = chrono::Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE semantic_cache SET hit_count = hit_count + 1, last_accessed = ?1 WHERE id = ?2",
                params![now, id],
            )?;

            // Get response
            let response: String = conn.query_row(
                "SELECT response FROM semantic_cache WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )?;

            Ok(CacheResult::Hit {
                response,
                similarity,
                cached_prompt: prompt,
            })
        } else {
            Ok(CacheResult::Miss)
        }
    }

    /// Evict old entries
    pub fn evict_old(&self, max_age_days: u32) -> anyhow::Result<u64> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(max_age_days as i64);
        let cutoff_str = cutoff.to_rfc3339();

        let conn = self.conn.read().unwrap();
        let count = conn.execute(
            "DELETE FROM semantic_cache WHERE created_at < ?1",
            params![cutoff_str],
        )?;

        Ok(count as u64)
    }

    /// Evict low-hit entries
    pub fn evict_low_hit(&self, min_hits: u64) -> anyhow::Result<u64> {
        let conn = self.conn.read().unwrap();
        let count = conn.execute(
            "DELETE FROM semantic_cache WHERE hit_count < ?1",
            params![min_hits],
        )?;

        Ok(count as u64)
    }

    /// Get cache stats
    pub fn stats(&self) -> anyhow::Result<CacheStats> {
        let conn = self.conn.read().unwrap();
        let total_entries: u64 =
            conn.query_row("SELECT COUNT(*) FROM semantic_cache", [], |row| row.get(0))?;
        let total_hits: u64 = conn.query_row(
            "SELECT COALESCE(SUM(hit_count), 0) FROM semantic_cache",
            [],
            |row| row.get(0),
        )?;

        Ok(CacheStats {
            total_entries,
            total_hits,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStats {
    pub total_entries: u64,
    pub total_hits: u64,
}

/// Cosine similarity between two vectors
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot_product / (norm_a * norm_b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn test_cache() -> SemanticCache {
        let tmp = NamedTempFile::new().unwrap();
        // Persist the file: a plain NamedTempFile deletes itself on drop, which
        // would remove the database out from under the connection.
        let (_file, path) = tmp.keep().unwrap();
        SemanticCache::new(&path, 0.9).unwrap()
    }

    #[test]
    fn cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 0.001);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 0.001);
    }

    #[test]
    fn store_and_lookup() {
        let cache = test_cache();
        let embedding = vec![1.0, 0.5, 0.2];
        cache
            .store("What is AI?", "AI is...", "gpt-4", &embedding)
            .unwrap();

        let result = cache.lookup(&embedding, "gpt-4", None).unwrap();
        match result {
            CacheResult::Hit {
                response,
                similarity,
                ..
            } => {
                assert_eq!(response, "AI is...");
                assert!(similarity > 0.99);
            }
            CacheResult::Miss => panic!("Expected cache hit"),
        }
    }

    #[test]
    fn lookup_miss() {
        let cache = test_cache();
        let embedding1 = vec![1.0, 0.0, 0.0];
        let embedding2 = vec![0.0, 1.0, 0.0];
        cache
            .store("prompt1", "response1", "gpt-4", &embedding1)
            .unwrap();

        let result = cache.lookup(&embedding2, "gpt-4", None).unwrap();
        match result {
            CacheResult::Hit { .. } => panic!("Expected cache miss"),
            CacheResult::Miss => {}
        }
    }

    #[test]
    fn evict_old() {
        let cache = test_cache();
        let embedding = vec![1.0, 0.5];
        cache
            .store("prompt", "response", "gpt-4", &embedding)
            .unwrap();

        // Should not evict (just created)
        let evicted = cache.evict_old(1).unwrap();
        assert_eq!(evicted, 0);
    }
}
