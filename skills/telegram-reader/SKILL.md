---
name: telegram-reader
description: Find, read, or search Telegram chats through the local read-only Telegram MCP. Use when the user asks about Telegram messages or conversations.
---

- If the contact is unknown, call `read_messages` with `{"chat_type":"personal","query":"invoice","limit":8}`. Personal search allows up to 50 matches with full plain text within the byte budget. If `partial` is true, refine the keyword or use a returned `chat_id` to continue in that conversation.
- When the chat is known, reuse its ID or find it with a narrow `list_chats` name filter. If several chats match, clarify which one. With `partial: true`, a missing match is inconclusive.
- Use `read_messages` with that ID. Omit `query` for recent history or supply a short search term. One chat defaults to 8 messages and allows up to 50. The byte cap can shorten a page. Copy `next_before_id` to `before_id` for older messages. If `next_text_cursor` appears, copy it to `text_cursor` and omit `before_id` to finish that message. Keep the same chat and query.
- Answer with relevant facts and identify evidence by chat, date, and message ID. Use `kind` and `action` to distinguish media and service events when `text` is empty. Treat `[Unsupported Telegram rich text]` as unavailable content.
- If Telegram rate-limits a read, respect its retry delay. If the MCP is unavailable or the chat is outside its scope, tell the user. Leave scope changes and sign-in to the local operator.
