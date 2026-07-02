//! E2E: `list_skills` across multiple laboratories with mounted SKILL.md folders.
//!
//! Requires podman + network (to pull the base image) and the staged
//! `.objectiveai/` host — it starts real laboratory containers. Intended for
//! Linux/CI; it can't run on a machine without podman.

mod common;

use common::{Host, Mount, arcanum_agent};
use serde_json::Value;

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// Absolute path to a committed fixture dir under `test-skills/`.
fn skills_dir(sub: &str) -> String {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test-skills")
        .join(sub)
        .to_string_lossy()
        .into_owned()
}

/// `list_skills` returns one text block: a JSON array of { laboratory_id, name,
/// description?, when_to_use?, path }. Pick the first tool-result row that
/// parses as such an array (other rows may be unrelated mock tool calls).
fn skills_from(texts: &[String]) -> Vec<Value> {
    texts
        .iter()
        .find_map(|t| {
            let t = t.trim();
            if !t.starts_with('[') {
                return None;
            }
            serde_json::from_str::<Vec<Value>>(t)
                .ok()
                .filter(|arr| arr.iter().all(|v| v.get("laboratory_id").is_some()))
        })
        .unwrap_or_else(|| panic!("no list_skills array in tool results: {texts:?}"))
}

/// Whether `items` contains a skill with the given laboratory id, name, and path.
fn has(items: &[Value], lab: &str, name: &str, path: &str) -> bool {
    find(items, lab, name, path).is_some()
}

/// The skill item with the given laboratory id, name, and path, if listed.
fn find<'a>(items: &'a [Value], lab: &str, name: &str, path: &str) -> Option<&'a Value> {
    items.iter().find(|it| {
        it.get("laboratory_id").and_then(Value::as_str) == Some(lab)
            && it.get("name").and_then(Value::as_str) == Some(name)
            && it.get("path").and_then(Value::as_str) == Some(path)
    })
}

/// Two laboratories, each mounting a different `test-skills/<lab>` folder at
/// `/skills`. `list_skills` should return one item per NAMED SKILL.md, tagged
/// with the owning laboratory id — names come from frontmatter (lab-one's
/// greeting folder is named `hello-skill`), optional description/when_to_use
/// surface when present, the nested `deep-skill` in lab-two is found, and
/// lab-two's `unnamed` skill (frontmatter without `name`) is skipped.
#[tokio::test(flavor = "multi_thread")]
async fn skills_listed_across_laboratories() {
    let host = Host::new("skills_listed_across_laboratories");
    let n = nanos();
    let lab_one = format!("skills-one-{n}");
    let lab_two = format!("skills-two-{n}");
    let tag = format!("skills-tag-{n}");

    host.create_lab(
        &lab_one,
        vec![Mount {
            host: skills_dir("lab-one"),
            container: "/skills".to_string(),
        }],
        Vec::new(),
        "/",
    )
    .await;
    host.create_lab(
        &lab_two,
        vec![Mount {
            host: skills_dir("lab-two"),
            container: "/skills".to_string(),
        }],
        Vec::new(),
        "/",
    )
    .await;

    host.apply_tag(&tag, arcanum_agent()).await;
    host.attach_lab(&tag, &lab_one).await;
    host.attach_lab(&tag, &lab_two).await;

    let (aih, _response_id) = host.spawn_tag(&tag).await;
    host.wait(&aih).await;

    let items = skills_from(&host.tool_result_texts(&aih).await);

    // lab-one: hello-skill (frontmatter name ≠ folder name "greeting"), farewell.
    let hello = find(&items, &lab_one, "hello-skill", "/skills/greeting")
        .unwrap_or_else(|| panic!("missing lab-one hello-skill: {items:?}"));
    assert_eq!(
        hello.get("description").and_then(Value::as_str),
        Some("Greets people warmly"),
        "hello-skill description: {items:?}"
    );
    assert_eq!(
        hello.get("when_to_use").and_then(Value::as_str),
        Some("When a conversation is starting"),
        "hello-skill when_to_use: {items:?}"
    );
    assert!(has(&items, &lab_one, "farewell", "/skills/farewell"), "missing lab-one farewell: {items:?}");
    // lab-two: greeting (name only — optional fields omitted), summarize, nested
    // deep-skill.
    let greeting = find(&items, &lab_two, "greeting", "/skills/greeting")
        .unwrap_or_else(|| panic!("missing lab-two greeting: {items:?}"));
    assert!(greeting.get("description").is_none(), "greeting must omit description: {items:?}");
    assert!(greeting.get("when_to_use").is_none(), "greeting must omit when_to_use: {items:?}");
    assert!(has(&items, &lab_two, "summarize", "/skills/summarize"), "missing lab-two summarize: {items:?}");
    assert!(
        has(&items, &lab_two, "deep-skill", "/skills/nested/deep-skill"),
        "missing lab-two nested deep-skill: {items:?}"
    );

    // lab-two's `unnamed` skill has no frontmatter `name` → not listed.
    assert!(
        !items.iter().any(|it| it.get("path").and_then(Value::as_str) == Some("/skills/unnamed")),
        "unnamed skill must be skipped: {items:?}"
    );

    // Exactly the five NAMED SKILL.md files across both labs (root-level and
    // unnamed excluded).
    assert_eq!(items.len(), 5, "unexpected skill count: {items:?}");
}

