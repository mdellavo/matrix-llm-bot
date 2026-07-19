# matrix-llm-bot

A stubbed-out Rust Matrix chat bot built on [`matrix-sdk`](https://github.com/matrix-org/matrix-rust-sdk), with TLS and E2E encryption support out of the box. It joins a configurable set of rooms and, for every message it receives, calls Claude (via [`anthropic-sdk-rust`](https://crates.io/crates/anthropic-sdk-rust)) to classify the message into a structured `MessageAnalysis` (see `src/classify.rs`) — intent, sentiment, entities, whether it needs a reply at all, and (for commands) a name and arguments. Commands dispatch to **skills**: prompts loaded from `skills/<name>/SKILL.md`, modeled on Claude Code's Skills, each run through Claude to produce a real generated reply (see "Commands (skills)" below). Every other intent is still a stub (`generate_response` in `src/handler.rs`): it picks a canned response per intent rather than generating one with an LLM. Alongside the sync loop, a small [`axum`](https://github.com/tokio-rs/axum) HTTP server (`src/status_server.rs`) serves a status/debugging page and JSON API over the same data.

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

4. Run it:

   ```sh
   cargo run
   ```

   By default it reads `config.toml` from the current directory; set `MATRIX_LLM_BOT_CONFIG` to point elsewhere. Set `RUST_LOG` (e.g. `RUST_LOG=debug`) to control log verbosity.

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
- `args` (optional) — declares expected arguments by name, `type`
  (`string`/`integer`/`number`/`boolean`), and optionally `description`, `required`,
  `default`, and (numeric types only) `min`/`max`. If a skill declares `args`, its
  invocation's parsed arguments are validated against this schema before Claude is
  ever called — a missing required argument, wrong type, or out-of-range value gets an
  immediate `Invalid arguments: ...` reply instead of an LLM call. A skill that
  declares no `args` gets the classifier's raw extracted arguments unvalidated
  (backward-compatible free-form behavior).

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

Seven skills ship as worked examples under `skills/`:

| Skill | Aliases | Tools | Demonstrates |
| --- | --- | --- | --- |
| `history` | `recent` | `message_log` | the original worked example — `aliases` + an `args` schema (`count`, defaulted/bounded) |
| `room` | — | `room_info`, `current_time` | injecting room metadata and the current time |
| `digest` | `summary` | `message_log` | thematic summarization vs. `history`'s raw listing |
| `vibe` | — | `message_log` | reading the `sentiment` the classifier already tagged each message with |
| `topics` | — | `message_log` | reading the `entities` the classifier already tagged each message with |
| `standup` | — | `message_log`, `current_time` | combining two tools in one skill |
| `define` | — | *(none)* | a pure-prompt skill with a required `string` argument and no tools at all |

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
- Every message is classified via a forced tool call to Claude (`claude-haiku-4-5` — cheap and fast, well-suited to this constrained extraction task) before the bot decides whether/how to respond — see `MessageAnalysis` in `src/classify.rs` for the schema (intent, confidence, sentiment, entities, command). Message metadata already known from the Matrix event (sender, room, timestamp) is deliberately *not* part of that schema — it's attached in code, not re-derived by the model.
- Every classified message — whether or not the bot replies — is appended as one JSON line to a per-room log file under `message_log_dir` (default `./data/messages`; git-ignored), one file per room named after its sanitized room ID (e.g. `_abcDEFghi_matrix.org.jsonl`), via `MessageLogger` in `src/message_log.rs`. Each line has `logged_at`, `room_id`, `event_id`, `sender`, `origin_server_ts_ms`, `body`, and the full `analysis` object. Logging is synchronous (a small `write()` under a `std::sync::Mutex`, one open file handle per room, held for the process lifetime); move it to `tokio::task::spawn_blocking` if message volume ever makes that a bottleneck.
- `generate_response` in `src/handler.rs` is still a stub for non-command intents: it picks a canned reply per `analysis.intent` rather than generating one. `Intent::Command` messages are fully dynamic (see "Commands (skills)" above); everything else (greetings, questions, chitchat, ...) still needs a real completion call using `analysis` as context.
- The `message_log` skill tool reads via `MessageLogger::recent(room_id, limit)`, which parses that room's JSONL file from disk on every call — no in-memory cache.
- The status/debugging server (`src/status_server.rs`) runs as a separate `tokio::spawn`ed task alongside the sync loop, reading the same `Client`, `MessageLogger`, and `SkillRegistry` the bot uses — it doesn't duplicate any state. Routes: `GET /` (HTML page — bot status, a room dropdown with recent classified messages, and a table of loaded commands), `GET /api/status` (bot user, homeserver, classifier model, uptime, joined rooms as JSON), `GET /api/rooms/{room_id}/messages?limit=N` (that room's recent `LoggedMessage`s as JSON), `GET /api/skills` (loaded commands' name/description/usage/tools/aliases/model as JSON). **No authentication** — binds to `127.0.0.1` by default (`http_listen_addr` in config); only widen this deliberately, and put it behind your own auth/reverse proxy if you do.
- `anthropic-sdk-rust` is a third-party community crate, not an Anthropic-maintained SDK — pin/review it accordingly. It also defines a `ServerTool::WebSearch` type that's never actually wired into the request builder (`MessageCreateParams.tools` only accepts custom `Tool`s) — so a `web_search` skill tool isn't feasible without bypassing the crate with a raw HTTP call, which we've deliberately not done.
