# Independent review — telegram-reader-mcp (frozen snapshot)

Snapshot: `/tmp/telegram-reader-review.CiU9I6Aa/source.tar`, SHA256
`977c3b7a…ec9` (verified). Reviewed offline; no Telegram credentials, no live
account access, implementation unmodified (the SSE comparison was built in a
throwaway copy, per the README's own documented procedure).

Validation performed:
- `cargo test --offline --locked` — 4/4 unit tests pass, clean compile, no warnings.
- `cargo build --release --offline --locked` — clean.
- `python3 tests/http_smoke.py <release>` — passes (two MCP clients, tool list,
  closed-scope errors, 401/403/413 paths, TTY guard, invalid-scope env).
- `--benchmark` on shipped binary — idle 0.00 CPU s / 6,364 KiB; 1,000 requests
  in 0.34 s wall, 0.12 CPU s, peak 6,468 KiB (README: 0.13 s / 6,416 KiB / 0.35 s).
- `--benchmark` on the SSE variant (json_response=false copy) — 0.22 CPU s,
  20.56 s wall, peak 6,360 KiB (README: 0.23 s / 6,340 KiB / 20.56 s).

## Verified against the review checklist

- **list_chats / read_messages correctness** — sound. Validation is strict
  (`deny_unknown_fields`, 1–40 / 1–20 bounds, query/name length caps,
  `before_id > 0`), see reader.rs:72–161. `chat_id` round-trips through
  `PeerId::from_bot_api_dialog_id`/`bot_api_dialog_id` (peer.rs:64–70;
  grammers-session peer.rs:186,245 — symmetric, range-validated). Pagination is
  correct: fetch limit+1, truncate, `next_before_id` = last id, and grammers
  `offset_id` is documented "starting message ID (exclusive)" for both
  iter_messages and search_messages (grammers-client messages.rs:284,383), so no
  duplicates/skips. `more` is set only when a further match exists
  (telegram.rs:162–165). Excerpts are char-safe (reader.rs:196–201).
- **Default-deny authorization** — correct. Unset/empty
  `TELEGRAM_ALLOWED_CHATS` → closed scope → server never connects to Telegram
  (http.rs:50–54) and tool calls return the CLOSED message before any access
  (reader.rs:16–17,219–222, 243–246, 262–266). Malformed scope → startup fails
  (reader.rs:37–50). Scope check precedes the Telegram call in read_messages
  (reader.rs:146–149); list_chats filters per-dialog (telegram.rs:141). Smoke
  test exercises this end-to-end.
- **Session/localhost multi-client** — IPv4 loopback only (http.rs:47); Host
  allowlist = bound address (http.rs:62–63; rmcp tower.rs:1100–1115
  host+port exact match); any `Origin` header → 403 before auth
  (http.rs:79–82); single `Authorization` header, constant-time compare
  (http.rs:96–109); 8-request in-flight cap → 503 (http.rs:90–92); token is
  256-bit, written 0600 via `persist_noclobber` race-safely, read via
  O_NOFOLLOW private-file checks (http.rs:111–139, state.rs:38–66). Each
  `initialize` gets an rmcp session sharing one `Arc<Telegram>` via `Reader`
  clones (reader.rs:203–217, http.rs:67–68). In this rmcp config
  (`legacy_session_mode=false`, no event store) only POST is allowed —
  GET/DELETE → 405 (rmcp tower.rs:1967–1978), so no standalone SSE stream can
  be held open to pin the in-flight cap.
- **Compact bounded results** — 12/40 chats, 8/20 messages, 600-char text,
  96-char titles (reader.rs:18–21); single JSON text block avoids
  structuredContent duplication (reader.rs:224–230); 8 KiB request cap → 413
  (http.rs:66, verified).
- **Failure handling** — 15 s deadline per Telegram op (telegram.rs:23,127–129,
  191–193); busy → tool error via `try_acquire` on the single-permit semaphore
  (telegram.rs:57,123–126,187–190); 401/420 mapped to actionable messages
  (telegram.rs:257–265); missing session file → clear "run login" error
  (state.rs:46–51); session flock prevents two readers (telegram.rs:39–42);
  login requires a real TTY (main.rs:31–33); bad scope env fails fast
  (verified by smoke); invalid token file → bail (http.rs:131–137).

## Findings (prioritized)

### P1 — Medium

1. **MCP sessions are killed after ~5 minutes of inactivity; a dead session
   then produces an internal error (not the spec's 404), clients have no
   session-close path, and stale session-map entries accumulate for the
   server's lifetime.**
   - Evidence: `StreamableHttpService::new(.., Default::default(), ..)` uses
     `LocalSessionManager` with `SessionConfig::default()` → `keep_alive =
     Some(300 s)` — worker exits `IdleTimeout` after 300 s without session
     events (rmcp-3.11.0 `session/local.rs` DEFAULT_KEEP_ALIVE ≈L1252,
     IdleTimeout ≈L1124). http.rs:67–68.
   - `with_legacy_session_mode(false)` + no event store ⇒ POST-only; DELETE is
     405 (rmcp tower.rs:1967–1978), so clients cannot close sessions and the
     only `sessions.remove` path (`close_session`, local.rs ≈L96) is
     unreachable. Dead entries persist.
   - Consequence: an agent idle >5 min between calls gets a failure rather
     than a session-not-found: `has_session` still returns true for the stale
     entry, then `create_stream`/`accept_message` fails with
     `SessionServiceTerminated` → `internal_error_response("get session")`
     (tower.rs:2236,2250) — some clients auto-re-initialize only on 404.
     Recovery requires re-`initialize`; whether target clients do this
     automatically is unverified offline.
   - Recommendation: verify re-initialize behavior of the intended clients;
     consider setting `LocalSessionManager.session_config.keep_alive`
     explicitly (longer or `None` plus a sweeper that removes terminated
     entries), and document the ~5-min session TTL. Slow unbounded map growth
     is bounded only by the initialize rate of authorized local clients — low
     practical impact but worth noting for a long-running server.

### P2 — Low

2. **Every tool call performs a full account dialog scan.** `list_chats`
   iterates all dialogs (telegram.rs:132–139) and `read_messages` resolves the
   peer via `dialog_ref`, which re-iterates the entire dialog list before the
   message fetch (telegram.rs:175–184). `auto_cache_peers: false`
   (telegram.rs:51) makes the scan the only way to obtain an access hash, but
   it costs O(dialogs) RPC pages inside the single 15 s deadline and occupies
   the single Telegram permit the whole time — on large accounts reads become
   uniformly slow and queued behind each other. (🧠 From Hindsight memory
   (Key decisions and rationale): the recorded decisions call for no read
   caches/polling, one concurrent Telegram operation, and a shared session —
   the scan-serialized design matches those constraints.) Recommend
   documenting the practical scale bound, or
   enabling peer caching so `PeerRef` resolution can skip the scan.
3. **Upstream panic paths inside `iter_dialogs`.** grammers' `Dialog::new`
   does `.expect("dialogs use an unknown peer")` and `peer_ref()` does
   `auth().unwrap()` (grammers-client peer/dialog.rs ≈L51,≈L78). Exotic
   dialog entries (e.g., Community dialogs or peers lacking access hashes)
   could panic inside the request task — tokio contains it to the connection
   task, so the server survives but the request dies abruptly. Cannot be
   exercised offline; flag for a dependency watch rather than a code change.
4. **Chat titles/usernames truncate silently.** `Chat` has no `truncated`
   indicator, unlike `Message` — `excerpt(name, TITLE_CHARS)` drops the flag
   (telegram.rs:168–169; reader.rs:196–201), so an agent cannot distinguish a
   >96-char title from a complete one.
5. **`localhost`-named clients cannot connect.** Bind is IPv4 loopback only
   (http.rs:47) and the Host allowlist is the literal `127.0.0.1:port`
   authority (http.rs:62–63; rmcp host+port exact match). A client pointed at
   `localhost` fails (403 via IPv4, or connection refused if `::1` wins).
   Consistent with the documented `http://127.0.0.1:8173/mcp` URL — note only
   for environments that default to `localhost`.
6. **Token file is brittle to hand edits.** `token()` requires exactly 64 hex
   chars (http.rs:131–137); `take(65)` means a trailing newline makes the file
   "invalid" and `serve` fails until the file is deleted — sharp but
   reasonable. Separately, `directory()` checks the leaf dir's owner/mode
   after creation (state.rs:28–35); if an operator points
   `TELEGRAM_READER_DIR` under a world-writable parent, a swap of the leaf dir
   between the check and the token write is theoretically possible. The
   default path under `$HOME` is safe — caveat applies to custom locations.
7. **`env::args()` panics on non-UTF-8 argv** (main.rs:26) — use `args_os` if
   that matters; trivial.
8. **The in-flight permit is released at response construction, not body
   completion** (http.rs:90–93) — accurate for the shipped JSON mode; in the
   README's SSE variant it would under-count long-lived streamed bodies. Only
   affects the unshipped comparison config.
9. **Non-401/420 Telegram errors are flattened to "could not complete"**
   (telegram.rs:257–265) — good for not leaking internals to clients, but a
   transport failure and a flood-wait look identical to the operator; consider
   logging the concrete error server-side.
10. **`serve` needs Telegram connectivity at startup for open scopes** —
    `authorized()` is a real RPC (`updates.getState`; telegram.rs:60–71) with
    the 15 s deadline. Correct fail-fast behavior; just note an open-scope
    server cannot start while offline.

### Notes confirmed as non-issues

- `chats` operator CLI intentionally bypasses the configured scope
  (`Scope::All`, main.rs:43) — documented discovery tool (README:50–51); the
  trust boundary is the state dir, not the env var.
- Saved Messages is reachable: `Dialog::peer_id()` returns the real user peer
  id (the `self_user` sentinel is only synthesized in `message.sender()`), so
  `bot_api_dialog_id()` is `Some` and it isn't filtered out.
- rmcp's own Origin check is a no-op with an empty allowlist
  (tower.rs:1360–1363); the middleware's blanket 403 (http.rs:80–82) is the
  actual enforcement — correctly layered.
- Media/service-only messages serialize as `text:""` with no media marker —
  disclosed in README ("Media content is outside the reader's text tools").
- `authorize` rejects multiple `Authorization` headers and requires exactly
  one; `Bearer` scheme match is case-insensitive with fixed-length token —
  correct.

## CPU/RAM evidence — representativeness

The published numbers are **reproducible and methodologically sound**: process
-wide utime+stime via `/proc`, VmHWM for peak RSS, persistent connections,
post-init sampling, a clean single-variable SSE A/B, and honest labeling
("closed chat scope", "Live Telegram CPU and RAM require a signed-in
account"). I reproduced both rows almost exactly on the same-class VM
(0.12/0.22 CPU s; 0.34 s vs 20.56 s wall). 🧠 From Hindsight memory (Key
decisions and rationale): the recorded validation requirement was "two clients
offline" plus release CPU/RSS compared against a plausible higher-overhead
transport — the shipped test and SSE baseline meet exactly that bar; the gaps
below are about extrapolating further, not about a missed requirement.

Limits of that evidence:
- **It measures the closed-scope process only** — no `Telegram` object exists,
  so no SqliteSession, sender-pool runner, MTProto keepalive, or update
  handling is exercised. Idle "0.00 CPU s" is the HTTP/MCP layer's floor, not
  a signed-in server's idle cost; connected idle will be nonzero and RSS
  higher. The README discloses this — just don't extrapolate the headline
  numbers to the connected case.
- **The workload exercises only the closed-scope error path** of
  `read_messages` — real reads add Telegram round-trips, the O(dialogs) scan,
  up-to-~25 KiB payload serialization, and semaphore contention; none of that
  CPU/RSS is covered.
- **Concurrency evidence is exactly 2 clients** with sequential requests each;
  the 8-permit cap, session churn/expiry (>300 s), and initialize bursts are
  never exercised.
- The SSE run's wall time reproduces deterministically (20.56 s both times) —
  ~41 ms/request of SSE-stream overhead in that config; fine as evidence that
  JSON responses are cheaper, but the mechanism lives in rmcp internals and
  wasn't root-caused here.
- Single-run numbers, single machine, no variance estimate.

Suggested addition: one signed-in idle sample (if credentialed) and a sentence
that "idle" denotes the no-Telegram configuration.

## Coverage limits of this review

- No live Telegram anywhere: success paths of `list_chats`/`read_messages`,
  dialog scanning, search, pagination, access-hash resolution, reconnect and
  flood-wait behavior are unexercised end-to-end — reviewed statically against
  vendored grammers 0.12 / rmcp 3.11 sources.
- The 300 s session expiry and stale-entry accumulation are verified from
  rmcp source, not runtime-tested.
- Multi-client verified to 2 clients (per the smoke test); higher concurrency
  only analyzed statically.
- The unit tests and smoke suite are thin on adversarial inputs (e.g.,
  oversized/UTF-8-boundary `name` filters, rapid initialize churn); behavior
  there was reasoned from code, not fuzzed.
