//! The daemon's per-agent token-usage monitor.
//!
//! For each watched agent instance hierarchy (AIH) it loops
//! `agents logs token-usage subscribe`; whenever the agent's `total_tokens`
//! grows past its `token_repeat` since the last injection, it re-reads the
//! loaded skill fresh from the laboratory and re-enqueues it as a
//! `<arcanum>…</arcanum>` message. It keeps subscribing even with no skill
//! loaded (advancing the baseline quietly) and stops when the instance goes
//! inactive.
//!
//! `token_repeat` is not persisted — it's passed in per trigger (begin's NOTIFY
//! payload or `load_skill`'s header) and captured by the loop. The skill content
//! is not persisted either — only its reference (lab id + path) is, and it's
//! re-read on each injection so edits are picked up.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use futures::StreamExt;
use objectiveai_sdk::cli::command::agents::enqueue;
use objectiveai_sdk::cli::command::agents::logs::token_usage::{get as tu_get, subscribe as tu_subscribe};
use objectiveai_sdk::cli::command::agents::message::RequestMessage;
use objectiveai_sdk::cli::command::agents::selector::AgentSelector;
use objectiveai_sdk::cli::command::plugin::PluginExecutor;
use tokio::task::JoinHandle;

use crate::db::Db;
use crate::mcp::common;

/// Idempotency key for the re-injected skill message: a later injection replaces
/// any still-queued earlier one for the same agent.
const ENQUEUE_KEY: &str = "arcanum-skill";

/// Wrap a skill's content as the `<arcanum>…</arcanum>` message the agent sees.
fn format_injection(skill_content: &str) -> String {
    format!("<arcanum>\n{skill_content}\n</arcanum>")
}

/// What one monitor tick does with a fresh `total_tokens` reading.
#[derive(Debug, PartialEq, Eq)]
enum Tick {
    /// Re-read the loaded skill and enqueue it; advance the baseline.
    Inject,
    /// No skill loaded — advance the baseline quietly (no injection).
    AdvanceBaseline,
    /// Skill loaded but below threshold — keep accumulating.
    Hold,
}

/// Decide what a tick does given the injection baseline, the new total, the
/// repeat threshold, and whether a skill is loaded. Injection is a REFRESHER,
/// not an initializer: it fires only once usage grows strictly past
/// `token_repeat` beyond the baseline. The first observation just records the
/// baseline — a freshly loaded skill is delivered by `load_skill`'s tool
/// response, not by the monitor.
fn decide(base: Option<i64>, new: i64, token_repeat: i64, has_skill: bool) -> Tick {
    match base {
        // No baseline recorded yet → record it without injecting.
        None => Tick::AdvanceBaseline,
        // No skill loaded → keep the baseline current, quietly.
        Some(_) if !has_skill => Tick::AdvanceBaseline,
        // Skill loaded and usage grew past the threshold → refresh it.
        Some(b) if new - b > token_repeat => Tick::Inject,
        // Skill loaded but below threshold → keep accumulating.
        Some(_) => Tick::Hold,
    }
}

/// Runs the per-AIH token-usage monitor loops in the daemon. At most one loop
/// per AIH: spawning a monitor for an AIH kills whichever monitor was already
/// running for it. Two triggers reach the same [`spawn`]: MCP begin (via
/// [`on_begin`], which only spawns if a skill is loaded) and skill load (via
/// `load_skill`, which records the baseline then calls [`spawn`] directly). The
/// spawner itself is identical for both.
///
/// [`on_begin`]: MonitorService::on_begin
/// [`spawn`]: MonitorService::spawn
pub struct MonitorService {
    db: Db,
    executor: PluginExecutor,
    /// Live monitors keyed by AIH, each tagged with the generation that spawned
    /// it so a loop only deregisters itself if a newer spawn hasn't replaced it.
    running: DashMap<String, (u64, JoinHandle<()>)>,
    next_generation: AtomicU64,
}

impl MonitorService {
    pub fn new(db: Db, executor: PluginExecutor) -> Arc<Self> {
        Arc::new(Self {
            db,
            executor,
            running: DashMap::new(),
            next_generation: AtomicU64::new(0),
        })
    }

