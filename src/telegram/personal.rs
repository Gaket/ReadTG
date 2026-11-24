use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use grammers_client::{
    peer::{Peer, User},
    tl,
};
use grammers_session::types::{PeerId, PeerKind, PeerRef};
use tokio::time::timeout;

use super::{
    MESSAGE_ENVELOPE_BYTES, MESSAGE_RESULT_BYTES, READ_TIMEOUT, TRUNCATED_BYTES, Telegram,
    encoded_row_bytes, encoded_text_prefix, full_name, plain_text, telegram_error,
};
use crate::reader::{Scope, TITLE_CHARS, excerpt};

const GLOBAL_ROWS: i32 = 60;
const TARGETED_CHATS: usize = 8;
const DIALOGS_TO_SCAN: usize = 1_000;
const TELEGRAM_NOTIFICATIONS_ID: i64 = 777000;

#[derive(serde::Serialize)]
pub struct PersonalPage {
    messages: Vec<PersonalMessage>,
    partial: bool,
}

#[derive(serde::Serialize)]
struct PersonalMessage {
    chat_id: String,
    chat_name: String,
    id: i32,
    date: String,
    text: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
}

impl Telegram {
    pub async fn search_personal(
        &self,
        scope: &Scope,
        query: String,
        limit: usize,
    ) -> Result<PersonalPage> {
        let _permit = self
            .requests
            .try_acquire()
            .context("Telegram reader is busy. Retry shortly.")?;
        timeout(READ_TIMEOUT, self.fetch_personal(scope, query, limit))
            .await
            .context("Telegram personal search timed out.")?
    }

    async fn fetch_personal(
        &self,
        scope: &Scope,
        query: String,
        limit: usize,
    ) -> Result<PersonalPage> {
        let self_id = self.client.get_me().await.map_err(telegram_error)?.id();
        let mut page = PersonalPage {
            messages: Vec::new(),
            partial: false,
        };
        match scope {
            Scope::All => {
                let response = self
                    .client
                    .invoke(&global_request(query))
                    .await
                    .map_err(telegram_error)?;
                let (rows, partial) = search_rows(response, GLOBAL_ROWS as usize)?;
                let candidates = rows.iter().filter_map(personal_message_peer).collect();
                let (dialogs, more) = self
                    .personal_dialogs(candidates, self_id, GLOBAL_ROWS as usize)
                    .await?;
                page.partial = partial || more;
                page.add_matches(rows, scope, &dialogs);
            }
            Scope::Only(ids) => {
                let candidates = ids
                    .iter()
                    .copied()
                    .filter(|id| id.kind() == PeerKind::User)
                    .collect();
                let (dialogs, more) = self
                    .personal_dialogs(candidates, self_id, TARGETED_CHATS)
                    .await?;
                page.partial = more;
                for (&id, user) in &dialogs {
                    let Some(peer) = user
                        .to_ref()
                        .await
                        .map_err(|_| anyhow!("Could not resolve a Telegram personal chat."))?
                    else {
                        page.partial = true;
                        continue;
                    };
                    let response = self
                        .client
                        .invoke(&targeted_request(peer, &query, limit + 1))
                        .await
                        .map_err(telegram_error)?;
                    let (mut rows, partial) = search_rows(response, limit + 1)?;
                    page.partial |= partial;
                    rows.retain(|row| personal_message_peer(row) == Some(id));
                    page.add_matches(rows, scope, &dialogs);
                    page.keep_newest(limit);
                }
            }
        }
        page.keep_newest(limit);
        page.bound_bytes()?;
        Ok(page)
    }

    async fn personal_dialogs(
        &self,
        mut candidates: HashSet<PeerId>,
        self_id: PeerId,
        limit: usize,
    ) -> Result<(HashMap<PeerId, User>, bool)> {
        let mut found = HashMap::new();
        let mut partial = false;
        candidates.remove(&self_id);
        candidates.remove(&PeerId::user_unchecked(TELEGRAM_NOTIFICATIONS_ID));
        let mut dialogs = self.client.iter_dialogs();
        for _ in 0..DIALOGS_TO_SCAN {
            if candidates.is_empty() {
                break;
            }
            let Some(dialog) = dialogs.next().await.map_err(telegram_error)? else {
                return Ok((found, partial));
            };
            let id = dialog.peer_id();
            if !candidates.remove(&id) {
                continue;
            }
            let Peer::User(user) = dialog.peer() else {
                continue;
            };
            match is_human(user) {
                Some(true) => {}
                Some(false) => continue,
                None => {
                    partial = true;
                    continue;
                }
            }
            if found.len() == limit {
                return Ok((found, true));
            }
            found.insert(id, user.clone());
        }
        Ok((found, partial || !candidates.is_empty()))
    }
}

