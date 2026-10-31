# matrix-llm-bot

A Rust Matrix chat bot built on [`matrix-sdk`](https://github.com/matrix-org/matrix-rust-sdk), with TLS and E2E encryption support out of the box.

It joins a configured set of rooms and uses Claude to understand every message it sees — classifying intent and sentiment — and decides whether to respond based on whether it was addressed. Commands dispatch to **skills**: prompt files modeled on Claude Code's Skills. Casual messages get a real, Claude-generated reply too, but only when the bot is actually addressed (an `@mention`, a reply, or a message starting with its name), except greetings, which get a fun reply regardless, on a cooldown. A small built-in HTTP server exposes a status/debugging page alongside the sync loop.

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
- `model` (optional) — overrides which Claude model runs this skill's prompt; defaults to the classifier's cheaper model. Skills that need judgment (`digest`, `standup`, `topics`, `vibe`, `define`) use `claude-sonnet-5-5`.
- `tools` (optional) — capabilities the skill needs; the bot fetches the relevant context and injects it before calling Claude:
  - `message_log` — that room's recent messages.
  - `room_info` — the room's ID, name, and topic.
  - `current_time` — the current UTC time.
  - `random_choice` — a true random pick (real Rust randomness, not an LLM guess).
  - `urban_dictionary` / `imdb_lookup` / `leafly_strain` — outbound HTTP calls to Urban Dictionary, OMDb, and Leafly, respectively (see `src/tools.rs`).
- `args` (optional) — declares expected arguments (name, type, required/default/bounds). If declared, arguments are validated before Claude is ever called; a skill with no `args` gets the classifier's raw extracted arguments unvalidated.

A `!command`'s args are parsed from the text after its name, without a model: they fill the skill's declared `args` in order, with the last one taking the rest of the line. Lists split on commas or " or ", or on spaces if there are neither.

Eleven skills ship as worked examples under `skills/`:

| Skill | Aliases | Tools | Demonstrates |
| --- | --- | --- | --- |
| `history` | `recent` | `message_log` | aliases + a bounded/defaulted arg |
| `room` | — | `room_info`, `current_time` | injecting room metadata and the current time |
| `digest` | `summary` | `message_log` | thematic summarization vs. `history`'s raw listing |
| `vibe` | — | `message_log` | reading each message's classified sentiment |
| `topics` | — | `message_log` | reading each message's tagged entities |
| `standup` | — | `message_log`, `current_time` | combining two tools in one skill |
| `define` | — | `web_search` | a skill that can look things up on the web |
| `search` | — | `message_search` | answering from the room's whole log, not just recent messages |
| `random` | — | `random_choice` | real randomness — Claude only phrases the pick |
| `ud` | `urban` | `urban_dictionary` | an optional arg with no default, so omitting it falls through to Urban Dictionary's own random-word behavior |
| `imdb` | — | `imdb_lookup` | a third-party API call requiring a configured key |
| `strain` | — | `leafly_strain` | an API call plus real disambiguation logic |

`random`, `ud`, `imdb`, and `strain` are ported from *gordy*, a separate Matrix bot referenced read-only during development; `imdb` was re-pointed from gordy's scraping approach to the documented OMDb API.

A skill file with bad frontmatter or an unrecognized `tools`/`args` entry is skipped with a warning at startup — everything else still loads. Two skills sharing a `name`, or an alias colliding with another skill's name/alias, fails startup instead, since the bot can't resolve that ambiguity on your behalf.

## Notes

- Session and E2E crypto state persist to `./data/store` (SQLite); `config.toml` and `data/` are git-ignored. Device verification/cross-signing isn't implemented — the bot implicitly trusts devices it encounters (see `Bot::new` in `src/bot.rs` before relying on this for rooms you care about).
- Every message is logged per-room as JSONL (`src/message_log.rs`), whether or not the bot replies to it — this is what skills like `history`/`digest`/`vibe` read from, and what the status page's room browser shows. The exception is `ignored_users` (`config.toml`): messages from those senders are dropped in `on_room_message` before any of that — no classification, no logging, no reply — so an ignored user never shows up in message history either.
- Addressing is decided first, in code: a message gets a Claude-generated reply when it @mentions the bot, replies to one of its messages, or starts with its name (`gordy: ...`, `gordy, ...`, `gordy what...`), whatever the classifier thought of it. A bare acknowledgement is the one exception. Unaddressed messages get nothing, except a greeting to the room, once per room per 5 minutes (`reply_kind` in `src/handler.rs`).
- Classification (`classify_message` in `src/classify.rs`) records intent, sentiment, and entities, and spots room greetings. It doesn't run for `!commands`, and if it fails the message is still logged and still answered if addressed.
- Chat replies (`claude-opus-5-5`) see the last 50 messages with timestamps, display names and reactions, including the bot's own replies, the message being replied to, saved facts, and notes on older chat. They can web search up to 3 times per reply when facts might be wrong or stale, at $0.01 per search, and read up to 3 links from the conversation.
- Memory (`src/room_memory.rs`, persisted to `memory_path`): chat replies have `remember` and `forget` tools. Facts stay in the room they were learned in, and `!memories` lists them. Room summaries are saved in the same file.
- Images: when addressed, the bot can see an attached image, one it's replying to, or one posted in the last 5 minutes. Images are never stored, and unaddressed ones cost nothing.
- Reactions: a lone-emoji reply is sent as a reaction, and an addressed "thanks" gets a 👍. Reactions from others are logged to `<room>.reactions.jsonl` and shown in history. Removed reactions aren't tracked.
- Each room's messages are handled one at a time, in order (`src/room_queue.rs`). Quick follow-ups from the same person are answered together, after a 1.5 second wait on addressed messages.
- Unprompted comments are off by default. Set `interjection_chance` to turn them on; the bot can still decide to pass, and `interjection_cooldown_secs` limits them per room.
- Replies show a typing indicator while being generated and are sent as real Matrix replies to the message that triggered them.
- Claude API token usage and an estimated USD cost are tracked in-memory per command/reply type and exposed on the status page (`src/usage.rs`) — cost estimates are based on a hardcoded pricing table, not fetched live.
- The status/debugging server (`src/status_server.rs`) has **no authentication** and binds to `127.0.0.1` by default — only widen `http_listen_addr` deliberately, and put it behind your own auth/reverse proxy if you do.
- `anthropic-sdk-rust` is a third-party community crate, not an Anthropic-maintained SDK — pin/review it accordingly.

## Roadmap

Ideas for later, not commitments. See [CHANGELOG.md](CHANGELOG.md) for what's shipped.

### Quality

- Build a test set from real replies, so prompt and model changes can be measured
- Use 👍/👎 reactions on bot messages as feedback on which replies landed
- Tune tone, reply length, and search use from live results

### Features

- Reminders: "gordy, remind me Friday to..."
- Scheduled posts, such as a daily digest or a catch-up on what someone missed
- Threads and DMs: keep replies inside threads, and decide how DMs should work

### Reliability

- Handle edited messages, which currently keep their original text in the log
- Back up the `data` directory, which holds the bot's logs and memory
- Track removed reactions

### Cost and performance

- Per-user rate limits
- Prompt caching, which means moving the timestamp out of the system prompt
- Cheaper classification, by skipping messages that clearly don't need it or running it after the reply
- Faster log reads, using SQLite or an in-memory buffer instead of re-reading the file
- Log rotation
- Fuller cost tracking: price cache tokens, and show spend per day and per person

### Security and admin

- Login for the status page
- Device verification instead of trusting all devices
- Stronger limits on prompt injection from web results and room text
- Admin commands to ignore a user, reload skills, or show spend from chat

### Deployment

- Keep the bot's service definition in the deploy config
- Deploy pinned tags or commits, not `main`
- CI for tests and clippy on every push
- Alerts for sync failures, Claude errors, and spend
- Replace the third-party Anthropic SDK with a small HTTP client, now that calls are raw HTTP