    /// The MCP-begin trigger: start a monitor for `aih`, but ONLY if a skill is
    /// already loaded. A loaded skill always has a recorded token baseline (set
    /// atomically at load time), so a loaded skill with no baseline is an
    /// impossible state and panics.
    pub async fn on_begin(self: &Arc<Self>, aih: &str, token_repeat: i64) {
        match self.db.skill_ref(aih).await {
            Ok(Some(_)) => {}
            // No skill loaded → nothing to monitor at begin.
            Ok(None) => return,
            // Transient DB error → a later begin NOTIFY retries.
            Err(_) => return,
        }
        match self.db.last_total_tokens(aih).await {
            Ok(Some(_)) => {}
            Ok(None) => panic!(
                "arcanum monitor invariant violated: skill loaded for {aih} but no token baseline"
            ),
            Err(_) => return,
        }
        self.spawn(aih.to_string(), token_repeat);
    }

    /// Spawn a fresh monitor loop for `aih`, killing any monitor already running
    /// for it. This is the identical spawner both triggers use — it touches no
    /// baseline: recording the token count is the caller's job (the skill-load
    /// path does it before calling here; the begin path relies on the baseline
    /// recorded at load). The loop deregisters itself on exit, but only if a
    /// newer spawn hasn't already replaced (and aborted) it.
    pub fn spawn(self: &Arc<Self>, aih: String, token_repeat: i64) {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let this = self.clone();
        let loop_aih = aih.clone();
        let handle = tokio::spawn(async move {
            this.run_loop(&loop_aih, token_repeat).await;
            if let Entry::Occupied(slot) = this.running.entry(loop_aih) {
                if slot.get().0 == generation {
                    slot.remove();
                }
            }
        });
        if let Some((_, previous)) = self.running.insert(aih, (generation, handle)) {
            previous.abort();
        }
    }

    /// Kill the monitor running for `aih`, aborting its loop. Returns whether a
    /// monitor was actually running (the `unload_skill` path).
    pub fn stop(&self, aih: &str) -> bool {
        match self.running.remove(aih) {
            Some((_, (_, handle))) => {
                handle.abort();
                true
            }
            None => false,
        }
    }

    async fn run_loop(&self, aih: &str, token_repeat: i64) {
        // `seen` is the subscribe cursor (advances every tick so the loop never
        // busy-spins). The injection baseline lives in the DB (`last_total_tokens`)
        // and is re-read each tick, so a concurrent `load_skill` reset is picked up.
        let mut seen = self.db.last_total_tokens(aih).await.ok().flatten();
        loop {
            let new = match self.subscribe(aih, seen).await {
                Some(Some(total)) => total,
                Some(None) => {
                    // agents_inactive — the instance is done.
                    let _ = self.db.delete(aih).await;
                    break;
                }
                None => break, // executor error / stream ended
            };
            seen = Some(new);
            let base = self.db.last_total_tokens(aih).await.ok().flatten();
            let skill = self.db.skill_ref(aih).await.ok().flatten();
            match decide(base, new, token_repeat, skill.is_some()) {
                // A skill is loaded and usage grew past the threshold → re-read
                // the skill fresh and inject. On a read failure, leave the
                // baseline put so the next tick retries (but `seen` advanced, so
                // no spin).
                Tick::Inject => {
                    let skill = skill.expect("decide() returns Inject only with a skill");
                    if let Some(content) = common::read_skill_md(
                        &self.executor,
                        &skill.response_id,
                        &skill.laboratory_id,
                        &skill.skill_path,
                    )
                    .await
                    {
                        tokio::join!(
                            self.enqueue(aih, &content),
                            async { let _ = self.db.set_last_total_tokens(aih, new).await; },
                        );
                    }
                }
                // No skill loaded → advance the baseline quietly (no injection).
                Tick::AdvanceBaseline => {
                    let _ = self.db.set_last_total_tokens(aih, new).await;
                }
                // Skill loaded but below threshold → keep accumulating.
                Tick::Hold => {}
            }
        }
    }

