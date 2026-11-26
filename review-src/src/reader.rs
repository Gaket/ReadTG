use std::{collections::HashSet, env, sync::Arc};

use anyhow::{Result, anyhow, bail};
use grammers_session::types::PeerId;
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    schemars::{self, JsonSchema},
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};

use crate::telegram::Telegram;

const CLOSED: &str =
    "Chat access is closed. Set TELEGRAM_ALLOWED_CHATS and restart the MCP server.";
const CHAT_LIMIT: usize = 40;
const MESSAGE_LIMIT: usize = 20;
pub const MESSAGE_TEXT_CHARS: usize = 600;
pub const TITLE_CHARS: usize = 96;

pub enum Scope {
    All,
    Only(HashSet<PeerId>),
}

impl Scope {
    pub fn from_env() -> Result<Self> {
        match env::var("TELEGRAM_ALLOWED_CHATS") {
            Ok(value) => Self::parse(&value),
            Err(env::VarError::NotPresent) => Self::parse(""),
            Err(_) => bail!("TELEGRAM_ALLOWED_CHATS must be UTF-8."),
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "all" => Ok(Self::All),
            "" => Ok(Self::Only(HashSet::new())),
            ids => {
                let peers = ids
                    .split(',')
                    .map(|id| parse_peer(id.trim()))
                    .collect::<Result<HashSet<_>>>()
                    .map_err(|_| anyhow!("TELEGRAM_ALLOWED_CHATS must be a comma-separated list of chat IDs, empty, or all."))?;
                Ok(Self::Only(peers))
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Only(ids) if ids.is_empty())
    }

    pub fn allows(&self, peer: PeerId) -> bool {
        match self {
            Self::All => true,
            Self::Only(ids) => ids.contains(&peer),
        }
    }
}

fn parse_peer(value: &str) -> Result<PeerId> {
    value
        .parse::<i64>()
        .ok()
        .and_then(PeerId::from_bot_api_dialog_id)
        .ok_or_else(|| anyhow!("Use a chat_id returned by list_chats."))
}

fn search_text(value: Option<String>, label: &str, max: usize) -> Result<Option<String>> {
    value
        .map(|value| {
            let text = value.trim();
            if text.is_empty() || text.chars().count() > max {
                bail!("{label} must contain 1–{max} characters.");
            }
            Ok(text.to_owned())
        })
        .transpose()
}

fn bounded(value: Option<usize>, default: usize, max: usize, label: &str) -> Result<usize> {
    let value = value.unwrap_or(default);
    if !(1..=max).contains(&value) {
        bail!("{label} must be between 1 and {max}.");
    }
    Ok(value)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatRequest {
    #[schemars(
        description = "Optional case-insensitive name or username substring.",
        length(max = 100)
    )]
    pub name: Option<String>,
    #[schemars(description = "Defaults to 12.", range(min = 1, max = 40))]
    pub limit: Option<usize>,
}

pub struct ChatParams {
    pub name: Option<String>,
    pub limit: usize,
}

impl ChatRequest {
    pub fn validate(self) -> Result<ChatParams> {
        Ok(ChatParams {
            name: search_text(self.name, "name", 100)?.map(|name| name.to_lowercase()),
            limit: bounded(self.limit, 12, CHAT_LIMIT, "limit")?,
        })
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MessageRequest {
    #[schemars(description = "Use the chat_id from list_chats.", length(max = 16))]
    chat_id: String,
    #[schemars(
        description = "Omit for recent history. Supply text for Telegram search.",
        length(max = 160)
    )]
    query: Option<String>,
    #[schemars(
        description = "Continue with next_before_id and the same query.",
        range(min = 1)
    )]
    before_id: Option<i32>,
    #[schemars(description = "Defaults to 8.", range(min = 1, max = 20))]
    limit: Option<usize>,
}

pub struct MessageParams {
    pub peer: PeerId,
    pub query: Option<String>,
    pub before_id: i32,
    pub limit: usize,
}

impl MessageRequest {
    fn validate(self, scope: &Scope) -> Result<MessageParams> {
        let peer = parse_peer(&self.chat_id)?;
        if !scope.allows(peer) {
            bail!("Chat is outside the configured scope.");
        }
        if self.before_id.is_some_and(|id| id <= 0) {
            bail!("before_id must be positive.");
        }

        Ok(MessageParams {
            peer,
            query: search_text(self.query, "query", 160)?,
            before_id: self.before_id.unwrap_or(0),
            limit: bounded(self.limit, 8, MESSAGE_LIMIT, "limit")?,
        })
    }
}

