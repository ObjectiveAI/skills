//! The `list_skills` toolset: enumerate every named `SKILL.md` across the
//! caller's laboratories, reading each one's YAML frontmatter.
//!
//! arcanum can't read a laboratory's filesystem directly, so for each attached
//! laboratory (an MCP server exposing a `Bash` tool) we shell out via the plugin
//! executor's `agents mcp tools call` and run a `find` that emits each
//! `SKILL.md`'s path and content, then parse the frontmatter in Rust.

use futures::future::join_all;
use rmcp::{
    ErrorData, RoleServer, tool, tool_router,
    model::{CallToolResult, Content},
    service::RequestContext,
};
use serde::Serialize;

use super::ArcanumMcp;
use super::common::{self, RESPONSE_ID_HEADER};

/// Record separator (ASCII RS, 0x1e) framing the find output: for each file the
/// command prints `<RS><path><RS><content>`. RS never appears in text and
/// survives the `Bash` tool's JSON stdout, so splitting on it is unambiguous.
const RS: char = '\u{1e}';

/// Bash command run inside each laboratory: locate every `SKILL.md`
/// (case-insensitive, pruning pseudo-filesystems for speed) and, for each, print
/// its path and full content framed by [`RS`].
const FIND_CMD: &str = "find / \\( -path /proc -o -path /sys -o -path /dev \\) -prune -o -type f -iname 'SKILL.md' -print 2>/dev/null | while IFS= read -r f; do printf '\\036%s\\036' \"$f\"; cat \"$f\" 2>/dev/null; done";

/// One discovered skill: which laboratory it lives in, its frontmatter `name`,
/// optional `description`/`when_to_use`, and its folder path within the lab.
#[derive(Serialize)]
struct SkillItem {
    laboratory_id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    when_to_use: Option<String>,
    path: String,
}

#[tool_router(router = list_skills_tools, vis = "pub")]
impl ArcanumMcp {
    #[tool(
        name = "list_skills",
        description = "List all skills across the agent's laboratories: each named SKILL.md's name, description, and when-to-use guidance (from its frontmatter), with its laboratory id and path."
    )]
    async fn list_skills(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let response_id = common::required_header(&ctx.extensions, RESPONSE_ID_HEADER)?;

        // Laboratories the agent is connected to, and each one's `Bash` tool name.
        let servers = common::list_servers(&self.context.executor, &response_id).await?;
        let labs: Vec<(String, String)> = servers
            .iter()
            .filter_map(|s| common::laboratory_id(s).map(|id| (id.to_string(), common::bash_tool(s))))
            .collect();

        // Concurrently run the find in each laboratory and collect items. A
        // laboratory that errors or returns unparseable output contributes
        // nothing (it's skipped, not fatal).
        let futures = labs.iter().map(|(lab_id, tool)| {
            let executor = &self.context.executor;
            let response_id = response_id.as_str();
            async move {
                let stdout = common::lab_bash(executor, response_id, tool, FIND_CMD).await?;
                Some(parse_skills(lab_id, &stdout))
            }
        });
        let items: Vec<SkillItem> = join_all(futures)
            .await
            .into_iter()
            .flatten() // drop skipped laboratories (None)
            .flatten() // flatten each lab's Vec<SkillItem>
            .collect();

        let body = serde_json::to_string(&items)
            .map_err(|e| ErrorData::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }
}

/// Parse the RS-framed `find` stdout into skill items. Records alternate
/// `<RS><path><RS><content>`, so splitting on [`RS`] yields (after the leading
/// empty chunk) path/content pairs. For each pair: the skill's `path` is the
/// containing folder (a `SKILL.md` at the filesystem root is excluded), and its
/// name/description/when_to_use come from the content's frontmatter. A skill
/// whose frontmatter has no `name` is skipped. Duplicates are kept.
///
/// Split paths on `/` manually rather than via `std::path` so they're parsed
/// with Linux semantics even when arcanum runs on a Windows host.
fn parse_skills(lab_id: &str, stdout: &str) -> Vec<SkillItem> {
    let mut items = Vec::new();
    let mut chunks = stdout.split(RS);
    // Everything before the first RS is noise (normally empty).
    let _ = chunks.next();
    while let Some(path) = chunks.next() {
        let content = chunks.next().unwrap_or("");
        let p = path.trim();
        if p.is_empty() {
            continue;
        }
        let i = match p.rfind('/') {
            // Root-level `/SKILL.md` (slash at index 0) or a relative bare name.
            Some(0) | None => continue,
            Some(i) => i,
        };
        let dir = &p[..i];
        if dir.is_empty() {
            continue;
        }
        let (fm, _) = common::parse_frontmatter(content);
        let Some(name) = fm.name else {
            continue; // unnamed skills aren't listed
        };
        items.push(SkillItem {
            laboratory_id: lab_id.to_string(),
            name,
            description: fm.description,
            when_to_use: fm.when_to_use,
            path: dir.to_string(),
        });
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frame `(path, content)` records the way `FIND_CMD` does.
    fn framed(records: &[(&str, &str)]) -> String {
        records
            .iter()
            .map(|(p, c)| format!("{RS}{p}{RS}{c}"))
            .collect()
    }

    #[test]
    fn parses_frontmatter_and_excludes_root() {
        let out = framed(&[
            ("/a/b/SKILL.md", "---\nname: beta\ndescription: d\nwhen_to_use: w\n---\nbody"),
            ("/x/skill.md", "---\nname: xray\n---\nbody"), // case-insensitive filename
            ("/SKILL.md", "---\nname: root\n---\nbody"),   // root-level: excluded
        ]);
        let items = parse_skills("lab1", &out);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "beta"); // frontmatter name, not folder "b"
        assert_eq!(items[0].description.as_deref(), Some("d"));
        assert_eq!(items[0].when_to_use.as_deref(), Some("w"));
        assert_eq!(items[0].path, "/a/b");
        assert_eq!(items[0].laboratory_id, "lab1");
        assert_eq!(items[1].name, "xray");
        assert!(items[1].description.is_none());
        assert_eq!(items[1].path, "/x");
    }

    #[test]
    fn skips_skills_without_a_name() {
        let out = framed(&[
            ("/a/named/SKILL.md", "---\nname: named\n---\nbody"),
            ("/a/desc-only/SKILL.md", "---\ndescription: d\n---\nbody"),
            ("/a/plain/SKILL.md", "no frontmatter at all"),
        ]);
        let items = parse_skills("lab", &out);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "named");
    }

    #[test]
    fn keeps_duplicates() {
        let rec = ("/a/b/SKILL.md", "---\nname: b\n---\nx");
        let out = framed(&[rec, rec]);
        assert_eq!(parse_skills("lab", &out).len(), 2);
    }
}