fn is_human(user: &User) -> Option<bool> {
    match &user.raw {
        tl::enums::User::User(raw) if !raw.min => Some(!raw.bot && !raw.is_self && !raw.deleted),
        _ => None,
    }
}

fn personal_message_peer(row: &tl::enums::Message) -> Option<PeerId> {
    let tl::enums::Message::Message(message) = row else {
        return None;
    };
    let tl::enums::Peer::User(peer) = &message.peer_id else {
        return None;
    };
    PeerId::user(peer.user_id)
}

impl PersonalPage {
    fn add_matches(
        &mut self,
        rows: Vec<tl::enums::Message>,
        scope: &Scope,
        dialogs: &HashMap<PeerId, User>,
    ) {
        for row in rows {
            let Some(id) = personal_message_peer(&row).filter(|id| scope.allows(*id)) else {
                continue;
            };
            let Some(user) = dialogs.get(&id) else {
                continue;
            };
            let tl::enums::Message::Message(message) = row else {
                continue;
            };
            let name = full_name(user.first_name(), user.last_name());
            let name = if name.is_empty() {
                user.username().unwrap_or("(unnamed)")
            } else {
                &name
            };
            let text = if message.message.is_empty() {
                plain_text("", message.rich_message.is_some()).to_owned()
            } else {
                message.message
            };
            self.messages.push(PersonalMessage {
                chat_id: id.bot_api_dialog_id_unchecked().to_string(),
                chat_name: excerpt(name, TITLE_CHARS).0.to_owned(),
                id: message.id,
                date: jiff::Timestamp::from_second(message.date.into())
                    .expect("i32 Unix timestamp fits in Jiff")
                    .to_string(),
                text,
                truncated: false,
            });
        }
    }

    fn keep_newest(&mut self, limit: usize) {
        self.messages
            .sort_by(|a, b| (&b.date, b.id, &b.chat_id).cmp(&(&a.date, a.id, &a.chat_id)));
        self.partial |= self.messages.len() > limit;
        self.messages.truncate(limit);
    }

    fn bound_bytes(&mut self) -> Result<()> {
        let mut remaining = MESSAGE_RESULT_BYTES - MESSAGE_ENVELOPE_BYTES;
        for (index, row) in self.messages.iter_mut().enumerate() {
            let mut text = std::mem::take(&mut row.text);
            let row_bytes = encoded_row_bytes(row)?;
            let (end, text_bytes) = encoded_text_prefix(&text, remaining.saturating_sub(row_bytes));
            if end == text.len() && row_bytes + text_bytes <= remaining {
                row.text = text;
                remaining -= row_bytes + text_bytes;
                continue;
            }

            let mut keep = index;
            if index == 0 {
                let (end, _) = encoded_text_prefix(
                    &text,
                    remaining.saturating_sub(row_bytes + TRUNCATED_BYTES),
                );
                anyhow::ensure!(end > 0, "Message metadata exceeds the response budget.");
                text.truncate(end);
                row.text = text;
                row.truncated = true;
                keep = 1;
            }
            self.messages.truncate(keep);
            self.partial = true;
            break;
        }
        Ok(())
    }
}

fn search_rows(
    response: tl::enums::messages::Messages,
    limit: usize,
) -> Result<(Vec<tl::enums::Message>, bool)> {
    use tl::enums::messages::Messages;
    let (mut rows, incomplete) = match response {
        Messages::Messages(result) => (result.messages, false),
        Messages::Slice(result) => (result.messages, true),
        _ => bail!("Telegram returned an unexpected personal-search response."),
    };
    let partial = incomplete || rows.len() >= limit;
    rows.truncate(limit);
    Ok((rows, partial))
}

