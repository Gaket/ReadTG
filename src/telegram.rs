use std::{borrow::Cow, env, fs::File, future::Future, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use grammers_client::{
    Client, SignInError,
    client::{ClientConfiguration, DialogIter},
    message::Message as TelegramMessage,
    peer::Peer,
    tl,
};
use grammers_mtsender::{AutoSleep, ConnectionParams, InvocationError, SenderPool};
use grammers_session::{
    storages::SqliteSession,
    types::{PeerId, PeerRef},
};
use tokio::{sync::Semaphore, time::timeout};
use zeroize::Zeroizing;

use crate::{
    reader::{
        Chat, ChatPage, ChatParams, Message, MessagePage, MessageParams, Scope, TITLE_CHARS,
        TextCursor, excerpt,
    },
    state,
};

mod personal;

const READ_TIMEOUT: Duration = Duration::from_secs(15);
const DIALOG_PAGE_LIMIT: usize = 100;
const MESSAGE_RESULT_BYTES: usize = 48 * 1024;
// Covers the fixed MCP result wrapper, the empty page, and cursor fields.
const MESSAGE_ENVELOPE_BYTES: usize = 256;
const TRUNCATED_BYTES: usize = r#",\"truncated\":true"#.len();
const UNSUPPORTED_RICH_TEXT: &str = "[Unsupported Telegram rich text]";

pub struct Telegram {
    client: Client,
    requests: Semaphore,
    _session_lock: File,
}

impl Telegram {
    pub async fn connect(create: bool) -> Result<Self> {
        let api_id = env::var("TELEGRAM_API_ID")
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|id| *id > 0)
            .context("Set TELEGRAM_API_ID to your Telegram application ID.")?;
        let path = state::directory()?.join(state::SESSION_FILE);
        let session_lock = state::private_file(&path, create)?;
        session_lock.try_lock().map_err(|_| {
            anyhow!("The Telegram session is in use. Stop the other reader before opening it.")
        })?;

        let session = SqliteSession::open(path)
            .await
            .map_err(|_| anyhow!("Could not open the Telegram session database."))?;
        let SenderPool { handle, runner, .. } =
            SenderPool::with_configuration(Arc::new(session), api_id, connection_params());
        let client = Client::with_configuration(
            handle,
            ClientConfiguration {
                auto_cache_peers: create,
            },
        );
        tokio::spawn(runner.run());
        Ok(Self {
            client,
            requests: Semaphore::new(1),
            _session_lock: session_lock,
        })
    }

    pub async fn authorized(&self) -> Result<bool> {
        timeout(READ_TIMEOUT, self.client.is_authorized())
            .await
            .context("Telegram authorization check timed out.")?
            .map_err(telegram_error)
    }

    pub async fn login(&self) -> Result<()> {
        if self.authorized().await? {
            eprintln!("This Telegram session is already signed in.");
            return Ok(());
        }

        let token = {
            let api_hash = secret("Telegram application API hash: ")?;
            let phone = secret("Telegram phone number with country code: ")?;
            timeout(
                READ_TIMEOUT,
                self.client
                    .request_login_code(phone.trim(), api_hash.trim()),
            )
            .await
            .context("Telegram login code request timed out.")?
            .map_err(telegram_error)?
        };

        let outcome = {
            let code = secret("Telegram login code: ")?;
            timeout(READ_TIMEOUT, self.client.sign_in(&token, code.trim()))
                .await
                .context("Telegram sign-in timed out.")?
        };
        match outcome {
            Ok(_) => {}
            Err(SignInError::PasswordRequired(password_token)) => {
                let password = secret("Telegram two-step verification password: ")?;
                timeout(
                    READ_TIMEOUT,
                    self.client
                        .check_password(password_token, password.as_str()),
                )
                .await
                .context("Telegram password check timed out.")?
                .map_err(|_| {
                    anyhow!("Telegram did not accept the two-step verification password.")
                })?;
            }
            Err(_) => {
                bail!("Telegram did not complete sign-in. Check the code and account, then retry.")
            }
        }

        eprintln!("Telegram session saved. You can now choose your chat scope.");
        Ok(())
    }

    pub async fn list_chats(&self, scope: &Scope, params: ChatParams) -> Result<ChatPage> {
        let _permit = self
            .requests
            .try_acquire()
            .context("Telegram reader is busy. Retry shortly.")?;
        timeout(READ_TIMEOUT, self.find_chats(scope, params))
            .await
            .context("Telegram chat lookup timed out.")?
    }

    async fn find_chats(&self, scope: &Scope, params: ChatParams) -> Result<ChatPage> {
        let budget = discovery_fetch_limit(scope, &params);
        let mut dialogs = self.client.iter_dialogs().limit(budget);
        collect_chats(scope, params, budget, &mut dialogs).await
    }

    async fn dialog_ref(&self, id: PeerId) -> Result<PeerRef> {
        // A cached or resolvable Telegram peer may never have been an account dialog.
        let mut dialogs = self.client.iter_dialogs();
        while let Some(dialog) = dialogs.next().await.map_err(telegram_error)? {
            if dialog.peer_id() == id {
                return Ok(dialog.peer_ref());
            }
        }
        bail!("Chat was not found in this account's dialogs.");
    }

    pub async fn read_messages(&self, params: MessageParams) -> Result<MessagePage> {
        let _permit = self
            .requests
            .try_acquire()
            .context("Telegram reader is busy. Retry shortly.")?;
        timeout(READ_TIMEOUT, self.fetch_messages(params))
            .await
            .context("Telegram message lookup timed out.")?
    }

    async fn fetch_messages(&self, params: MessageParams) -> Result<MessagePage> {
        let peer = self.dialog_ref(params.peer).await?;
        let limit = params.limit + 1;
        // Telegram's offset is exclusive. At the maximum ID, start at the newest message.
        let offset_id = params.text_cursor.map_or(params.before_id, |cursor| {
            cursor.message_id.checked_add(1).unwrap_or(0)
        });
        let mut page = MessagePageBuilder::new(params.limit, params.text_cursor);
        if let Some(query) = &params.query {
            let mut iter = self
                .client
                .search_messages(peer)
                .query(query)
                .offset_id(offset_id)
                .limit(limit);
            while let Some(message) = iter.next().await.map_err(telegram_error)? {
                let (row, text) = project(&message);
                if !page.push(row, text)? {
                    break;
                }
            }
        } else {
            let mut iter = self
                .client
                .iter_messages(peer)
                .offset_id(offset_id)
                .limit(limit);
            while let Some(message) = iter.next().await.map_err(telegram_error)? {
                let (row, text) = project(&message);
                if !page.push(row, text)? {
                    break;
                }
            }
        }
        page.finish()
    }
}

