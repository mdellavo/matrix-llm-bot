# matrix-llm-bot

A Rust Matrix chat bot built on [`matrix-sdk`](https://github.com/matrix-org/matrix-rust-sdk), with TLS and E2E encryption support out of the box. It joins a configurable set of rooms and, for every message it receives, calls Claude (via [`anthropic-sdk-rust`](https://crates.io/crates/anthropic-sdk-rust)) to classify the message into a structured `MessageAnalysis` (see `src/classify.rs`) — intent, sentiment, entities, whether it needs a reply at all, and (for commands) a name and arguments. Commands dispatch to **skills**: prompts loaded from `skills/<name>/SKILL.md`, modeled on Claude Code's Skills, each run through Claude to produce a real generated reply (see "Commands (skills)" below). Every other intent gets a real Claude-generated reply too — informative first, with a light, mildly snarky sense of humor, grounded in the room's recent chat history rather than replying in a vacuum (see `generate_chat_reply`/`CHAT_SYSTEM_PROMPT` in `src/handler.rs`) — but only when the message actually addresses the bot — an `@mention` or a reply to one of its own messages (see `is_directed_at_bot`) — so it doesn't jump into ambient conversation between humans. Alongside the sync loop, a small [`axum`](https://github.com/tokio-rs/axum) HTTP server (`src/status_server.rs`) serves a status/debugging page and JSON API over the same data.

## Setup

1. Copy the example config and fill in your bot account's credentials:

   ```sh
   cp config.toml.example config.toml
   ```

2. Edit `config.toml`:
   - `homeserver_url` — your Matrix homeserver
   - `username` / `password` — the bot account's login credentials
   - `rooms` — room IDs (`!abc:example.org`) or aliases (`#room:example.org`) to join and monitor

3. Set `ANTHROPIC_API_KEY` in the environment (used by the message classifier in `src/classify.rs`):

   ```sh
   export ANTHROPIC_API_KEY="sk-ant-..."
   ```

   Optionally, set `omdb_api_key` in `config.toml` (a free key from
   <https://www.omdbapi.com/apikey.aspx>) to enable the `imdb` skill; without it,
   that skill just replies that it isn't configured.

4. Run it:

   ```sh
   cargo run
   ```

   By default it reads `config.toml` from the current directory; set `MATRIX_LLM_BOT_CONFIG` to point elsewhere. Set `RUST_LOG` (e.g. `RUST_LOG=debug`) to control log verbosity — this overrides the built-in default (`info`, with two chatty-but-harmless `matrix-sdk` internals quieted; see `DEFAULT_LOG_FILTER` in `src/main.rs`) entirely, so include `matrix_sdk_base`/`matrix_sdk_crypto` directives yourself if you set it and still want those quieted.

5. Open the status/debugging page at <http://127.0.0.1:8080> (or wherever `http_listen_addr` points).

## Commands (skills)

Bot commands work like Claude Code Skills: each is a directory under `skills_dir`
(default `./skills`) containing a `SKILL.md` with a YAML frontmatter header and a
prompt body:

```markdown
---
name: history
description: Show recent messages in this room.
usage: "history [count]"
aliases:
  - recent
tools:
  - message_log
args:
  - name: count
    type: integer
    description: "How many recent messages to show"
    default: 5
    min: 1
    max: 20
---
You are replying to a Matrix chat command. Summarize or list the recent messages
provided below for the user in a clear, concise way. ...
```

Frontmatter fields:

- `name` / `description` (required) — the command name matched against the
  classifier's extracted `command.name`, and the one-line description shown in `help`.
- `usage` (optional) — a usage hint shown alongside the description in `help`.
- `aliases` (optional) — other names that resolve to this skill (e.g. `history` /
  `recent`), matched case-insensitively like `name`. An alias that collides with
  another skill's name or another skill's alias fails the whole load at startup —
  same ambiguous-dispatch reasoning as duplicate skill names, see below.
- `model` (optional) — overrides which Claude model runs this skill's prompt (e.g.
  `claude-sonnet-5` for a skill that needs more capability than the classifier's
  default `claude-haiku-4-5`). Falls back to `classify::MODEL` if unset.
- `tools` (optional) — capabilities the skill needs; the bot fetches the relevant
  context and injects it before calling Claude. Recognized values:
  - `message_log` — that room's recent messages (count from the `count` arg if
    declared, default 5, capped at 20 either way).
  - `room_info` — the room's ID, name, and topic.
  - `current_time` — the current UTC time (skills reasoning about "today"/"now" need
    this explicitly; the model has no clock of its own).
  - `random_choice` — a true random pick (real Rust randomness, not an LLM guess)
    over the skill's `choices` argument.
  - `urban_dictionary` / `imdb_lookup` / `leafly_strain` — real outbound HTTP calls
    to Urban Dictionary, OMDb, and Leafly's (unofficial) consumer API, respectively;
    see `src/tools.rs`. Claude itself never makes these calls or sees raw JSON — the
    Rust code fetches and formats the result (or a clear "not found"/"lookup failed"
    line) as context *before* the one completion call, same as every other tool.