fn global_request(query: String) -> tl::functions::messages::SearchGlobal {
    tl::functions::messages::SearchGlobal {
        broadcasts_only: false,
        groups_only: false,
        users_only: true,
        folder_id: None,
        community: None,
        q: query,
        filter: tl::enums::MessagesFilter::InputMessagesFilterEmpty,
        min_date: 0,
        max_date: 0,
        offset_rate: 0,
        offset_peer: tl::enums::InputPeer::Empty,
        offset_id: 0,
        limit: GLOBAL_ROWS,
    }
}

fn targeted_request(peer: PeerRef, query: &str, limit: usize) -> tl::functions::messages::Search {
    tl::functions::messages::Search {
        peer: peer.into(),
        q: query.to_owned(),
        from_id: None,
        saved_peer_id: None,
        saved_reaction: None,
        top_msg_id: None,
        filter: tl::enums::MessagesFilter::InputMessagesFilterEmpty,
        min_date: 0,
        max_date: 0,
        offset_id: 0,
        add_offset: 0,
        limit: limit as i32,
        max_id: 0,
        min_id: 0,
        hash: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::reply;
    use grammers_client::{
        Client,
        message::{InputMessage, Message as TelegramMessage},
    };
    use grammers_mtsender::SenderPool;
    use grammers_session::storages::MemorySession;
    use std::sync::Arc;
    use tl::{Deserializable, Serializable};

    fn user(client: &Client, id: i64, name: &str, flags: u32) -> User {
        const FIRST_NAME: u32 = 1 << 1;
        const BOT: u32 = 1 << 14;
        let mut body = Vec::new();
        let mut write = |bytes: &[u8]| body.extend_from_slice(bytes);
        (flags | FIRST_NAME).serialize(&mut write);
        0u32.serialize(&mut write); // Secondary user flags.
        id.serialize(&mut write);
        name.to_owned().serialize(&mut write);
        if flags & BOT != 0 {
            1i32.serialize(&mut write); // Bot info version.
        }
        User::from_raw(client, tl::types::User::from_bytes(&body).unwrap().into())
    }

    fn row(client: &Client, peer: PeerId, text: String) -> tl::enums::Message {
        TelegramMessage::from_raw_short_updates(
            client,
            tl::types::UpdateShortSentMessage {
                out: true,
                id: 42,
                pts: 0,
                pts_count: 0,
                date: 1_700_000_000,
                media: None,
                entities: None,
                ttl_period: None,
            },
            InputMessage::new().text(text),
            peer.to_ambient_ref(),
        )
        .raw
    }

    #[test]
    fn mixed_rows_expose_only_allowed_verified_personal_dialogs() {
        let pool = SenderPool::new(Arc::new(MemorySession::default()), 1);
        let client = Client::new(pool.handle);
        let allowed = user(&client, 10, "Allowed", 0);
        let disallowed = user(&client, 11, "Private contact", 0);
        let bot = user(&client, 12, "Bot", 1 << 14);
        let saved = user(&client, 13, "Saved Messages", 1 << 10);
        let min = user(&client, 14, "Unclassified", 1 << 20);
        let plain = "猫".repeat(4096);
        let rows = vec![
            row(&client, allowed.id(), plain.clone()),
            row(&client, disallowed.id(), "disallowed".into()),
            row(&client, bot.id(), "bot".into()),
            row(&client, saved.id(), "saved".into()),
            row(&client, min.id(), "unclassified".into()),
            row(&client, PeerId::chat(10).unwrap(), "group".into()),
            row(&client, PeerId::channel(10).unwrap(), "channel".into()),
            row(&client, PeerId::user(15).unwrap(), "out of dialogs".into()),
        ];
        assert_eq!(is_human(&bot), Some(false));
        assert_eq!(is_human(&saved), Some(false));
        assert_eq!(is_human(&min), None);
        let scope = Scope::Only(HashSet::from([
            allowed.id(),
            bot.id(),
            saved.id(),
            min.id(),
        ]));
        let dialogs = [allowed, disallowed, bot, saved, min]
            .into_iter()
            .filter(|user| is_human(user) == Some(true))
            .map(|user| (user.id(), user))
            .collect();
        let mut page = PersonalPage {
            messages: vec![],
            partial: false,
        };
        page.add_matches(rows, &scope, &dialogs);
        page.bound_bytes().unwrap();
        assert_eq!(page.messages.len(), 1);
        let hit = &page.messages[0];
        assert_eq!(
            (hit.chat_id.as_str(), hit.chat_name.as_str(), hit.id),
            ("10", "Allowed", 42)
        );
        assert_eq!(hit.date, "2023-11-14T22:13:20Z");
        assert_eq!(hit.text, plain);
        assert!(!hit.truncated);
        assert!(!page.partial);
    }

    #[test]
    fn personal_search_requests_have_fixed_window_and_no_global_offset() {
        let request = global_request("invoice".into());
        assert!(request.users_only);
        assert!(!request.groups_only && !request.broadcasts_only);
        assert_eq!(request.limit, GLOBAL_ROWS);
        assert_eq!((request.offset_rate, request.offset_id), (0, 0));
        assert!(matches!(request.offset_peer, tl::enums::InputPeer::Empty));
        let peer = PeerId::user(10).unwrap().to_ambient_ref();
        let request = targeted_request(peer, "invoice", 51);
        assert!(matches!(request.peer, tl::enums::InputPeer::User(user) if user.user_id == 10));
        assert_eq!(request.limit, 51);
        let response = tl::types::messages::Messages {
            messages: Vec::new(),
            topics: Vec::new(),
            chats: Vec::new(),
            users: Vec::new(),
        };
        assert!(!search_rows(response.into(), 9).unwrap().1);
    }

    fn hit(id: i32, text: String) -> PersonalMessage {
        PersonalMessage {
            chat_id: "10".into(),
            chat_name: "\0".repeat(TITLE_CHARS),
            id,
            date: "2023-11-14T22:13:20Z".into(),
            text,
            truncated: false,
        }
    }

    fn encoded_size(page: PersonalPage) -> usize {
        serde_json::to_vec(&reply(Ok(page))).unwrap().len()
    }

    #[test]
    fn personal_search_keeps_fifty_newest_matches_across_batches() {
        let mut page = PersonalPage {
            messages: (1..=51).map(|id| hit(id, "first".into())).collect(),
            partial: false,
        };
        page.keep_newest(50);
        page.messages
            .extend((52..=60).map(|id| hit(id, "second".into())));
        page.keep_newest(50);
        page.bound_bytes().unwrap();
        assert_eq!(page.messages.len(), 50);
        assert_eq!(page.messages.first().unwrap().id, 60);
        assert_eq!(page.messages.last().unwrap().id, 11);
        assert!(page.partial);
        assert!(encoded_size(page) <= MESSAGE_RESULT_BYTES);
    }

    #[test]
    fn personal_search_bounds_encoded_text_and_marks_incomplete_matches() {
        let text = "猫🌊\"\\\n\r\t\u{8}\u{c}\0\u{1f}".repeat(750);
        let mut page = PersonalPage {
            messages: vec![
                hit(3, text.clone()),
                hit(2, text.clone()),
                hit(1, "older".into()),
            ],
            partial: false,
        };
        page.bound_bytes().unwrap();
        assert!(page.partial);
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].text, text);
        assert!(!page.messages[0].truncated);
        assert!(encoded_size(page) <= MESSAGE_RESULT_BYTES);

        let oversized = text.repeat(3);
        let mut page = PersonalPage {
            messages: vec![hit(2, oversized.clone()), hit(1, "older".into())],
            partial: false,
        };
        page.bound_bytes().unwrap();
        assert!(page.partial);
        assert_eq!(page.messages.len(), 1);
        let first = &page.messages[0];
        assert!(first.truncated && !first.text.is_empty());
        assert!(oversized.starts_with(&first.text));
        let size = encoded_size(page);
        assert!((MESSAGE_RESULT_BYTES - 512..=MESSAGE_RESULT_BYTES).contains(&size));
    }

    #[test]
    fn slices_and_overflow_report_partial_without_exposing_server_totals() {
        let empty: tl::enums::Message = tl::types::MessageEmpty {
            id: 1,
            peer_id: None,
        }
        .into();
        let response = tl::types::messages::MessagesSlice {
            inexact: true,
            count: 100,
            next_rate: Some(7),
            offset_id_offset: None,
            search_flood: None,
            messages: vec![empty; 3],
            topics: vec![],
            chats: vec![],
            users: vec![],
        };
        let (rows, partial) = search_rows(response.clone().into(), 2).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(partial);
        assert!(search_rows(response.into(), 10).unwrap().1);
    }
}
