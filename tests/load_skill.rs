//! E2E: `load_skill` reads a laboratory's SKILL.md and hands it back to the
//! agent as the tool response. State is asserted only via real objectiveai
//! commands (`agents logs` / `agents queue`), never `db query`.
//!
//! Requires podman + network + the staged `.objectiveai` host (real laboratory
//! containers). Intended for Linux/CI.
//!
//! Scope note: loading a skill delivers it in the tool response and does NOT
//! enqueue anything — enqueue is the daemon monitor's growth-driven REFRESHER,
//! which fires only once `total_tokens` grows past `token_repeat`. The mock
//! upstream always reports `total_tokens = 0`, so the refresh path can't be
//! exercised e2e; it's covered by `monitor.rs` unit tests (`decide`, the
//! `<arcanum>` wrapping, and the stable idempotency key). These tests therefore
//! assert what IS deterministic: the SKILL.md the tool returns, and that
//! loading injects nothing.

mod common;

use common::{Host, Mount, arcanum_agent_with_calls, tool_call};
use serde_json::json;

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn skills_dir(sub: &str) -> String {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test-skills")
        .join(sub)
        .to_string_lossy()
        .into_owned()
}

/// Create a lab bind-mounting `test-skills/lab-one` (greeting + farewell) at
/// `/skills`.
async fn create_lab_one(host: &Host, lab: &str) {
    host.create_lab(
        lab,
        vec![Mount {
            host: skills_dir("lab-one"),
            container: "/skills".to_string(),
        }],
        Vec::new(),
        "/",
    )
    .await;
}

/// A `load_skill` call returns the laboratory's SKILL.md content to the caller
/// and enqueues nothing (loading delivers via the tool response; the monitor is
/// the only injector, and only on growth).
#[tokio::test(flavor = "multi_thread")]
async fn load_skill_returns_content() {
    let host = Host::new("load_skill_returns_content");
    let n = nanos();
    let lab = format!("load-{n}");
    let tag = format!("load-tag-{n}");
    create_lab_one(&host, &lab).await;

    let agent = arcanum_agent_with_calls(vec![tool_call(
        "arcanum_load_skill",
        json!({ "laboratory_id": lab.clone(), "path": "/skills/greeting" }),
    )]);
    host.apply_tag(&tag, agent).await;
    host.attach_lab(&tag, &lab).await;

    let (aih, _) = host.spawn_tag(&tag).await;
    host.wait(&aih).await;

    // load_skill returns the SKILL.md BODY (via `agents logs`) with the
    // frontmatter stripped.
    let tool = host.tool_result_texts(&aih).await.join("\n");
    assert!(
        tool.contains("Greeting skill"),
        "load_skill should return the SKILL.md body; got: {tool}"
    );
    assert!(
        !tool.contains("---") && !tool.contains("when_to_use") && !tool.contains("hello-skill"),
        "the frontmatter must be stripped from the loaded skill; got: {tool}"
    );

    // Loading enqueues nothing — injection is the monitor's job.
    let msgs = host.pending_texts(&aih).await.join("\n---\n");
    assert!(
        !msgs.contains("<arcanum>"),
        "loading a skill must not enqueue an injection; got: {msgs}"
    );
}

/// Re-loading a NEW skill mid-session returns the new skill's content too — the
/// agent can switch skills by calling `load_skill` again.
#[tokio::test(flavor = "multi_thread")]
async fn reload_mid_session_returns_new_content() {
    let host = Host::new("reload_mid_session_returns_new_content");
    let n = nanos();
    let lab = format!("reload-{n}");
    let tag = format!("reload-tag-{n}");
    create_lab_one(&host, &lab).await;

    let agent = arcanum_agent_with_calls(vec![
        tool_call("arcanum_load_skill", json!({ "laboratory_id": lab.clone(), "path": "/skills/greeting" })),
        tool_call("arcanum_load_skill", json!({ "laboratory_id": lab.clone(), "path": "/skills/farewell" })),
    ]);
    host.apply_tag(&tag, agent).await;
    host.attach_lab(&tag, &lab).await;

    let (aih, _) = host.spawn_tag(&tag).await;
    host.wait(&aih).await;

    // Both loads returned their bodies (frontmatter stripped) to the agent.
    let tool = host.tool_result_texts(&aih).await.join("\n");
    assert!(tool.contains("Greeting skill"), "expected greeting body; got: {tool}");
    assert!(tool.contains("Farewell skill"), "expected farewell body; got: {tool}");
    assert!(
        !tool.contains("---") && !tool.contains("description:"),
        "the frontmatter must be stripped from loaded skills; got: {tool}"
    );

    // Still nothing enqueued (no growth → monitor never refreshes).
    let msgs = host.pending_texts(&aih).await.join("\n---\n");
    assert!(
        !msgs.contains("<arcanum>"),
        "loading skills must not enqueue an injection; got: {msgs}"
    );
}

/// Loading a skill at a path with no SKILL.md errors and injects nothing.
#[tokio::test(flavor = "multi_thread")]
async fn load_skill_missing_path_does_not_inject() {
    let host = Host::new("load_skill_missing_path_does_not_inject");
    let n = nanos();
    let lab = format!("miss-{n}");
    let tag = format!("miss-tag-{n}");
    create_lab_one(&host, &lab).await;

    let agent = arcanum_agent_with_calls(vec![tool_call(
        "arcanum_load_skill",
        json!({ "laboratory_id": lab.clone(), "path": "/skills/nope" }),
    )]);
    host.apply_tag(&tag, agent).await;
    host.attach_lab(&tag, &lab).await;

    let (aih, _) = host.spawn_tag(&tag).await;
    host.wait(&aih).await;

    // Nothing was injected.
    let msgs = host.pending_texts(&aih).await.join("\n---\n");
    assert!(
        !msgs.contains("<arcanum>"),
        "a missing skill must not enqueue an injection; got: {msgs}"
    );
    // The tool surfaced the failure.
    let tool = host.tool_result_texts(&aih).await.join("\n");
    assert!(
        tool.contains("SKILL.md") || tool.contains("nope"),
        "expected the tool result to mention the missing skill; got: {tool}"
    );
}