- `args` (optional) — declares expected arguments by name, `type`
  (`string`/`integer`/`number`/`boolean`/`array` — `array` means an array of strings,
  e.g. `random`'s `choices`), and optionally `description`, `required`, `default`,
  and `min`/`max` (numeric types: value bounds; `array`: element-count bounds). If a
  skill declares `args`, its invocation's parsed arguments are validated against this
  schema before Claude is ever called — a missing required argument, wrong type, or
  out-of-range value/length gets an immediate `Invalid arguments: ...` reply instead
  of an LLM call. A skill that declares no `args` gets the classifier's raw extracted
  arguments unvalidated (backward-compatible free-form behavior).

When a message classifies as `intent: command`, `handle_command` in `src/handler.rs`
looks up `command.name` (case-insensitively, checking aliases too) in the loaded
`SkillRegistry` (`src/skills.rs`) and, if found, sends the skill's prompt to Claude as
the system prompt — along with the user's raw message text, the validated/defaulted
command arguments, and any injected tool context — and replies with the generated text
(`skills::execute`, a plain completion call with no forced tool use, unlike
`classify_message`). `help` is a reserved, built-in command (not a skill file) that
lists every loaded skill (including its aliases); an unrecognized or missing command
name gets a friendly `Unknown command. Try "help"...` reply instead of silently
falling through to the generic stub.

Eleven skills ship as worked examples under `skills/`:

| Skill | Aliases | Tools | Demonstrates |
| --- | --- | --- | --- |
| `history` | `recent` | `message_log` | the original worked example — `aliases` + an `args` schema (`count`, defaulted/bounded) |
| `room` | — | `room_info`, `current_time` | injecting room metadata and the current time |
| `digest` | `summary` | `message_log` | thematic summarization vs. `history`'s raw listing |
| `vibe` | — | `message_log` | reading the `sentiment` the classifier already tagged each message with |
| `topics` | — | `message_log` | reading the `entities` the classifier already tagged each message with |
| `standup` | — | `message_log`, `current_time` | combining two tools in one skill |
| `define` | — | *(none)* | a pure-prompt skill with a required `string` argument and no tools at all |
| `random` | — | `random_choice` | an `array`-typed arg (`choices`, `min: 2`) and real Rust-side randomness — Claude only phrases the already-made pick |
| `ud` | `urban` | `urban_dictionary` | an optional arg with deliberately *no* default, so omitting it hits Urban Dictionary's own random-word behavior instead of a bot-side default |
| `imdb` | — | `imdb_lookup` | an outbound HTTP call to a third-party API (OMDb) requiring a configured API key (`omdb_api_key`, see below) |
| `strain` | — | `leafly_strain` | an outbound HTTP call plus real disambiguation logic (AKA-name parsing, last-exact-match-wins selection) ported faithfully from gordy |

`random`, `ud`, `imdb`, and `strain` are ported from *gordy*, a separate, existing Matrix bot referenced read-only during development. gordy's `help` needed no port (matrix-llm-bot already has an equivalent built-in), and its `pp` (procedural animated-GIF generation via PIL, no LLM involved, binary media output) was deliberately skipped as out of scope for a prompt/skill system. `imdb` was re-pointed from gordy's original scraping library to the documented OMDb API. The `imdb` skill needs `omdb_api_key` set in `config.toml` (see `config.toml.example`); without it, the skill replies with a clear "not configured" message instead of attempting a lookup or calling Claude at all.

`vibe` and `topics` are only possible because the `message_log` tool's injected
context includes each message's sentiment and tagged entities (`format_entry_tags` in
`src/skills.rs`) — data the classifier was already computing and logging, just not
previously surfaced anywhere. Unlike `store_path` and `message_log_dir`, `skills_dir`
holds source you write, not runtime state, so it's **not** git-ignored.