fn discovery_fetch_limit(scope: &Scope, params: &ChatParams) -> usize {
    if matches!(scope, Scope::All) && params.name.is_none() {
        params.limit + 1
    } else {
        DIALOG_PAGE_LIMIT
    }
}

trait DialogPeers {
    fn next_peer(&mut self) -> impl Future<Output = Result<Option<Peer>>> + Send;
}

impl DialogPeers for DialogIter {
    async fn next_peer(&mut self) -> Result<Option<Peer>> {
        Ok(self
            .next()
            .await
            .map_err(telegram_error)?
            .map(|dialog| dialog.peer))
    }
}

async fn collect_chats(
    scope: &Scope,
    params: ChatParams,
    budget: usize,
    dialogs: &mut (impl DialogPeers + Send),
) -> Result<ChatPage> {
    let mut page = ChatPage {
        chats: Vec::with_capacity(params.limit),
        more: false,
        partial: false,
    };
    for _ in 0..budget {
        let Some(peer) = dialogs.next_peer().await? else {
            return Ok(page);
        };
        let id = peer.id();
        if !scope.allows(id) {
            continue;
        }
        let Some(id) = id.bot_api_dialog_id() else {
            continue;
        };
        let display_name = peer_display_name(&peer);
        let name = if display_name.is_empty() {
            "(unnamed)"
        } else {
            display_name.as_ref()
        };
        let username = peer.username();
        if let Some(filter) = &params.name
            && !name.to_lowercase().contains(filter)
            && !username
                .is_some_and(|name| name.to_lowercase().contains(filter.trim_start_matches('@')))
        {
            continue;
        }
        if page.chats.len() == params.limit {
            page.more = true;
            return Ok(page);
        }
        page.chats.push(Chat {
            chat_id: id.to_string(),
            name: excerpt(name, TITLE_CHARS).0.to_owned(),
            username: username.map(|value| excerpt(value, TITLE_CHARS).0.to_owned()),
        });
    }
    page.partial = true;
    Ok(page)
}

