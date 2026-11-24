# Telegram reader MCP

Two read-only tools for a Telegram user account, served over Streamable HTTP at
`http://127.0.0.1:8173/mcp`. Authorized clients on the same VM share one
Telegram connection pool and account session. The implementation uses
grammers for MTProto and the official MCP SDK for Rust.
[grammers](https://codeberg.org/Lonami/grammers) · [Rust MCP SDK](https://rust.sdk.modelcontextprotocol.io/)

## Install and sign in

1. Build on Linux or macOS with current stable Rust and a C compiler.

   ```sh
   cargo install --locked --path .
   ```

2. Create an application under **API development tools** at
   [my.telegram.org](https://my.telegram.org). Telegram provides an API ID and
   an API hash. See the [Telegram application guide](https://core.telegram.org/api/obtaining_api_id).
3. Run in your own terminal with the API ID set. Enter the app hash, phone,
   verification code, and two-step password if prompted. Input is hidden.

   ```sh
   export TELEGRAM_API_ID=your_numeric_app_id
   telegram-reader-mcp login
   ```

The hash is used during login. Chat access requires the API ID and a verified
saved session. Keep both the session and the local MCP token within your
private state directory, `${XDG_DATA_HOME:-$HOME/.local/share}/telegram-reader-mcp`
by default. Set `TELEGRAM_READER_DIR` to an absolute path to use another
directory. The directory and its files require owner-only permissions. Use
Telegram **Settings → Devices** to revoke the Telegram session.

## Choose chats and start the server

With `TELEGRAM_ALLOWED_CHATS` unset, startup verifies the saved session with
Telegram and opens all cloud dialogs available to the signed-in account.
Before sign-in, the MCP can serve its tool list with chat access closed. An
automatic startup with no API ID, no saved session, or a session that Telegram
reports as unauthorized stays closed. Connection or session errors surface at
startup.

Set `TELEGRAM_ALLOWED_CHATS` **before the first server start** if a bearer-token
holder should have restricted access. An explicit setting takes precedence.

| Value | Chat access |
| --- | --- |
| Unset | All available cloud dialogs after verified sign-in |
| `closed` or an empty value | Closed |
| `all` | All available cloud dialogs, with a verified sign-in required at startup |
| Comma-separated chat IDs | Only those dialogs, with a verified sign-in required at startup |

After a successful login, keep the API ID exported in the server's environment.
For the automatic default, run `telegram-reader-mcp serve` with
`TELEGRAM_ALLOWED_CHATS` unset. For an allowlist, use the operator command to
find IDs before starting the server.

```sh
telegram-reader-mcp chats
export TELEGRAM_ALLOWED_CHATS='123456789,-1001234567890'
telegram-reader-mcp serve
```

Restart after changing the scope. `TELEGRAM_MCP_PORT` changes the port.
The `chats` operator command reads dialog names directly from the signed-in
account. Stop the server before running it against the same session.
Pass an actual chat name as an optional argument to filter the first dialog page.
If Telegram reports a retry delay, wait the full interval before retrying.

## Connect local agents

The server creates `mcp-token` in the state directory. Run the following from
each authorized agent's launch environment. Keep that environment private
and restart the agent after changing it.

```sh
export TELEGRAM_MCP_TOKEN="$(cat "${TELEGRAM_READER_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/telegram-reader-mcp}/mcp-token")"
```

For Codex, add this to your Codex config and start Codex from that environment.
For another Streamable HTTP client, use the same URL and a Bearer authorization header.

```toml
[mcp_servers.telegram]
url = "http://127.0.0.1:8173/mcp"
bearer_token_env_var = "TELEGRAM_MCP_TOKEN"
```

All clients using this token share the configured chat scope. The endpoint
binds to IPv4 loopback and rejects browser Origin headers. Use it from the
same VM. Authorized clients receive message text and may send it to their model.
To rotate MCP access, stop the server, remove the token file, and restart.

The MCP endpoint serves JSON per request without retaining MCP sessions.
Clients can use the older initialize handshake or modern discovery. An idle
client can reconnect and make its next authenticated call on a new connection.

To install the agent skill, copy `skills/telegram-reader` into
`$CODEX_HOME/skills/` (or `~/.codex/skills/` if unset).

## Tools

| Tool | Inputs | Defaults and limits |
| --- | --- | --- |
| `list_chats` | Optional `name` substring and `limit` | 12 chats, maximum 40. Search a person's full name or surname, a chat title, or a username. If `more` is true, narrow the name filter. |
| `read_messages` | `chat_id` with optional `query`, `before_id`, or `text_cursor`. For personal search, use `chat_type: "personal"` with a required `query`. Both modes accept `limit`. | 8 messages by default. Both modes allow up to 50 messages within a 48 KiB encoded result. |

Filtered or allowlisted discovery checks at most the first 100 dialogs in one
Telegram page. `partial: true` means the scan reached its dialog limit and
additional matches may exist, even when the result is empty. `more: true`
means the reader found another allowed match beyond the requested result
limit. With full access and no name filter, discovery asks Telegram for just
the requested limit plus one lookahead.

With no query, `read_messages` reads recent history. With a query, Telegram
performs the search and may match related forms of a term. Use
`next_before_id` as `before_id` with the same chat and query for older results.
Single-chat results return Telegram's full plain text or caption, with a
48 KiB cap on the JSON-encoded MCP tool result including escaping and content
metadata. A page may contain fewer messages than requested. If one message
alone exceeds the budget, it returns `truncated: true` and `next_text_cursor`.
Pass that object back as `text_cursor` with the same chat and query, omitting
`before_id`. Append the next chunk for that message ID. If the source changes
and the cursor becomes invalid, restart the read. The page resumes older
results once the message is complete.

Dates are UTC. Plain text has no `kind`. A message with media can have
`kind: "photo"`, `"document"`, or `"media"` and an empty `text` when it has
no caption. Service events have `kind: "service"` and may include an `action`
such as `"join_by_link"`. Reading history leaves read status unchanged.

If you remember a keyword but have forgotten the contact, call `read_messages`
with the following arguments.

```json
{"chat_type":"personal","query":"invoice","limit":8}
```

Personal matches include `chat_id`, `chat_name`, message `id`, `date`, and
full plain text within the same encoded result budget. They come from current
one-to-one human cloud dialogs.
The mode excludes groups, channels, bots, Saved Messages, and Telegram's
notification account. [Telegram service notifications](https://core.telegram.org/constructor/updateServiceNotification)

- With all-dialog access, the reader asks Telegram for
  one private-search window of up to 60 rows and verifies matching peers against
  current dialogs. The grammers 0.12 iterator leaves `users_only` disabled,
  so the reader sets it on the raw request. [Telegram search API](https://core.telegram.org/method/messages.searchGlobal)
- With a chat ID allowlist, the reader searches at most eight verified, allowed
  human dialogs in Telegram's dialog order, with one bounded query per dialog.
  Both paths inspect at most 1,000 current dialog entries to verify membership.
- Personal results are sorted newest first within the inspected window.
  `partial: true` means the search window, count, or byte budget may have
  limited results. If the first match alone exceeds the byte budget, its text
  is returned with `truncated: true`. Refine the query or use the returned
  `chat_id` to continue in one chat. Omit `chat_id`, `before_id`, and
  `text_cursor` when searching across personal chats.
  The 15-second deadline covers the whole search. A timeout returns an error.

Block-only rich posts return `[Unsupported Telegram rich text]` in the
`text` field. This marker indicates that the post has no plain-text field.

The server performs reads on demand and allows one Telegram read operation at
a time. A simultaneous read gets a busy response and can be retried. The HTTP
endpoint handles up to eight requests at a time. One read
has a 15-second deadline. A Telegram rate-limit response returns its retry
delay when available and releases the read slot.

Each single-chat read resolves the chat against the account's current dialogs
before fetching messages. Resolving a late or missing ID can require scanning
the whole list.
The pinned Telegram SDK fetches up to 100 dialogs per RPC. For example, a chat
around position 451 can take five dialog RPCs before the message request, and
three history pages can repeat all 15 dialog RPCs within their respective read
deadlines. These are source-based estimates. Keeping a peer reference would
save some lookups. A cached reference can outlive removal from the account's
dialog list.

To verify the cost with a signed-in test account, call `read_messages` against
an early and a late dialog, then fetch three pages of history and three pages
of search in each. Record dialog RPC counts and request latency without
logging message bodies or access hashes. Remove a disposable dialog through
the Telegram app and check that its previous ID is refused by the reader.

## Verify offline

```sh
cargo test --offline --locked
cargo build --release --offline --locked
python3 tests/http_smoke.py target/release/telegram-reader-mcp
python3 tests/http_smoke.py target/release/telegram-reader-mcp --idle-seconds 305
```

After Cargo has fetched the dependencies, these checks can run offline.
The HTTP smoke test uses temporary state directories and checks that unset
scope before sign-in and explicit closed mode both serve with chat access
closed. It exercises two independent local MCP clients, 300 initialize
handshakes without a session ID, a client returning after idle, and modern
protocol requests without an initialize handshake. The 305-second run exceeds
the pinned SDK's legacy session TTL.

### Release footprint

Measured on the ARM64 Linux VM with Rust 1.100.0 and a closed chat scope.
Each run initializes two clients, samples both clients idle for 10 seconds,
then sends 1,000 HTTP requests over two persistent connections. Each client
alternates 250 tool listings and 250 scoped read calls. Read calls take the
closed-scope path. RSS and CPU time are for the server process. Wall time
includes the clients. Live Telegram CPU and RAM require a signed-in account.

| Response format | Idle CPU, 10 s | Idle RSS | Workload CPU | Peak RSS | Workload wall time |
| --- | ---: | ---: | ---: | ---: | ---: |
| Initial JSON build | 0.00 s | 6,308 KiB | 0.13 s | 6,416 KiB | 0.35 s |
| SSE comparison | 0.00 s | 6,184 KiB | 0.23 s | 6,340 KiB | 20.56 s |

JSON used less CPU in this run. Peak RSS differed by 76 KiB.

The same workload was rerun with the frozen JSON binary and the updated
reader after the FloodWait, full-name, and rich-text fixes. Closed-scope
requests exercise the MCP layer and leave the Telegram-specific paths to
the offline unit tests and live verification.

| JSON build | Idle CPU, 10 s | Idle RSS | Workload CPU | Peak RSS | Workload wall time |
| --- | ---: | ---: | ---: | ---: | ---: |
| Frozen baseline | 0.00 s | 6,316 KiB | 0.14 s | 6,428 KiB | 0.45 s |
| Updated reader | 0.00 s | 6,300 KiB | 0.11 s | 6,404 KiB | 0.24 s |

After selecting rmcp's session-disabled manager, two more runs used the same
closed-scope workload. The final build measured 5,636–5,640 KiB idle RSS
and 5,796–5,808 KiB peak RSS. Idle CPU was 0.00 s in both 10-second samples.
Each 1,000-request workload took 0.10–0.11 CPU s and 0.24–0.26 s wall time.

With personal search enabled in the tool schema, the same closed-scope
workload measured 5,856 KiB idle RSS, 5,952 KiB peak RSS, and 0.11 CPU s for
1,000 requests. The paired prior build measured 5,632 KiB idle RSS, 5,800 KiB
peak RSS, and 0.10 CPU s. Both 10-second idle samples showed 0.00 CPU s.
This measures MCP overhead. Signed-in personal search was not benchmarked.

```sh
cargo build --release --offline --locked
python3 tests/http_smoke.py target/release/telegram-reader-mcp --benchmark

# Build the stateless SSE comparison in a temporary source copy.
baseline="$(mktemp -d)"
cp Cargo.toml Cargo.lock "$baseline/"
cp -r src "$baseline/"
sed -i 's/with_json_response(true)/with_json_response(false)/' "$baseline/src/http.rs"
cargo build --release --offline --locked --manifest-path "$baseline/Cargo.toml" --target-dir "$baseline/target"
python3 tests/http_smoke.py "$baseline/target/release/telegram-reader-mcp" --benchmark
```
