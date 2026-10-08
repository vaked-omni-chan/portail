//! Prompt Versioning — git-like prompt history with commits, branches, tags.
//!
//! Stores prompts in SQLite with full history tracking:
//! - `commit`: snapshot a prompt version with message
//! - `log`: view history with diff
//! - `branch`: create named branches for experimentation
//! - `tag`: mark important versions
//! - `checkout`: restore a previous version
//!
//! Prompts are content-addressed (SHA-256) for dedup.

use chrono::Utc;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptCommit {
    pub hash: String,
    pub parent_hash: Option<String>,
    pub branch: String,
    pub message: String,
    pub content: String,
    pub created_at: String,
    pub author: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptTag {
    pub name: String,
    pub commit_hash: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptBranch {
    pub name: String,
    pub head_hash: String,
    pub updated_at: String,
}

pub struct PromptStore {
    conn: Arc<RwLock<Connection>>,
}

impl PromptStore {
    // rusqlite's `Connection` is `!Sync`; it is shared behind an `RwLock` for
    // interior mutability inside this store and never sent across threads.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new(db_path: &Path) -> anyhow::Result<Self> {
        // Open the database and ensure the schema exists. Runs unconditionally
        // so an existing-but-empty file still gets its tables (see the same
        // pattern in cost_attribution / semantic_cache).
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS prompt_commits (
                hash TEXT PRIMARY KEY,
                parent_hash TEXT,
                branch TEXT NOT NULL,
                message TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL,
                author TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS prompt_tags (
                name TEXT PRIMARY KEY,
                commit_hash TEXT NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY (commit_hash) REFERENCES prompt_commits(hash)
            );
            CREATE TABLE IF NOT EXISTS prompt_branches (
                name TEXT PRIMARY KEY,
                head_hash TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                FOREIGN KEY (head_hash) REFERENCES prompt_commits(hash)
            );
            CREATE INDEX IF NOT EXISTS idx_commits_branch ON prompt_commits(branch);
            CREATE INDEX IF NOT EXISTS idx_commits_created ON prompt_commits(created_at);",
        )?;

        Ok(Self {
            conn: Arc::new(RwLock::new(conn)),
        })
    }

    /// Commit a prompt version. Returns the commit hash.
    pub fn commit(
        &self,
        branch: &str,
        content: &str,
        message: &str,
        author: &str,
    ) -> anyhow::Result<String> {
        let hash = compute_hash(content);
        let now = Utc::now().to_rfc3339();

        let conn = self.conn.read().unwrap();

        // Get parent (current HEAD of branch)
        let parent_hash: Option<String> = conn
            .query_row(
                "SELECT head_hash FROM prompt_branches WHERE name = ?1",
                params![branch],
                |row| row.get(0),
            )
            .ok();

        conn.execute(
            "INSERT INTO prompt_commits (hash, parent_hash, branch, message, content, created_at, author)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![hash, parent_hash, branch, message, content, now, author],
        )?;

        // Update or create branch
        conn.execute(
            "INSERT INTO prompt_branches (name, head_hash, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET head_hash = ?2, updated_at = ?3",
            params![branch, hash, now],
        )?;

        Ok(hash)
    }

    /// Get commit by hash
    pub fn get_commit(&self, hash: &str) -> Option<PromptCommit> {
        let conn = self.conn.read().unwrap();
        conn.query_row(
            "SELECT hash, parent_hash, branch, message, content, created_at, author
             FROM prompt_commits WHERE hash = ?1",
            params![hash],
            |row| {
                Ok(PromptCommit {
                    hash: row.get(0)?,
                    parent_hash: row.get(1)?,
                    branch: row.get(2)?,
                    message: row.get(3)?,
                    content: row.get(4)?,
                    created_at: row.get(5)?,
                    author: row.get(6)?,
                })
            },
        )
        .ok()
    }

    /// Get current HEAD of branch
    pub fn head(&self, branch: &str) -> Option<PromptCommit> {
        let conn = self.conn.read().unwrap();
        let hash: String = conn
            .query_row(
                "SELECT head_hash FROM prompt_branches WHERE name = ?1",
                params![branch],
                |row| row.get(0),
            )
            .ok()?;
        drop(conn);
        self.get_commit(&hash)
    }

    /// Log commits for a branch (newest first)
    pub fn log(&self, branch: &str, limit: usize) -> Vec<PromptCommit> {
        let conn = self.conn.read().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT hash, parent_hash, branch, message, content, created_at, author
                 FROM prompt_commits WHERE branch = ?1
                 ORDER BY created_at DESC LIMIT ?2",
            )
            .unwrap();

        stmt.query_map(params![branch, limit as i64], |row| {
            Ok(PromptCommit {
                hash: row.get(0)?,
                parent_hash: row.get(1)?,
                branch: row.get(2)?,
                message: row.get(3)?,
                content: row.get(4)?,
                created_at: row.get(5)?,
                author: row.get(6)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    /// Create a tag pointing to a commit
    pub fn tag(&self, name: &str, commit_hash: &str) -> anyhow::Result<()> {
        let conn = self.conn.read().unwrap();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO prompt_tags (name, commit_hash, created_at) VALUES (?1, ?2, ?3)",
            params![name, commit_hash, now],
        )?;
        Ok(())
    }

    /// Get tag
    pub fn get_tag(&self, name: &str) -> Option<PromptTag> {
        let conn = self.conn.read().unwrap();
        conn.query_row(
            "SELECT name, commit_hash, created_at FROM prompt_tags WHERE name = ?1",
            params![name],
            |row| {
                Ok(PromptTag {
                    name: row.get(0)?,
                    commit_hash: row.get(1)?,
                    created_at: row.get(2)?,
                })
            },
        )
        .ok()
    }

    /// List all branches
    pub fn branches(&self) -> Vec<PromptBranch> {
        let conn = self.conn.read().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT name, head_hash, updated_at FROM prompt_branches ORDER BY updated_at DESC",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok(PromptBranch {
                name: row.get(0)?,
                head_hash: row.get(1)?,
                updated_at: row.get(2)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    /// Create a new branch from another branch's HEAD
    pub fn create_branch(&self, name: &str, from_branch: &str) -> anyhow::Result<()> {
        let head = self
            .head(from_branch)
            .ok_or_else(|| anyhow::anyhow!("source branch '{}' not found", from_branch))?;
        let conn = self.conn.read().unwrap();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO prompt_branches (name, head_hash, updated_at) VALUES (?1, ?2, ?3)",
            params![name, head.hash, now],
        )?;
        Ok(())
    }
}

fn compute_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())[..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn test_store() -> PromptStore {
        let tmp = NamedTempFile::new().unwrap();
        // Persist the file: a plain NamedTempFile deletes itself on drop, which
        // would remove the database out from under the connection.
        let (_file, path) = tmp.keep().unwrap();
        PromptStore::new(&path).unwrap()
    }

    #[test]
    fn commit_and_get() {
        let store = test_store();
        let hash = store
            .commit("main", "Hello world", "initial", "alice")
            .unwrap();
        let commit = store.get_commit(&hash).unwrap();
        assert_eq!(commit.content, "Hello world");
        assert_eq!(commit.message, "initial");
        assert_eq!(commit.branch, "main");
    }

    #[test]
    fn head_tracks_latest() {
        let store = test_store();
        store.commit("main", "v1", "first", "alice").unwrap();
        store.commit("main", "v2", "second", "alice").unwrap();
        let head = store.head("main").unwrap();
        assert_eq!(head.content, "v2");
    }

    #[test]
    fn log_returns_newest_first() {
        let store = test_store();
        store.commit("main", "v1", "first", "alice").unwrap();
        store.commit("main", "v2", "second", "alice").unwrap();
        store.commit("main", "v3", "third", "alice").unwrap();
        let log = store.log("main", 10);
        assert_eq!(log.len(), 3);
        assert_eq!(log[0].content, "v3");
        assert_eq!(log[2].content, "v1");
    }

    #[test]
    fn tag_and_retrieve() {
        let store = test_store();
        let hash = store.commit("main", "v1.0", "release", "alice").unwrap();
        store.tag("v1.0.0", &hash).unwrap();
        let tag = store.get_tag("v1.0.0").unwrap();
        assert_eq!(tag.commit_hash, hash);
    }

    #[test]
    fn create_branch_from_head() {
        let store = test_store();
        store.commit("main", "v1", "first", "alice").unwrap();
        store.create_branch("feature", "main").unwrap();
        let branches = store.branches();
        assert_eq!(branches.len(), 2);
    }
}