fn connection_params() -> ConnectionParams {
    // Report positive FloodWaits to the caller while keeping grammers' short I/O retry.
    ConnectionParams {
        retry_policy: Box::new(AutoSleep {
            threshold: Duration::ZERO,
            ..AutoSleep::default()
        }),
        ..ConnectionParams::default()
    }
}

fn peer_display_name(peer: &Peer) -> Cow<'_, str> {
    match peer {
        Peer::User(user) => full_name(user.first_name(), user.last_name()),
        _ => Cow::Borrowed(peer.name().unwrap_or_default()),
    }
}

fn full_name<'a>(first: Option<&'a str>, last: Option<&'a str>) -> Cow<'a, str> {
    let first = first.unwrap_or_default().trim();
    let last = last.unwrap_or_default().trim();
    match (first, last) {
        ("", name) | (name, "") => Cow::Borrowed(name),
        (first, last) => Cow::Owned(format!("{first} {last}")),
    }
}

struct MessagePageBuilder {
    page: MessagePage,
    limit: usize,
    remaining: usize,
    resume: Option<TextCursor>,
}

impl MessagePageBuilder {
    fn new(limit: usize, resume: Option<TextCursor>) -> Self {
        Self {
            page: MessagePage {
                messages: Vec::with_capacity(limit),
                next_before_id: None,
                next_text_cursor: None,
            },
            limit,
            remaining: MESSAGE_RESULT_BYTES - MESSAGE_ENVELOPE_BYTES,
            resume,
        }
    }

    fn push(&mut self, mut row: Message, text: &str) -> Result<bool> {
        if self.page.messages.len() == self.limit {
            self.stop_before_next();
            return Ok(false);
        }

        let start = match self.resume.take() {
            Some(cursor)
                if cursor.message_id == row.id
                    && cursor.byte_offset < text.len()
                    && text.is_char_boundary(cursor.byte_offset) =>
            {
                cursor.byte_offset
            }
            Some(_) => bail!("Message text is no longer available at this cursor."),
            None => 0,
        };
        let text = &text[start..];
        let row_bytes = encoded_row_bytes(&row)?;
        let (end, text_bytes) = encoded_text_prefix(text, self.remaining.saturating_sub(row_bytes));
        if row_bytes + text_bytes <= self.remaining && end == text.len() {
            row.text = text.to_owned();
            self.remaining -= row_bytes + text_bytes;
            self.page.messages.push(row);
            return Ok(true);
        }
        if !self.page.messages.is_empty() {
            self.stop_before_next();
            return Ok(false);
        }

        let (end, _) = encoded_text_prefix(
            text,
            self.remaining.saturating_sub(row_bytes + TRUNCATED_BYTES),
        );
        anyhow::ensure!(end > 0, "Message metadata exceeds the response budget.");
        self.page.next_text_cursor = Some(TextCursor {
            message_id: row.id,
            byte_offset: start + end,
        });
        row.text = text[..end].to_owned();
        row.truncated = true;
        self.page.messages.push(row);
        Ok(false)
    }

    fn stop_before_next(&mut self) {
        self.page.next_before_id = self.page.messages.last().map(|row| row.id);
    }

    fn finish(self) -> Result<MessagePage> {
        anyhow::ensure!(
            self.resume.is_none(),
            "Message text is no longer available at this cursor."
        );
        Ok(self.page)
    }
}

// The serialized row is inside the MCP text block's JSON string.
fn encoded_row_bytes(row: &impl serde::Serialize) -> Result<usize> {
    let metadata = serde_json::to_vec(row)?;
    Ok(metadata.len()
        + metadata
            .iter()
            .filter(|&&byte| matches!(byte, b'"' | b'\\'))
            .count()
        + 1) // possible array comma
}

// Count text after both JSON encoding layers, stopping only at a UTF-8 boundary.
fn encoded_text_prefix(text: &str, budget: usize) -> (usize, usize) {
    let mut bytes = 0;
    for (offset, ch) in text.char_indices() {
        let cost = match ch {
            '"' | '\\' => 4,
            '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 3,
            '\0'..='\u{1f}' => 7,
            _ => ch.len_utf8(),
        };
        if bytes + cost > budget {
            return (offset, bytes);
        }
        bytes += cost;
    }
    (text.len(), bytes)
}