    /// Read the agent's current stored `total_tokens` (no waiting).
    pub async fn token_usage_get(&self, aih: &str) -> Option<i64> {
        tu_get::execute(
            &self.executor,
            tu_get::Request {
                path_type: tu_get::Path::AgentsLogsTokenUsageGet,
                agent_instance_hierarchy: aih.to_string(),
                base: Default::default(),
            },
            None,
        )
        .await
        .ok()?
        .total_tokens
    }

    /// One-shot subscribe. `Some(Some(total))` = a new snapshot,
    /// `Some(None)` = agents_inactive, `None` = executor error / no item.
    async fn subscribe(&self, aih: &str, previous: Option<i64>) -> Option<Option<i64>> {
        let mut stream = tu_subscribe::execute(
            &self.executor,
            tu_subscribe::Request {
                path_type: tu_subscribe::Path::AgentsLogsTokenUsageSubscribe,
                agent_instance_hierarchy: aih.to_string(),
                previous,
                base: Default::default(),
            },
            None,
        )
        .await
        .ok()?;
        let item = stream.next().await?.ok()?;
        Some(match item {
            tu_subscribe::ResponseItem::Item(tu) => Some(tu.total_tokens),
            tu_subscribe::ResponseItem::AgentsInactive(_) => None,
        })
    }

    /// Enqueue `skill_content` as a `<arcanum>…</arcanum>` message to `aih`.
    pub async fn enqueue(&self, aih: &str, skill_content: &str) {
        let (parent, instance) = match aih.rsplit_once('/') {
            Some((p, i)) => (Some(p.to_string()), i.to_string()),
            None => (None, aih.to_string()),
        };
        let message = format_injection(skill_content);
        let _ = enqueue::execute(
            &self.executor,
            enqueue::Request {
                path_type: enqueue::Path::AgentsEnqueue,
                agent: AgentSelector::Instance {
                    parent_agent_instance_hierarchy: parent,
                    agent_instance: instance,
                },
                message: RequestMessage::Simple(message),
                key: Some(ENQUEUE_KEY.to_string()),
                base: Default::default(),
            },
            None,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::{ENQUEUE_KEY, Tick, decide, format_injection};

    #[test]
    fn injection_wraps_content_in_arcanum_tags() {
        assert_eq!(
            format_injection("Greeting skill: say hello."),
            "<arcanum>\nGreeting skill: say hello.\n</arcanum>"
        );
    }

    #[test]
    fn enqueue_key_is_stable() {
        // A stable key is what makes a newer injection REPLACE any still-queued
        // earlier one (objectiveai's `agents enqueue` keying) — so reloading a
        // skill mid-session leaves only the latest injection queued.
        assert_eq!(ENQUEUE_KEY, "arcanum-skill");
    }

    #[test]
    fn no_skill_advances_baseline() {
        // Regardless of the counts, with no skill loaded we just advance.
        assert_eq!(decide(None, 999, 10, false), Tick::AdvanceBaseline);
        assert_eq!(decide(Some(100), 100_000, 10, false), Tick::AdvanceBaseline);
    }

    #[test]
    fn first_observation_records_baseline_without_injecting() {
        // Injection is a refresher, not an initializer: the first tick after a
        // load records the baseline (the skill was already delivered in
        // load_skill's tool response) — it does not inject.
        assert_eq!(decide(None, 0, 1000, true), Tick::AdvanceBaseline);
        assert_eq!(decide(None, 50_000, 1000, true), Tick::AdvanceBaseline);
    }

    #[test]
    fn threshold_is_strictly_greater() {
        // base=100, repeat=50.
        assert_eq!(decide(Some(100), 140, 50, true), Tick::Hold); // +40  < 50
        assert_eq!(decide(Some(100), 150, 50, true), Tick::Hold); // +50 == 50 (not >)
        assert_eq!(decide(Some(100), 151, 50, true), Tick::Inject); // +51  > 50
        assert_eq!(decide(Some(100), 200, 50, true), Tick::Inject); // +100 > 50
    }
}
