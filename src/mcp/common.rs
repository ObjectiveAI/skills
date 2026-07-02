//! Shared helpers for the MCP tools: request headers, per-session arguments, and
//! invoking a laboratory's `Bash` tool through the plugin executor.

use indexmap::IndexMap;
use objectiveai_sdk::cli::command::agents::mcp::servers::list as servers_list;
use objectiveai_sdk::cli::command::agents::mcp::tools::call as tools_call;
use objectiveai_sdk::cli::command::plugin::PluginExecutor;
use objectiveai_sdk::laboratories::Laboratory;
use objectiveai_sdk::mcp::server::Server;
use objectiveai_sdk::mcp::tool::{CallToolRequestParams, ContentBlock};
use rmcp::{ErrorData, model::Extensions};

/// Header carrying the caller's response id (scopes the executor tool calls).
pub const RESPONSE_ID_HEADER: &str = "x-objectiveai-response-id";
/// Header carrying the caller's agent instance hierarchy.
pub const AIH_HEADER: &str = "x-objectiveai-agent-instance-hierarchy";
/// Header carrying the per-session arguments JSON (e.g. `token-repeat`).
pub const ARGUMENTS_HEADER: &str = "x-objectiveai-arguments";

/// Read a required header off the request extensions, erroring if absent/empty.
pub fn required_header(extensions: &Extensions, name: &str) -> Result<String, ErrorData> {
    let parts = extensions
        .get::<http::request::Parts>()
        .ok_or_else(|| ErrorData::invalid_params("missing request parts", None))?;
    parts
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ErrorData::invalid_params(format!("missing required header: {name}"), None))
}

/// Parse `token-repeat` (u64) from the `x-objectiveai-arguments` JSON header.
/// The host encodes argument values as strings, so accept a number or a string.
pub fn token_repeat(extensions: &Extensions) -> Result<u64, ErrorData> {
    let raw = required_header(extensions, ARGUMENTS_HEADER)?;
    parse_token_repeat(&raw)
        .ok_or_else(|| ErrorData::invalid_params("token-repeat must be a u64", None))
}