fn project(message: &TelegramMessage) -> (Message, &str) {
    let (kind, action) = match &message.raw {
        tl::enums::Message::Message(raw) => (
            match &raw.media {
                None | Some(tl::enums::MessageMedia::Empty) => None,
                Some(tl::enums::MessageMedia::Photo(_)) => Some("photo"),
                Some(tl::enums::MessageMedia::Document(_)) => Some("document"),
                Some(_) => Some("media"),
            },
            None,
        ),
        tl::enums::Message::Service(raw) => (
            Some("service"),
            match &raw.action {
                tl::enums::MessageAction::ChatJoinedByLink(_) => Some("join_by_link"),
                tl::enums::MessageAction::ChatJoinedByRequest => Some("join_by_request"),
                _ => None,
            },
        ),
        tl::enums::Message::Empty(_) => (None, None),
    };
    let row = Message {
        id: message.id(),
        date: message.date().to_string(),
        sender: message
            .sender()
            .map(peer_display_name)
            .filter(|name| !name.is_empty())
            .map(|name| excerpt(&name, TITLE_CHARS).0.to_owned()),
        kind,
        action,
        text: String::new(),
        truncated: false,
    };
    (
        row,
        plain_text(message.text(), message.rich_message().is_some()),
    )
}

fn plain_text(plain: &str, has_rich_text: bool) -> &str {
    if plain.is_empty() && has_rich_text {
        UNSUPPORTED_RICH_TEXT
    } else {
        plain
    }
}

fn secret(prompt: &str) -> Result<Zeroizing<String>> {
    rpassword::prompt_password(prompt)
        .map(Zeroizing::new)
        .context("Could not read from the local terminal.")
}