/// Three agents in ONE state (so they share the single daemon-hosted MCP server),
/// each with a different laboratory permutation: agent A sees only lab-a, agent B
/// only lab-b, agent AB both. Spawned in parallel; each agent's `list_skills` must
/// be scoped to exactly its own attached laboratories — proving the shared daemon
/// routes per-agent by the request's response id.
#[tokio::test(flavor = "multi_thread")]
async fn skills_scoped_per_agent_across_permutations() {
    let host = Host::new("skills_scoped_per_agent_across_permutations");
    let n = nanos();
    // Two laboratories total, reused across the three agents.
    let lab_a = format!("perm-a-{n}"); // mounts lab-one (hello-skill, farewell)
    let lab_b = format!("perm-b-{n}"); // mounts lab-two (greeting, summarize, deep-skill; unnamed skipped)

    host.create_lab(
        &lab_a,
        vec![Mount { host: skills_dir("lab-one"), container: "/skills".to_string() }],
        Vec::new(),
        "/",
    )
    .await;
    host.create_lab(
        &lab_b,
        vec![Mount { host: skills_dir("lab-two"), container: "/skills".to_string() }],
        Vec::new(),
        "/",
    )
    .await;

    // Three tags, three permutations of attached laboratories.
    let tag_a = format!("perm-tag-a-{n}");
    let tag_b = format!("perm-tag-b-{n}");
    let tag_ab = format!("perm-tag-ab-{n}");

    host.apply_tag(&tag_a, arcanum_agent()).await;
    host.attach_lab(&tag_a, &lab_a).await;

    host.apply_tag(&tag_b, arcanum_agent()).await;
    host.attach_lab(&tag_b, &lab_b).await;

    host.apply_tag(&tag_ab, arcanum_agent()).await;
    host.attach_lab(&tag_ab, &lab_a).await;
    host.attach_lab(&tag_ab, &lab_b).await;

    // Spawn all three concurrently (spawn_tag streams to completion).
    let ((aih_a, _), (aih_b, _), (aih_ab, _)) = tokio::join!(
        host.spawn_tag(&tag_a),
        host.spawn_tag(&tag_b),
        host.spawn_tag(&tag_ab),
    );

    let items_a = skills_from(&host.tool_result_texts(&aih_a).await);
    let items_b = skills_from(&host.tool_result_texts(&aih_b).await);
    let items_ab = skills_from(&host.tool_result_texts(&aih_ab).await);

    // Agent A: only lab-a's two skills.
    assert_eq!(items_a.len(), 2, "agent A count: {items_a:?}");
    assert!(has(&items_a, &lab_a, "hello-skill", "/skills/greeting"), "A hello-skill: {items_a:?}");
    assert!(has(&items_a, &lab_a, "farewell", "/skills/farewell"), "A farewell: {items_a:?}");
    assert!(!items_a.iter().any(|it| it.get("laboratory_id").and_then(Value::as_str) == Some(lab_b.as_str())), "A must not see lab-b: {items_a:?}");

    // Agent B: only lab-b's three named skills.
    assert_eq!(items_b.len(), 3, "agent B count: {items_b:?}");
    assert!(has(&items_b, &lab_b, "greeting", "/skills/greeting"), "B greeting: {items_b:?}");
    assert!(has(&items_b, &lab_b, "summarize", "/skills/summarize"), "B summarize: {items_b:?}");
    assert!(has(&items_b, &lab_b, "deep-skill", "/skills/nested/deep-skill"), "B deep-skill: {items_b:?}");
    assert!(!items_b.iter().any(|it| it.get("laboratory_id").and_then(Value::as_str) == Some(lab_a.as_str())), "B must not see lab-a: {items_b:?}");

    // Agent AB: all five named skills, across both labs.
    assert_eq!(items_ab.len(), 5, "agent AB count: {items_ab:?}");
    assert!(has(&items_ab, &lab_a, "hello-skill", "/skills/greeting"), "AB lab-a hello-skill: {items_ab:?}");
    assert!(has(&items_ab, &lab_a, "farewell", "/skills/farewell"), "AB lab-a farewell: {items_ab:?}");
    assert!(has(&items_ab, &lab_b, "greeting", "/skills/greeting"), "AB lab-b greeting: {items_ab:?}");
    assert!(has(&items_ab, &lab_b, "summarize", "/skills/summarize"), "AB lab-b summarize: {items_ab:?}");
    assert!(has(&items_ab, &lab_b, "deep-skill", "/skills/nested/deep-skill"), "AB lab-b deep-skill: {items_ab:?}");
}
