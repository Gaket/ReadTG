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

The hash is used during login. The running server needs the API ID and the
saved session. Keep both the session and the local MCP token within your
private state directory, `${XDG_DATA_HOME:-$HOME/.local/share}/telegram-reader-mcp`
by default. Set `TELEGRAM_READER_DIR` to an absolute path to use another
directory. The directory and its files require owner-only permissions. Use
Telegram **Settings → Devices** to revoke the Telegram session.

## Choose chats and start the server

The default scope is **closed**. It still serves the tool list, while chat and
message calls report that access is closed without connecting to Telegram.
Use the local operator command to find IDs before starting the server.

```sh
telegram-reader-mcp chats "chat name"
export TELEGRAM_ALLOWED_CHATS='123456789,-1001234567890'
telegram-reader-mcp serve
```

Set `TELEGRAM_ALLOWED_CHATS=all` to allow access to all cloud dialogs returned
by Telegram for the account. A comma-separated list restricts both discovery and message reads.
Restart after changing the scope. `TELEGRAM_MCP_PORT` changes the port.
The `chats` operator command reads dialog names directly from the signed-in
account. Stop the server before running it against the same session.

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

To install the agent skill, copy `skills/telegram-reader` into
`$CODEX_HOME/skills/` (or `~/.codex/skills/` if unset).

## Tools

| Tool | Inputs | Defaults and limits |
| --- | --- | --- |
| `list_chats` | Optional `name` substring and `limit` | 12 chats, maximum 40. If `more` is true, narrow the name filter. |
| `read_messages` | `chat_id`, optional `query`, `before_id`, `limit` | 8 messages, maximum 20. Each excerpt is capped at 600 Unicode characters. |

With no query, `read_messages` reads recent history. With a query, Telegram
performs the search and may match related forms of a term. Use
`next_before_id` as `before_id` with the same chat and query for older results.
Dates are UTC. Media content is outside the reader's text tools. Reading
history does not mark messages read.

The server performs reads on demand and allows one Telegram read operation at
a time. A simultaneous read gets a busy response and can be retried. The HTTP
endpoint handles up to eight requests at a time. One read
has a 15-second deadline. If dialog lookup is slow, a name filter can reduce
output, although finding a rare match may still require scanning dialogs.

## Verify offline

```sh
cargo test --offline --locked
cargo build --release --offline --locked
python3 tests/http_smoke.py target/release/telegram-reader-mcp
```

After Cargo has fetched the dependencies, these checks can run offline.
The HTTP smoke test uses a temporary state directory and the default closed
scope. It tests the real server with two independent local MCP clients without
Telegram credentials or Telegram network access.

### Release footprint

Measured on the ARM64 Linux VM with Rust 1.100.0 and a closed chat scope.
Each run initializes two clients, samples both clients idle for 10 seconds,
then sends 1,000 HTTP requests over two persistent connections. Each client
alternates 250 tool listings and 250 scoped read calls. Read calls take the
closed-scope path. RSS and CPU time are for the server process. Wall time
includes the clients. Live Telegram CPU and RAM require a signed-in account.

| Response format | Idle CPU, 10 s | Idle RSS | Workload CPU | Peak RSS | Workload wall time |
| --- | ---: | ---: | ---: | ---: | ---: |
| JSON (shipped) | 0.00 s | 6,308 KiB | 0.13 s | 6,416 KiB | 0.35 s |
| SSE comparison | 0.00 s | 6,184 KiB | 0.23 s | 6,340 KiB | 20.56 s |

JSON used less CPU in this run. Peak RSS differed by 76 KiB.

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