A skill file with bad frontmatter, an unknown `tools`/`args` entry, or a missing
`SKILL.md` is skipped with a `tracing::warn!` at startup — the bot still starts and
every other skill still works. The things that *do* fail startup: two skills sharing
the same `name` across different directories, or an `alias` colliding with another
skill's name or alias — all ambiguous-dispatch situations the bot can't silently
resolve on your behalf.

## Notes

- Session and E2E crypto state are persisted to `./data/store` (SQLite) so encryption keys survive restarts. This path is git-ignored, as is `config.toml`.
- Device verification/cross-signing is **not** implemented — the bot will implicitly trust devices it encounters. Add explicit verification in `Bot::new` (`src/bot.rs`) before relying on this in encrypted rooms you care about.
- `on_room_message` (`src/handler.rs`) shows a typing indicator for as long as it's processing a message that reaches classification — `TypingIndicator` loops `Room::typing_notice(true)` every `TYPING_REFRESH_INTERVAL` (3s) in the background, since Matrix's typing state expires after ~4s unless refreshed and classification plus response generation routinely take longer than that. It's stopped (clearing the indicator immediately) once a response is ready to send; on an early return — classification failed, or the message didn't need a response — it's just dropped, which aborts the background loop and lets the indicator expire on its own rather than lingering.
- Every reply (`send_message` in `src/handler.rs`) is directed at whoever sent the message that triggered it: the body is prefixed with a Markdown link to the sender's `matrix.to` URI (e.g. `[@alice:matrix.org](https://matrix.to/#/@alice:matrix.org): ...`), which clients render as a highlighted "pill," and the Matrix intentional-mention field (`m.mentions`) is set to that user so it actually notifies them rather than just naming them in the text.
- Every message is classified via a forced tool call to Claude (`claude-haiku-4-5` — cheap and fast, well-suited to this constrained extraction task) before the bot decides whether/how to respond — see `MessageAnalysis` in `src/classify.rs` for the schema (intent, confidence, sentiment, entities, command). Message metadata already known from the Matrix event (sender, room, timestamp) is deliberately *not* part of that schema — it's attached in code, not re-derived by the model. The classifier's system prompt is built with `SkillRegistry::command_reference()` (`src/skills.rs`), which lists every loaded skill's name/aliases and its declared `args` key names/types — without this, the model has no way to know a skill expects (say) an arg named `name` rather than `strain`/`query`/anything else it might otherwise guess, and `resolve_args` would reject the mismatched key as missing.
- The classifier can recognize command intent from natural language alone (it'll classify `strain gsc` as a command just as readily as `!strain gsc`), but `generate_response` (`src/handler.rs`) only actually dispatches to a skill when the raw message also starts with `!` — a deterministic, LLM-independent gate on top of the model's judgment, so a skill can't fire off an offhand sentence that merely sounds command-like. A message classified as a command but missing the `!` falls through to the normal chat-reply handling (only replied to if it's directed at the bot, per `is_directed_at_bot` below).
- Every classified message — whether or not the bot replies — is appended as one JSON line to a per-room log file under `message_log_dir` (default `./data/messages`; git-ignored), one file per room named after its sanitized room ID (e.g. `_abcDEFghi_matrix.org.jsonl`), via `MessageLogger` in `src/message_log.rs`. Each line has `logged_at`, `room_id`, `event_id`, `sender`, `origin_server_ts_ms`, `body`, and the full `analysis` object. Logging is synchronous (a small `write()` under a `std::sync::Mutex`, one open file handle per room, held for the process lifetime); move it to `tokio::task::spawn_blocking` if message volume ever makes that a bottleneck.
- `generate_response` in `src/handler.rs` handles `Intent::Command` messages via skills (see "Commands (skills)" above) regardless of addressing. Every other intent (greetings, questions, chitchat, ...) is answered by `generate_chat_reply` — a real Claude completion, but only when `is_directed_at_bot` returns true: the message either `@mentions` the bot (Matrix's `m.mentions`) or is a reply to one of the bot's own prior messages (resolved via `Room::event` on the `m.relates_to` target). The classifier's own `requires_response` judgment (text-only, no addressing signal) still gates everything first; without an explicit mention/reply on top of that, the bot stays silent rather than replying to every question a human asks another human. `Intent::Acknowledgement` never gets a reply either way. `CHAT_SYSTEM_PROMPT` gives the bot an informative-and-helpful-first personality with a light, mildly snarky sense of humor woven in (a dry aside, occasional teasing) rather than a formal-assistant tone — the humor is seasoning on top of an actual answer, not the point of the reply — with a good-natured boundary baked into the prompt (no targeting protected traits, no genuinely cutting remarks meant to hurt someone, no inventing personal details). Replies are grounded in the room's recent history: `generate_chat_reply` pulls the last `CHAT_HISTORY_LIMIT` (20) messages via `MessageLogger::recent` and formats them as a `sender: body` transcript ahead of the message being replied to, so the bot can call back to running jokes or a specific user's own history in the room instead of replying in a vacuum. Usage from these calls is tracked under the `"chat"` label (see `UsageTracker` below).
- The `message_log` skill tool reads via `MessageLogger::recent(room_id, limit)`, which parses that room's JSONL file from disk on every call — no in-memory cache.
- The `random_choice`, `urban_dictionary`, `imdb_lookup`, and `leafly_strain` tools live in `src/tools.rs`, behind a shared `ToolClients` (one reused `reqwest::Client`, plus the optional OMDb key). Unlike the local tools, these make real outbound HTTP calls to third-party APIs — `reqwest` (with `rustls`, reusing matrix-sdk's already-compiled TLS stack rather than a second openssl-based one) and `rand` (true Rust-side randomness for `random_choice`, not an LLM guess) are the two dependencies this added. A lookup failure here (network/HTTP/parse error) is logged and turned into an explicit "(this lookup failed due to a technical error...)" line appended to the prompt context, so Claude apologizes instead of inventing an answer — a third failure-handling pattern alongside `message_log`'s silent skip and `run_prompt`'s generic fallback. No automated test hits any of these live APIs; verify them with a manual smoke test through an actual Matrix room.
- Every Claude API call's token usage is accumulated in-memory by `UsageTracker` (`src/usage.rs`) — the classifier (`classify_message`, labeled `"classify"`) and every skill's completion (`skills::execute`'s `run_prompt`, labeled by skill name) each record their `input_tokens`/`output_tokens` there right after the response comes back. It's the number that actually answers "how much is this bot costing," and nothing else in the bot tracked it before. In-memory only — resets on restart, same as everything but the message log and the crypto store — and exposed via the status server's `/api/usage` (see below) and the status page's "Claude API usage" section.
- The status/debugging server (`src/status_server.rs`) runs as a separate `tokio::spawn`ed task alongside the sync loop, reading the same `Client`, `MessageLogger`, `SkillRegistry`, and `UsageTracker` the bot uses — it doesn't duplicate any state. Routes: `GET /` (HTML page — bot status, token usage totals and a per-label breakdown table, a room dropdown with recent classified messages, and a table of loaded commands), `GET /api/status` (bot user, homeserver, classifier model, uptime, joined rooms as JSON), `GET /api/rooms/{room_id}/messages?limit=N` (that room's recent `LoggedMessage`s as JSON), `GET /api/skills` (loaded commands' name/description/usage/tools/aliases/model as JSON), `GET /api/usage` (overall and per-label — `"classify"` or a skill name — token/request totals as JSON). **No authentication** — binds to `127.0.0.1` by default (`http_listen_addr` in config); only widen this deliberately, and put it behind your own auth/reverse proxy if you do.
- `anthropic-sdk-rust` is a third-party community crate, not an Anthropic-maintained SDK — pin/review it accordingly. It also defines a `ServerTool::WebSearch` type that's never actually wired into the request builder (`MessageCreateParams.tools` only accepts custom `Tool`s) — so a `web_search` skill tool isn't feasible without bypassing the crate with a raw HTTP call, which we've deliberately not done.
