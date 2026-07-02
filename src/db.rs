//! The arcanum Postgres store: per-agent token-monitor state, plus the
//! `arcanum_monitor` NOTIFY channel the daemon LISTENs on.
//!
//! Connected lazily from [`Context`](crate::context::Context) (only the daemon,
//! `mcp arcanum begin`, and `load_skill` touch it). Runtime queries only — no
//! compile-time macros, so no `DATABASE_URL` is needed at build time.
//!
//! Persists only the injection baseline, the loaded skill's *reference*
//! (laboratory id + path), and the agent's latest response id. `token_repeat`
//! and the skill content are intentionally not stored.

use sqlx::postgres::{PgListener, PgPool, PgPoolOptions};

/// The embedded schema, applied idempotently on [`Db::connect`].
const SCHEMA: &str = include_str!("schema.sql");

/// The Postgres NOTIFY channel `mcp arcanum begin` pings and the daemon LISTENs.
pub const MONITOR_CHANNEL: &str = "arcanum_monitor";

/// The loaded skill's reference plus the response id used to re-read it.
pub struct SkillRef {
    pub laboratory_id: String,
    pub skill_path: String,
    pub response_id: String,
}

/// Cloneable handle to the Postgres pool. Clone is cheap (the pool is `Arc`).
#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    /// Open the pool and apply the schema. The schema is `CREATE TABLE IF NOT
    /// EXISTS`, so this is idempotent.
    pub async fn connect(url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(url)
            .await?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }

    /// Record (or refresh) the agent's latest response id, creating the row if new.
    pub async fn upsert_response_id(&self, aih: &str, response_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO arcanum_agents (agent_instance_hierarchy, response_id) VALUES ($1, $2) \
             ON CONFLICT (agent_instance_hierarchy) DO UPDATE SET response_id = excluded.response_id",
        )
        .bind(aih)
        .bind(response_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set the loaded skill reference + response id AND the monitor baseline in
    /// one upsert, creating the row if new. Writing them together upholds the
    /// invariant the monitor relies on: a loaded skill always has a baseline.
    pub async fn set_skill(
        &self,
        aih: &str,
        laboratory_id: &str,
        skill_path: &str,
        response_id: &str,
        last_total_tokens: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO arcanum_agents \
                 (agent_instance_hierarchy, laboratory_id, skill_path, response_id, last_total_tokens) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (agent_instance_hierarchy) DO UPDATE SET \
                 laboratory_id = excluded.laboratory_id, \
                 skill_path = excluded.skill_path, \
                 response_id = excluded.response_id, \
                 last_total_tokens = excluded.last_total_tokens",
        )
        .bind(aih)
        .bind(laboratory_id)
        .bind(skill_path)
        .bind(response_id)
        .bind(last_total_tokens)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The loaded skill reference, or `None` if no skill is loaded (or the
    /// response id is missing).
    pub async fn skill_ref(&self, aih: &str) -> Result<Option<SkillRef>, sqlx::Error> {
        let row: Option<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT laboratory_id, skill_path, response_id \
             FROM arcanum_agents WHERE agent_instance_hierarchy = $1",
        )
        .bind(aih)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((Some(laboratory_id), Some(skill_path), Some(response_id))) => Some(SkillRef {
                laboratory_id,
                skill_path,
                response_id,
            }),
            _ => None,
        })
    }

    /// The injection baseline, or `None` if the row is absent OR the column is NULL.
    pub async fn last_total_tokens(&self, aih: &str) -> Result<Option<i64>, sqlx::Error> {
        let row: Option<Option<i64>> = sqlx::query_scalar(
            "SELECT last_total_tokens FROM arcanum_agents WHERE agent_instance_hierarchy = $1",
        )
        .bind(aih)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.flatten())
    }

    /// Advance the injection baseline (the row must already exist).
    pub async fn set_last_total_tokens(&self, aih: &str, value: i64) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE arcanum_agents SET last_total_tokens = $2 WHERE agent_instance_hierarchy = $1")
            .bind(aih)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Drop an agent's row (the agent instance went inactive).
    pub async fn delete(&self, aih: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM arcanum_agents WHERE agent_instance_hierarchy = $1")
            .bind(aih)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// NOTIFY the daemon that `aih` should be (re)evaluated for monitoring, with
    /// the agent's `token_repeat` (carried in the payload — it isn't persisted).
    pub async fn notify_monitor(&self, aih: &str, token_repeat: i64) -> Result<(), sqlx::Error> {
        let payload = serde_json::json!({ "aih": aih, "token_repeat": token_repeat }).to_string();
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(MONITOR_CHANNEL)
            .bind(payload)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A dedicated listener on the [`MONITOR_CHANNEL`].
    pub async fn monitor_listener(&self) -> Result<PgListener, sqlx::Error> {
        let mut listener = PgListener::connect_with(&self.pool).await?;
        listener.listen(MONITOR_CHANNEL).await?;
        Ok(listener)
    }
}
