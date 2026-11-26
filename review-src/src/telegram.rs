use std::{env, fs::File, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use grammers_client::{
    Client, SignInError, client::ClientConfiguration, message::Message as TelegramMessage,
};
use grammers_mtsender::{InvocationError, SenderPool};
use grammers_session::{
    storages::SqliteSession,
    types::{PeerId, PeerRef},
};
use tokio::{sync::Semaphore, time::timeout};
use zeroize::Zeroizing;

use crate::{
    reader::{
        Chat, ChatPage, ChatParams, MESSAGE_TEXT_CHARS, Message, MessagePage, MessageParams, Scope,
        TITLE_CHARS, excerpt,
    },
    state,
};

const READ_TIMEOUT: Duration = Duration::from_secs(15);

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
        let path = state::directory()?.join("account.session");
        let session_lock = state::private_file(&path, create)?;
        session_lock.try_lock().map_err(|_| {
            anyhow!("The Telegram session is in use. Stop the other reader before opening it.")
        })?;

        let session = SqliteSession::open(path)
            .await
            .map_err(|_| anyhow!("Could not open the Telegram session database."))?;
        let SenderPool { handle, runner, .. } = SenderPool::new(Arc::new(session), api_id);
        let client = Client::with_configuration(
            handle,
            ClientConfiguration {
                auto_cache_peers: create,
            },
        );
        tokio::spawn(runner.run());
        let telegram = Self {
            client,
            requests: Semaphore::new(1),
            _session_lock: session_lock,
        };
        if !create && !telegram.authorized().await? {
            bail!("The Telegram session requires login. Stop the server and run login locally.");
        }
        Ok(telegram)
    }

    async fn authorized(&self) -> Result<bool> {
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
        let mut dialogs = self.client.iter_dialogs();
        let mut page = ChatPage {
            chats: Vec::with_capacity(params.limit),
            more: false,
        };

        while let Some(dialog) = dialogs.next().await.map_err(telegram_error)? {
            let id = dialog.peer_id();
            if !scope.allows(id) {
                continue;
            }
            let Some(id) = id.bot_api_dialog_id() else {
                continue;
            };
            let peer = dialog.peer();
            let name = peer
                .name()
                .filter(|name| !name.is_empty())
                .unwrap_or("(unnamed)");
            let username = peer.username();

            if let Some(filter) = &params.name
                && !name.to_lowercase().contains(filter)
                && !username.is_some_and(|name| {
                    name.to_lowercase().contains(filter.trim_start_matches('@'))
                })
            {
                continue;
            }
            if page.chats.len() == params.limit {
                page.more = true;
                break;
            }
            page.chats.push(Chat {
                chat_id: id.to_string(),
                name: excerpt(name, TITLE_CHARS).0.to_owned(),
                username: username.map(|value| excerpt(value, TITLE_CHARS).0.to_owned()),
            });
        }
        Ok(page)
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
        let mut messages = Vec::with_capacity(limit);
        if let Some(query) = &params.query {
            let mut iter = self
                .client
                .search_messages(peer)
                .query(query)
                .offset_id(params.before_id)
                .limit(limit);
            while let Some(message) = iter.next().await.map_err(telegram_error)? {
                messages.push(project(&message));
            }
        } else {
            let mut iter = self
                .client
                .iter_messages(peer)
                .offset_id(params.before_id)
                .limit(limit);
            while let Some(message) = iter.next().await.map_err(telegram_error)? {
                messages.push(project(&message));
            }
        }
        Ok(message_page(messages, params.limit))
    }
}

fn message_page(mut messages: Vec<Message>, limit: usize) -> MessagePage {
    let next_before_id = if messages.len() > limit {
        messages.truncate(limit);
        messages.last().map(|message| message.id)
    } else {
        None
    };
    MessagePage {
        messages,
        next_before_id,
    }
}

fn project(message: &TelegramMessage) -> Message {
    let (text, truncated) = excerpt(message.text(), MESSAGE_TEXT_CHARS);
    Message {
        id: message.id(),
        date: message.date().to_string(),
        sender: message
            .sender()
            .and_then(|peer| peer.name())
            .map(|name| excerpt(name, TITLE_CHARS).0.to_owned()),
        text: text.to_owned(),
        truncated,
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
        InvocationError::Rpc(rpc) if rpc.code == 420 => {
            anyhow!("Telegram rate-limited the request. Retry later.")
        }
        _ => anyhow!("Telegram could not complete the request."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_points_to_last_returned_message_only_when_more_exist() {
        let message = |id| Message {
            id,
            date: String::new(),
            sender: None,
            text: String::new(),
            truncated: false,
        };
        let page = message_page([7, 5, 3].into_iter().map(message).collect(), 2);
        assert_eq!(
            page.messages
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            [7, 5]
        );
        assert_eq!(page.next_before_id, Some(5));
        assert_eq!(message_page(vec![message(3)], 2).next_before_id, None);
        assert_eq!(message_page(Vec::new(), 2).next_before_id, None);
    }
}
