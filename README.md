# matrix-llm-bot

A Rust Matrix chat bot built on [`matrix-sdk`](https://github.com/matrix-org/matrix-rust-sdk), with TLS and E2E encryption support out of the box.

It joins a configured set of rooms and uses Claude to understand every message it sees — classifying intent, sentiment, and (for commands) arguments — before deciding whether and how to respond. Commands dispatch to **skills**: prompt files modeled on Claude Code's Skills. Casual messages get a real, Claude-generated reply too, but only when the bot is actually addressed (an `@mention` or a reply), except greetings, which get a fun reply regardless, on a cooldown. A small built-in HTTP server exposes a status/debugging page alongside the sync loop.

## Setup

1. Copy the example config and fill in your bot account's credentials:

   ```sh
   cp config.toml.example config.toml
   ```

2. Edit `config.toml`:
   - `homeserver_url` — your Matrix homeserver
   - `username` / `password` — the bot account's login credentials
   - `rooms` — room IDs (`!abc:example.org`) or aliases (`#room:example.org`) to join and monitor

3. Set `ANTHROPIC_API_KEY` in the environment:

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

- `name` / `description` (required) — the command name and its one-line description in `help`.
- `usage` (optional) — a usage hint shown alongside the description in `help`.
- `aliases` (optional) — other names that resolve to this skill.
- `model` (optional) — overrides which Claude model runs this skill's prompt; defaults to the classifier's model.
- `tools` (optional) — capabilities the skill needs; the bot fetches the relevant context and injects it before calling Claude:
  - `message_log` — that room's recent messages.
  - `room_info` — the room's ID, name, and topic.
  - `current_time` — the current UTC time.
  - `random_choice` — a true random pick (real Rust randomness, not an LLM guess).
  - `urban_dictionary` / `imdb_lookup` / `leafly_strain` — outbound HTTP calls to Urban Dictionary, OMDb, and Leafly, respectively (see `src/tools.rs`).
- `args` (optional) — declares expected arguments (name, type, required/default/bounds). If declared, arguments are validated before Claude is ever called; a skill with no `args` gets the classifier's raw extracted arguments unvalidated.

A command's args are extracted by the same Claude call that classifies the message (`src/classify.rs`), matched against each skill's declared argument names so the classifier doesn't have to guess them. Commands only actually dispatch when the message starts with a literal `!` — the classifier can recognize command *intent* without one, but running a skill off just that guess would be too easy to trigger by accident.

Eleven skills ship as worked examples under `skills/`:

| Skill | Aliases | Tools | Demonstrates |
| --- | --- | --- | --- |
| `history` | `recent` | `message_log` | aliases + a bounded/defaulted arg |
| `room` | — | `room_info`, `current_time` | injecting room metadata and the current time |
| `digest` | `summary` | `message_log` | thematic summarization vs. `history`'s raw listing |
| `vibe` | — | `message_log` | reading each message's classified sentiment |
| `topics` | — | `message_log` | reading each message's tagged entities |
| `standup` | — | `message_log`, `current_time` | combining two tools in one skill |
| `define` | — | *(none)* | a pure-prompt skill, no tools |
| `random` | — | `random_choice` | real randomness — Claude only phrases the pick |
| `ud` | `urban` | `urban_dictionary` | an optional arg with no default, so omitting it falls through to Urban Dictionary's own random-word behavior |
| `imdb` | — | `imdb_lookup` | a third-party API call requiring a configured key |
| `strain` | — | `leafly_strain` | an API call plus real disambiguation logic |

`random`, `ud`, `imdb`, and `strain` are ported from *gordy*, a separate Matrix bot referenced read-only during development; `imdb` was re-pointed from gordy's scraping approach to the documented OMDb API.

A skill file with bad frontmatter or an unrecognized `tools`/`args` entry is skipped with a warning at startup — everything else still loads. Two skills sharing a `name`, or an alias colliding with another skill's name/alias, fails startup instead, since the bot can't resolve that ambiguity on your behalf.

## Notes

- Session and E2E crypto state persist to `./data/store` (SQLite); `config.toml` and `data/` are git-ignored. Device verification/cross-signing isn't implemented — the bot implicitly trusts devices it encounters (see `Bot::new` in `src/bot.rs` before relying on this for rooms you care about).
- Every message is logged per-room as JSONL (`src/message_log.rs`), whether or not the bot replies to it — this is what skills like `history`/`digest`/`vibe` read from, and what the status page's room browser shows.
- Non-command replies are Claude-generated, not canned, but gated: casual chat only gets a reply when the bot is actually addressed (`@mention` or a reply-to; see `is_directed_at_bot` in `src/handler.rs`), while greetings (ported from gordy) reply to anyone, on a per-room 5-minute cooldown (`src/greeting.rs`) so a busy room doesn't get greeted on every "hi". These replies are grounded in recent room history with each message clearly attributed to its sender (`format_chat_turn` in `src/handler.rs`), so the model doesn't lose track of who said what.
- Replies show a typing indicator while being generated and are always directed at (and `@`-mention) whoever triggered them.
- Claude API token usage and an estimated USD cost are tracked in-memory per command/reply type and exposed on the status page (`src/usage.rs`) — cost estimates are based on a hardcoded pricing table, not fetched live.
- The status/debugging server (`src/status_server.rs`) has **no authentication** and binds to `127.0.0.1` by default — only widen `http_listen_addr` deliberately, and put it behind your own auth/reverse proxy if you do.
- `anthropic-sdk-rust` is a third-party community crate, not an Anthropic-maintained SDK — pin/review it accordingly.