/// Pull `token-repeat` out of the arguments JSON object (number or string form).
fn parse_token_repeat(raw: &str) -> Option<u64> {
    let args: serde_json::Value = serde_json::from_str(raw).ok()?;
    match args.get("token-repeat")? {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_frontmatter, parse_token_repeat};

    #[test]
    fn token_repeat_accepts_string_and_number() {
        assert_eq!(parse_token_repeat(r#"{"token-repeat":"5000"}"#), Some(5000));
        assert_eq!(parse_token_repeat(r#"{"token-repeat":5000}"#), Some(5000));
    }

    #[test]
    fn token_repeat_rejects_missing_or_bad() {
        assert_eq!(parse_token_repeat(r#"{}"#), None);
        assert_eq!(parse_token_repeat(r#"{"token-repeat":"abc"}"#), None);
        assert_eq!(parse_token_repeat(r#"not json"#), None);
    }

    #[test]
    fn frontmatter_absent_keeps_whole_body() {
        let (fm, body) = parse_frontmatter("just body\nmore");
        assert!(fm.name.is_none() && fm.description.is_none() && fm.when_to_use.is_none());
        assert_eq!(body, "just body\nmore");
    }

    #[test]
    fn frontmatter_full_fields_and_body() {
        let (fm, body) =
            parse_frontmatter("---\nname: n\ndescription: d\nwhen_to_use: w\n---\nthe body\nline2");
        assert_eq!(fm.name.as_deref(), Some("n"));
        assert_eq!(fm.description.as_deref(), Some("d"));
        assert_eq!(fm.when_to_use.as_deref(), Some("w"));
        assert_eq!(body, "the body\nline2");
    }

    #[test]
    fn frontmatter_partial_and_extra_keys_ignored() {
        let (fm, _) = parse_frontmatter("---\nname: only\nmodel: x\n---\nb");
        assert_eq!(fm.name.as_deref(), Some("only"));
        assert!(fm.description.is_none());
        assert!(fm.when_to_use.is_none());
    }

    #[test]
    fn frontmatter_strips_matching_quotes() {
        let (fm, _) = parse_frontmatter("---\nname: \"quoted\"\ndescription: 'single'\n---\nb");
        assert_eq!(fm.name.as_deref(), Some("quoted"));
        assert_eq!(fm.description.as_deref(), Some("single"));
    }

    #[test]
    fn frontmatter_value_may_contain_colons() {
        let (fm, _) = parse_frontmatter("---\nwhen_to_use: use when: it applies\n---\nb");
        assert_eq!(fm.when_to_use.as_deref(), Some("use when: it applies"));
    }

    #[test]
    fn frontmatter_empty_value_is_absent() {
        let (fm, _) = parse_frontmatter("---\nname:\ndescription: d\n---\nb");
        assert!(fm.name.is_none());
        assert_eq!(fm.description.as_deref(), Some("d"));
    }

    #[test]
    fn frontmatter_no_closing_fence_is_absent() {
        let content = "---\nname: n\nno closing fence";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.name.is_none());
        assert_eq!(body, content);
    }

    #[test]
    fn frontmatter_crlf_tolerated() {
        let (fm, body) = parse_frontmatter("---\r\nname: n\r\n---\r\nbody\r\nx");
        assert_eq!(fm.name.as_deref(), Some("n"));
        assert_eq!(body, "body\nx");
    }
}

/// List the agent's connected MCP servers (scoped by response id).
pub async fn list_servers(
    executor: &PluginExecutor,
    response_id: &str,
) -> Result<Vec<Server>, ErrorData> {
    let result = servers_list::execute(
        executor,
        servers_list::Request {
            path_type: servers_list::Path::AgentsMcpServersList,
            response_id: response_id.to_string(),
            base: Default::default(),
        },
        None,
    )
    .await
    .map_err(|e| ErrorData::internal_error(format!("servers list: {e}"), None))?;
    Ok(result.servers)
}

/// The laboratory id of a server, if it is a client laboratory.
pub fn laboratory_id(server: &Server) -> Option<&str> {
    match &server.laboratory {
        Some(Laboratory::Client(c)) => Some(c.id.as_str()),
        None => None,
    }
}

/// The `Bash` tool name for a server. Tools surface through the proxy as
/// `<server.name>_<tool>`, so the laboratory's `Bash` tool is `<server.name>_Bash`.
pub fn bash_tool(server: &Server) -> String {
    format!("{}_Bash", server.name)
}

/// Just the `stdout` field of the laboratory `Bash` tool's JSON result.
#[derive(serde::Deserialize)]
struct BashOut {
    #[serde(default)]
    stdout: String,
}

/// Run `command` in a laboratory via its `Bash` tool (named `tool`); return
/// stdout, or `None` on any failure (executor error, no text block, unparseable
/// JSON).
pub async fn lab_bash(
    executor: &PluginExecutor,
    response_id: &str,
    tool: &str,
    command: &str,
) -> Option<String> {
    let params = CallToolRequestParams {
        name: tool.to_string(),
        arguments: Some(IndexMap::from([(
            "command".to_string(),
            serde_json::Value::String(command.to_string()),
        )])),
        _meta: None,
        task: None,
    };
    let result = tools_call::execute(
        executor,
        tools_call::Request {
            path_type: tools_call::Path::AgentsMcpToolsCall,
            response_id: response_id.to_string(),
            params,
            base: Default::default(),
        },
        None,
    )
    .await
    .ok()?;
    let text = result.content.into_iter().find_map(|b| match b {
        ContentBlock::Text(t) => Some(t.text),
        _ => None,
    })?;
    let parsed: BashOut = serde_json::from_str(&text).ok()?;
    Some(parsed.stdout)
}

/// The recognized `SKILL.md` YAML frontmatter fields. All optional; unknown keys
/// are ignored.
#[derive(Default)]
pub struct Frontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub when_to_use: Option<String>,
}

/// Split a `SKILL.md` into its (frontmatter fields, body). A file has
/// frontmatter only if its first line is exactly `---` and a later line is
/// exactly `---`; the body is everything after that closing fence. Otherwise
/// (no fence, or no closing fence) there is no frontmatter and the whole content
/// is the body — the unchanged behavior for plain files.
///
/// Only simple single-line scalar values are supported (`key: value`, optional
/// surrounding quotes). Empty values are treated as absent.
pub fn parse_frontmatter(content: &str) -> (Frontmatter, String) {
    let mut lines = content.lines();
    match lines.next() {
        Some(first) if first.trim() == "---" => {}
        _ => return (Frontmatter::default(), content.to_string()),
    }

    let mut fm_lines = Vec::new();
    let mut body_lines = Vec::new();
    let mut closed = false;
    for line in lines {
        if !closed && line.trim() == "---" {
            closed = true;
        } else if closed {
            body_lines.push(line);
        } else {
            fm_lines.push(line);
        }
    }
    // No closing fence → not valid frontmatter; keep the whole file as body.
    if !closed {
        return (Frontmatter::default(), content.to_string());
    }

    let mut fm = Frontmatter::default();
    for line in fm_lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = strip_quotes(value.trim());
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "name" => fm.name = Some(value.to_string()),
            "description" => fm.description = Some(value.to_string()),
            "when_to_use" => fm.when_to_use = Some(value.to_string()),
            _ => {}
        }
    }
    (fm, body_lines.join("\n"))
}

/// Strip one matching pair of surrounding single or double quotes, if present.
fn strip_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(&a), Some(&b)) if s.len() >= 2 && a == b && (a == b'"' || a == b'\'') => {
            &s[1..s.len() - 1]
        }
        _ => s,
    }
}

/// Read the `SKILL.md` (case-insensitive) directly under `path` in laboratory
/// `lab_id`, via the agent's live `response_id`, and return its **body**
/// (frontmatter stripped), trimmed. `None` if the lab isn't connected / the file
/// is missing / the read fails / the body is empty.
pub async fn read_skill_md(
    executor: &PluginExecutor,
    response_id: &str,
    lab_id: &str,
    path: &str,
) -> Option<String> {
    let servers = list_servers(executor, response_id).await.ok()?;
    let server = servers.iter().find(|s| laboratory_id(s) == Some(lab_id))?;
    let tool = bash_tool(server);
    let command = format!(
        "f=$(find {path} -maxdepth 1 -iname 'SKILL.md' 2>/dev/null | head -1); [ -n \"$f\" ] && cat \"$f\"",
        path = shell_single_quote(path),
    );
    let content = lab_bash(executor, response_id, &tool, &command).await?;
    let (_, body) = parse_frontmatter(&content);
    let body = body.trim();
    (!body.is_empty()).then(|| body.to_string())
}

/// Single-quote a string for safe embedding in a bash command.
pub fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
