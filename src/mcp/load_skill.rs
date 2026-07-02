//! The `load_skill` toolset: read a laboratory's `SKILL.md`, return it to the
//! agent as the tool response, register it as the agent's loaded skill, reset
//! the monitor baseline to the current token count, and start token-usage
//! monitoring so the skill is RE-injected (refreshed) as the agent's context
//! grows. Loading itself does not enqueue — the monitor is the only injector.

use rmcp::{
    ErrorData, RoleServer, tool, tool_router,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    service::RequestContext,
};
use schemars::JsonSchema;
use serde::Deserialize;

use super::ArcanumMcp;
use super::common::{self, AIH_HEADER, RESPONSE_ID_HEADER};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadSkillRequest {
    /// The laboratory id (from `list_skills`) that contains the skill.
    pub laboratory_id: String,
    /// The skill folder path within the laboratory (from `list_skills`), e.g.
    /// `/skills/greeting`. Its `SKILL.md` is read and loaded.
    pub path: String,
}

#[tool_router(router = load_skill_tools, vis = "pub")]
impl ArcanumMcp {
    #[tool(
        name = "load_skill",
        description = "Load a skill by its laboratory id and path (from list_skills): reads its SKILL.md and keeps it re-injected into your context as you work."
    )]
    async fn load_skill(
        &self,
        Parameters(req): Parameters<LoadSkillRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let response_id = common::required_header(&ctx.extensions, RESPONSE_ID_HEADER)?;
        let aih = common::required_header(&ctx.extensions, AIH_HEADER)?;
        let token_repeat = common::token_repeat(&ctx.extensions)? as i64;

        // Read the skill's SKILL.md over the laboratory connection.
        let content = common::read_skill_md(
            &self.context.executor,
            &response_id,
            &req.laboratory_id,
            &req.path,
        )
        .await
        .ok_or_else(|| {
            ErrorData::invalid_params(
                format!("no SKILL.md at {} in laboratory {}", req.path, req.laboratory_id),
                None,
            )
        })?;

        let db = self
            .context
            .db()
            .await
            .map_err(|e| ErrorData::internal_error(format!("db: {e}"), None))?;

        // Register the loaded skill reference (its id + path — not the content,
        // which the monitor re-reads fresh on each refresh). Every load resets
        // the monitor below, so an agent can deliberately re-load to refresh.
        db.set_skill(&aih, &req.laboratory_id, &req.path, &response_id)
            .await
            .map_err(|e| ErrorData::internal_error(format!("db: {e}"), None))?;

        // Loading does NOT enqueue: the freshly loaded skill reaches the agent
        // in this tool response (returned below). Reset the monitor baseline to
        // the agent's current token count — "now" is the last time it saw the
        // skill — then start the monitor. The monitor is a REFRESHER: it
        // re-injects the skill only once usage grows past `token_repeat`.
        let baseline = self.monitor.token_usage_get(&aih).await.unwrap_or(0);
        db.set_last_total_tokens(&aih, baseline)
            .await
            .map_err(|e| ErrorData::internal_error(format!("db: {e}"), None))?;
        self.monitor.start(&aih, token_repeat);

        Ok(CallToolResult::success(vec![Content::text(content)]))
    }
}