fn telegram_error(error: InvocationError) -> anyhow::Error {
    match &error {
        InvocationError::Rpc(rpc) if rpc.code == 401 => anyhow!("Telegram session needs login."),
        InvocationError::Rpc(rpc) if rpc.code == 420 => match rpc.value {
            Some(seconds) => {
                anyhow!("Telegram rate-limited the request. Retry after {seconds} seconds.")
            }
            None => anyhow!("Telegram rate-limited the request. Retry later."),
        },
        _ => anyhow!("Telegram could not complete the request."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::reply;
    use grammers_client::message::InputMessage;
    use grammers_mtsender::{RetryContext, RpcError};
    use grammers_session::storages::MemorySession;
    use std::{io, num::NonZeroU32, ops::ControlFlow};

    #[test]
    fn flood_wait_returns_retry_delay_without_sleeping() {
        let params = connection_params();
        for seconds in [4, 42, 61] {
            let context = RetryContext {
                fail_count: NonZeroU32::MIN,
                slept_so_far: Duration::ZERO,
                error: InvocationError::Rpc(RpcError {
                    code: 420,
                    name: "FLOOD_WAIT".into(),
                    value: Some(seconds),
                    caused_by: None,
                }),
            };
            assert!(params.retry_policy.should_retry(&context).is_break());
            assert!(
                telegram_error(context.error)
                    .to_string()
                    .contains(&format!("Retry after {seconds} seconds"))
            );
        }
    }

    #[test]
    fn unfiltered_operator_discovery_fetches_only_one_lookahead() {
        let mut params = ChatParams {
            name: None,
            limit: 1,
        };
        assert_eq!(discovery_fetch_limit(&Scope::All, &params), 2);
        assert_eq!(
            discovery_fetch_limit(&Scope::Only(Default::default()), &params),
            DIALOG_PAGE_LIMIT
        );
        params.name = Some("contact".into());
        assert_eq!(
            discovery_fetch_limit(&Scope::All, &params),
            DIALOG_PAGE_LIMIT
        );
    }

    fn fixture_user(client: &Client, id: i64, name: &str, username: &str) -> Peer {
        use grammers_client::peer::User;
        use tl::{Deserializable, Serializable};
        let mut body = Vec::new();
        let mut write = |bytes: &[u8]| body.extend_from_slice(bytes);
        const FIRST_NAME_AND_USERNAME: u32 = (1 << 1) | (1 << 3);
        FIRST_NAME_AND_USERNAME.serialize(&mut write);
        0u32.serialize(&mut write);
        id.serialize(&mut write);
        name.to_owned().serialize(&mut write);
        username.to_owned().serialize(&mut write);
        Peer::User(User::from_raw(
            client,
            tl::types::User::from_bytes(&body).unwrap().into(),
        ))
    }

    struct DialogFixture {
        client: Client,
        total: usize,
        examined: usize,
        requests: usize,
    }

    impl DialogPeers for DialogFixture {
        async fn next_peer(&mut self) -> Result<Option<Peer>> {
            if self.examined.is_multiple_of(DIALOG_PAGE_LIMIT) {
                self.requests += 1;
            }
            if self.examined == self.total {
                return Ok(None);
            }
            self.examined += 1;
            let (name, username) = match self.examined {
                3 => ("Разработка", "dev_chat"),
                4 => ("Restricted", "dev_private"),
                _ => ("Other", "other"),
            };
            Ok(Some(fixture_user(
                &self.client,
                self.examined as i64,
                name,
                username,
            )))
        }
    }

    async fn fixture_scan(
        scope: &Scope,
        params: ChatParams,
        budget: usize,
        total: usize,
    ) -> (ChatPage, usize) {
        let mut fixture = DialogFixture {
            client: Client::new(SenderPool::new(Arc::new(MemorySession::default()), 1).handle),
            total,
            examined: 0,
            requests: 0,
        };
        let page = collect_chats(scope, params, budget, &mut fixture)
            .await
            .unwrap();
        (page, fixture.requests)
    }

    #[tokio::test]
    async fn early_match_stays_within_one_dialog_page() {
        let params = || ChatParams {
            name: Some("разработка".into()),
            limit: 5,
        };
        let (uncapped, requests) = fixture_scan(&Scope::All, params(), usize::MAX, 955).await;
        assert_eq!(requests, 10);
        assert_eq!(uncapped.chats.len(), 1);
        assert!(!uncapped.partial);

        let budget = discovery_fetch_limit(&Scope::All, &params());
        let (bounded, requests) = fixture_scan(&Scope::All, params(), budget, 955).await;
        assert_eq!(requests, 1);
        assert_eq!(bounded.chats.len(), 1);
        assert_eq!(bounded.chats[0].name, "Разработка");
        assert!(bounded.partial);
        assert!(!bounded.more);
    }

    #[tokio::test]
    async fn missing_matches_distinguish_a_capped_scan_from_exhaustion() {
        let params = || ChatParams {
            name: Some("absent".into()),
            limit: 5,
        };
        let budget = discovery_fetch_limit(&Scope::All, &params());
        let (capped, requests) = fixture_scan(&Scope::All, params(), budget, 955).await;
        assert_eq!(requests, 1);
        assert!(capped.chats.is_empty() && capped.partial && !capped.more);
        let (complete, requests) = fixture_scan(&Scope::All, params(), budget, 7).await;
        assert_eq!(requests, 1);
        assert!(complete.chats.is_empty() && !complete.partial && !complete.more);
    }

    #[tokio::test]
    async fn username_matching_still_checks_scope_and_bounded_lookahead() {
        let scope = Scope::Only([PeerId::user(3).unwrap()].into_iter().collect());
        let params = ChatParams {
            name: Some("@dev".into()),
            limit: 1,
        };
        let budget = discovery_fetch_limit(&scope, &params);
        let (page, requests) = fixture_scan(&scope, params, budget, 12).await;
        assert_eq!(requests, 1);
        assert_eq!(page.chats.len(), 1);
        assert_eq!(page.chats[0].username.as_deref(), Some("dev_chat"));
        assert!(!page.more && !page.partial);

        let params = ChatParams {
            name: None,
            limit: 5,
        };
        let budget = discovery_fetch_limit(&Scope::All, &params);
        let (page, requests) = fixture_scan(&Scope::All, params, budget, 955).await;
        assert_eq!(requests, 1);
        assert_eq!(page.chats.len(), 5);
        assert!(page.more && !page.partial);
    }

    #[test]
    fn transient_io_keeps_one_short_retry() {
        let params = connection_params();
        let mut context = RetryContext {
            fail_count: NonZeroU32::MIN,
            slept_so_far: Duration::ZERO,
            error: InvocationError::Io(io::ErrorKind::ConnectionReset.into()),
        };
        assert_eq!(
            params.retry_policy.should_retry(&context),
            ControlFlow::Continue(Duration::from_secs(1))
        );
        context.fail_count = NonZeroU32::new(2).unwrap();
        context.slept_so_far = Duration::from_secs(1);
        assert!(params.retry_policy.should_retry(&context).is_break());
    }

    #[test]
    fn full_names_keep_single_parts_and_trim_whitespace() {
        assert_eq!(full_name(None, Some(" 李 ")), "李");
        assert_eq!(full_name(Some(" "), Some(" Núñez ")), "Núñez");
        assert_eq!(full_name(Some(" Ada "), None), "Ada");
        assert_eq!(full_name(Some(" Ana "), Some(" Núñez ")), "Ana Núñez");
        assert_eq!(full_name(None, None), "");
    }

    fn sample_message() -> TelegramMessage {
        let pool = SenderPool::new(Arc::new(MemorySession::default()), 1);
        let client = Client::new(pool.handle);
        TelegramMessage::from_raw_short_updates(
            &client,
            tl::types::UpdateShortSentMessage {
                out: false,
                id: 1,
                pts: 0,
                pts_count: 0,
                date: 0,
                media: None,
                entities: None,
                ttl_period: None,
            },
            InputMessage::new(),
            PeerId::channel(1).unwrap().to_ambient_ref(),
        )
    }

    #[test]
    fn block_only_post_is_marked_and_single_chat_keeps_full_plain_text() {
        let mut message = sample_message();
        assert!(project(&message).1.is_empty());
        let tl::enums::Message::Message(raw) = &mut message.raw else {
            panic!("expected a regular Telegram message");
        };
        raw.rich_message = Some(
            tl::types::RichMessage {
                rtl: false,
                part: false,
                blocks: vec![
                    tl::types::PageBlockParagraph {
                        text: tl::types::TextPlain {
                            text: "Rich paragraph".into(),
                        }
                        .into(),
                    }
                    .into(),
                ],
                photos: vec![],
                documents: vec![],
            }
            .into(),
        );
        assert_eq!(project(&message).1, UNSUPPORTED_RICH_TEXT);

        let tl::enums::Message::Message(raw) = &mut message.raw else {
            panic!("expected a regular Telegram message");
        };
        let plain = "猫".repeat(4096);
        raw.message = plain.clone();
        let (row, text) = project(&message);
        let mut page = MessagePageBuilder::new(8, None);
        assert!(page.push(row, text).unwrap());
        let page = page.finish().unwrap();
        assert_eq!(page.messages[0].text, plain);
        assert!(!page.messages[0].truncated);
    }

    #[test]
    fn projection_labels_captionless_media_and_join_by_link() {
        let mut message = sample_message();
        let tl::enums::Message::Message(raw) = &mut message.raw else {
            panic!("expected a regular Telegram message");
        };
        raw.media = Some(
            tl::types::MessageMediaPhoto {
                spoiler: false,
                live_photo: false,
                photo: None,
                ttl_seconds: None,
                video: None,
            }
            .into(),
        );
        let (row, text) = project(&message);
        assert_eq!((row.kind, row.action, text), (Some("photo"), None, ""));

        let tl::enums::Message::Message(raw) = &mut message.raw else {
            panic!("expected a regular Telegram message");
        };
        raw.media = Some(
            tl::types::MessageMediaDocument {
                nopremium: false,
                spoiler: false,
                video: false,
                round: false,
                voice: false,
                document: None,
                alt_documents: None,
                video_cover: None,
                video_timestamp: None,
                ttl_seconds: None,
            }
            .into(),
        );
        let (row, text) = project(&message);
        assert_eq!((row.kind, row.action, text), (Some("document"), None, ""));

        message.raw = tl::types::MessageService {
            out: false,
            mentioned: false,
            media_unread: false,
            reactions_are_possible: false,
            silent: false,
            post: false,
            legacy: false,
            id: message.id(),
            from_id: None,
            peer_id: tl::types::PeerChannel { channel_id: 1 }.into(),
            saved_peer_id: None,
            reply_to: None,
            date: 0,
            action: tl::types::MessageActionChatJoinedByLink { inviter_id: 2 }.into(),
            reactions: None,
            ttl_period: None,
        }
        .into();
        let (row, text) = project(&message);
        assert_eq!(
            (row.kind, row.action, text),
            (Some("service"), Some("join_by_link"), "")
        );
        let mut page = MessagePageBuilder::new(8, None);
        assert!(page.push(row, text).unwrap());
        let result = reply(Ok(page.finish().unwrap()));
        assert!(
            result.content[0]
                .as_text()
                .unwrap()
                .text
                .contains(r#""action":"join_by_link","text":"""#)
        );
    }

    fn message_row(id: i32) -> Message {
        Message {
            id,
            date: "2026-11-24T00:00:00Z".into(),
            sender: Some("\0".repeat(TITLE_CHARS)),
            kind: None,
            action: None,
            text: String::new(),
            truncated: false,
        }
    }

    fn assert_encoded_budget(page: MessagePage) -> usize {
        let bytes = serde_json::to_vec(&reply(Ok(page))).unwrap().len();
        assert!(bytes <= MESSAGE_RESULT_BYTES, "{bytes}");
        bytes
    }

    #[test]
    fn count_cutoff_uses_last_returned_message_and_can_return_one_hundred() {
        let mut page = MessagePageBuilder::new(100, None);
        for id in (2..=101).rev() {
            let mut row = message_row(id);
            row.sender = None;
            assert!(page.push(row, "short").unwrap());
        }
        assert!(!page.push(message_row(1), "").unwrap());
        let page = page.finish().unwrap();
        assert_eq!(page.messages.len(), 100);
        assert_eq!(page.next_before_id, Some(2));
        assert!(page.next_text_cursor.is_none());
        assert_encoded_budget(page);
    }

    #[test]
    fn byte_cutoff_resumes_at_the_first_omitted_message() {
        let text = "猫🌊\"\\\n\r\t\u{8}\u{c}\0\u{1f}".repeat(750);
        let mut first = MessagePageBuilder::new(100, None);
        assert!(first.push(message_row(9), &text).unwrap());
        assert!(!first.push(message_row(8), &text).unwrap());
        let first = first.finish().unwrap();
        assert_eq!(first.next_before_id, Some(9));
        assert_eq!(first.messages[0].text, text);
        assert_encoded_budget(first);

        let mut second = MessagePageBuilder::new(100, None);
        assert!(second.push(message_row(8), &text).unwrap());
        assert!(second.push(message_row(7), "older").unwrap());
        let second = second.finish().unwrap();
        assert_eq!(
            second.messages.iter().map(|row| row.id).collect::<Vec<_>>(),
            [8, 7]
        );
        assert_eq!(second.next_before_id, None);
        assert_encoded_budget(second);
    }

    #[test]
    fn oversized_text_advances_on_utf8_boundaries_then_resumes_older_messages() {
        let text = "é猫🌊\"\\\n\r\t\u{8}\u{c}\0\u{1f}".repeat(3000);
        let mut resume = None;
        let mut combined = String::new();
        for _ in 0..2 {
            let mut page = MessagePageBuilder::new(8, resume);
            assert!(!page.push(message_row(9), &text).unwrap());
            let page = page.finish().unwrap();
            assert_eq!(page.next_before_id, None);
            assert!(page.messages[0].truncated);
            combined.push_str(&page.messages[0].text);
            resume = page.next_text_cursor;
            assert_eq!(resume.unwrap().byte_offset, combined.len());
            assert!(assert_encoded_budget(page) > MESSAGE_RESULT_BYTES - 512);
        }
        let mut last = MessagePageBuilder::new(1, resume);
        assert!(last.push(message_row(9), &text).unwrap());
        assert!(!last.push(message_row(8), "older").unwrap());
        let last = last.finish().unwrap();
        combined.push_str(&last.messages[0].text);
        assert_eq!(combined, text);
        assert_eq!(last.next_before_id, Some(9));
        assert!(last.next_text_cursor.is_none());
        assert!(!last.messages[0].truncated);
        assert_encoded_budget(last);

        let mut missing = MessagePageBuilder::new(8, resume);
        assert!(missing.push(message_row(8), "older").is_err());
        assert!(MessagePageBuilder::new(8, resume).finish().is_err());
    }
}