#[derive(Serialize)]
pub struct Chat {
    pub chat_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

#[derive(Serialize)]
pub struct ChatPage {
    pub chats: Vec<Chat>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub more: bool,
}

#[derive(Serialize)]
pub struct Message {
    pub id: i32,
    pub date: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    pub text: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[derive(Serialize)]
pub struct MessagePage {
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_before_id: Option<i32>,
}

pub fn excerpt(text: &str, max: usize) -> (&str, bool) {
    match text.char_indices().nth(max) {
        Some((end, _)) => (&text[..end], true),
        None => (text, false),
    }
}

#[derive(Clone)]
pub struct Reader {
    scope: Arc<Scope>,
    telegram: Option<Arc<Telegram>>,
    tool_router: Arc<ToolRouter<Self>>,
}

impl Reader {
    pub fn new(scope: Scope, telegram: Option<Telegram>) -> Self {
        Self {
            scope: Arc::new(scope),
            telegram: telegram.map(Arc::new),
            tool_router: Arc::new(Self::tool_router()),
        }
    }

    fn telegram(&self) -> Result<&Telegram> {
        self.telegram.as_deref().ok_or_else(|| anyhow!(CLOSED))
    }
}

// Return one compact JSON text block to avoid duplicating the payload in structuredContent.
fn reply(result: Result<impl Serialize>) -> CallToolResult {
    match result.and_then(|page| Ok(serde_json::to_string(&page)?)) {
        Ok(json) => CallToolResult::success(vec![ContentBlock::text(json)]),
        Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
    }
}

#[tool_router]
impl Reader {
    #[tool(
        description = "Find Telegram chats in the configured scope.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn list_chats(&self, Parameters(request): Parameters<ChatRequest>) -> CallToolResult {
        let result = async {
            let telegram = self.telegram()?;
            telegram.list_chats(&self.scope, request.validate()?).await
        };
        reply(result.await)
    }

    #[tool(
        description = "Read recent messages or search within one Telegram chat.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn read_messages(
        &self,
        Parameters(request): Parameters<MessageRequest>,
    ) -> CallToolResult {
        let result = async {
            let telegram = self.telegram()?;
            telegram.read_messages(request.validate(&self.scope)?).await
        };
        reply(result.await)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Reader {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("telegram-reader-mcp", env!("CARGO_PKG_VERSION")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_fails_closed_and_preserves_peer_types() {
        let user = parse_peer("123456789012").unwrap();
        let group = parse_peer("-123456789012").unwrap();
        let channel = parse_peer("-1001234567890").unwrap();
        let closed = Scope::parse("  ").unwrap();
        assert!(closed.is_closed());
        assert!(!closed.allows(user));
        let allowed = Scope::parse(" 123456789012, -1001234567890, 123456789012 ").unwrap();
        assert!(allowed.allows(user));
        assert!(!allowed.allows(group));
        assert!(allowed.allows(channel));
        assert!(Scope::parse("all").unwrap().allows(group));
        for invalid in ["all,123", "123,", "0", "@chat", "9999999999999999999"] {
            assert!(Scope::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn validates_message_requests_before_the_telegram_call() {
        let scope = Scope::parse("123").unwrap();
        let request = |json: &str| {
            serde_json::from_str::<MessageRequest>(json)
                .unwrap()
                .validate(&scope)
        };
        let default = request(r#"{"chat_id":"123"}"#).unwrap();
        assert_eq!(
            (default.limit, default.before_id, default.query),
            (8, 0, None)
        );
        let search =
            request(r#"{"chat_id":"123","query":"  invoices ","before_id":10,"limit":2}"#).unwrap();
        assert_eq!(
            (search.query.as_deref(), search.before_id, search.limit),
            (Some("invoices"), 10, 2)
        );
        for json in [
            r#"{"chat_id":"-123"}"#,
            r#"{"chat_id":"123","before_id":0}"#,
            r#"{"chat_id":"123","limit":0}"#,
            r#"{"chat_id":"123","limit":21}"#,
            r#"{"chat_id":"123","query":" "}"#,
        ] {
            assert!(request(json).is_err(), "{json}");
        }
    }

    #[test]
    fn excerpts_keep_utf8_and_only_flag_cut_text() {
        assert_eq!(excerpt("é🌊猫", 2), ("é🌊", true));
        assert_eq!(excerpt("é🌊猫", 3), ("é🌊猫", false));
        assert_eq!(excerpt("", 2), ("", false));
    }
}
