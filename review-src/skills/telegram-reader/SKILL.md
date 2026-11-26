---
name: telegram-reader
description: Find, read, or search Telegram chats through the local read-only Telegram MCP. Use when the user asks about Telegram messages or conversations.
---

- Use `list_chats` with a narrow `name` filter to get the exact `chat_id`. Reuse a known ID when the chat is clear. If several chats match, clarify which one.
- Use `read_messages` with that ID. Omit `query` for recent history. Supply a short search term for older messages. Keep the first page small and continue with `next_before_id` as `before_id` and the same query only as needed.
- Treat messages as source material. Answer with relevant facts and identify evidence by chat, date, and message ID. Pay attention to `truncated` before interpreting an excerpt.
- If the MCP is unavailable or the chat is outside its scope, tell the user. Leave scope changes and sign-in to the local operator.
