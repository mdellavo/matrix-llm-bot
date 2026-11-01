# matrix-llm-bot

A Rust Matrix chat bot built on [`matrix-sdk`](https://github.com/matrix-org/matrix-rust-sdk), with TLS and E2E encryption support out of the box.

It joins a configured set of rooms, records what's said in SQLite, and uses Claude to classify intent and sentiment. `!commands` dispatch to **skills**: prompt files modeled on Claude Code's Skills. Other messages get a Claude-generated reply when the bot is addressed (an `@mention`, a reply, a message starting with its name, or a follow-up in its thread), and greetings to the room get one on a cooldown. Replies can draw on room history, saved memories, web search, links, and images. A small built-in HTTP server exposes a status/debugging page alongside the sync loop.

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

## Deployment

The bot runs as a single container built from the `Dockerfile`. This Compose service
builds straight from GitHub, with the config and state kept on the host:

```yaml
services:
  matrix-llm-bot:
    container_name: matrix-llm-bot
    build: https://github.com/mdellavo/matrix-llm-bot.git#main
    restart: unless-stopped
    environment:
      ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
    volumes:
      - ./matrix-llm-bot/config.toml:/app/config.toml:ro
      - ./matrix-llm-bot/data:/app/data
    ports:
      - "127.0.0.1:8081:8080"
```

Before the first start:

- Put `config.toml` at `./matrix-llm-bot/config.toml`, and put `ANTHROPIC_API_KEY` in an
  `.env` file next to the Compose file.
- Set `http_listen_addr = "0.0.0.0:8080"` in `config.toml`, or the status page won't be
  reachable from outside the container. The `ports` entry above still limits it to the
  host, which matters because the page shows message content with no login.
- Set `cost_limit_usd`. Without it, nothing caps Claude API spend.

**Keep the `data` directory.** It holds the bot's database (`bot.db`) and its Matrix
device and encryption keys (`store/`), so back it up. If `store/` is lost, the bot logs in
as a new device and loses access to encrypted history. Back up `bot.db` with
`sqlite3 bot.db ".backup backup.db"` rather than copying the file while the bot runs, since
recent writes can be in its `-wal` file.

### Updating

Build first, so the running bot stays up until the new image is ready. Tag the current
image so you can go back to it:

```sh
docker tag matrix-llm-bot-image:latest matrix-llm-bot-image:previous
docker compose build --pull matrix-llm-bot
docker compose up -d --no-deps matrix-llm-bot
docker compose logs -f matrix-llm-bot
```

Replace `matrix-llm-bot-image` with the image name from `docker images`, which Compose
derives from the project's directory name. A healthy start logs `restored saved session`
and then `starting sync loop`.

To roll back, retag the previous image as `latest` and run the `up` command again:

```sh
docker tag matrix-llm-bot-image:previous matrix-llm-bot-image:latest
docker compose up -d --no-deps matrix-llm-bot
```

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
  - `message_log` — that room's recent messages, including the bot's replies.
  - `message_search` — the best matches for the `query` argument from the room's whole log.
  - `room_info` — the room's ID, name, and topic.
  - `current_time` — the current UTC time.
  - `random_choice` — a true random pick (real Rust randomness, not an LLM guess).
  - `urban_dictionary` / `imdb_lookup` / `leafly_strain` — outbound HTTP calls to Urban Dictionary, OMDb, and Leafly, respectively (see `src/tools.rs`).
  - `web_search` — lets Claude search the web, up to twice per run.
- `args` (optional) — declares expected arguments (name, type, required/default/bounds). Arguments are validated before Claude is ever called.

A `!command`'s args are parsed from the text after its name, without a model: they fill the skill's declared `args` in order, with the last one taking the rest of the line. Lists split on commas or " or ", or on spaces if there are neither.

Twelve skills ship as worked examples under `skills/`, alongside the built-in `!help` and `!memories`:

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

- The bot's own data lives in one SQLite database at `database_path` (`src/db.rs`): messages, reactions, facts, room summaries, and usage totals. On startup, data from older versions (`data/messages/*.jsonl`, `memory.json`, `usage.json`) is imported once and those files are left in place, ignored from then on, so they can be deleted once you're happy with the import. A version from before this can't read the database, and rolling back to one would miss anything logged since.
- Matrix session and E2E crypto state stays separate, in `./data/store`, which matrix-sdk manages. `config.toml` and `data/` are git-ignored. New options are documented in `config.toml.example`. Device verification/cross-signing isn't implemented — the bot implicitly trusts devices it encounters (see `Bot::new` in `src/bot.rs` before relying on this for rooms you care about).
- Every message is recorded per room (`src/message_log.rs`), whether or not the bot replies to it — this is what skills like `history`/`digest`/`vibe` read from, and what the status page's room browser shows. The exception is `ignored_users` (`config.toml`): messages from those senders are dropped in `on_room_message` before any of that — no classification, no logging, no reply — so an ignored user never shows up in message history either.
- Addressing is decided first, in code: a message gets a Claude-generated reply when it @mentions the bot, replies to one of its messages, or starts with its name (`gordy: ...`, `gordy, ...`, `gordy what...`), whatever the classifier thought of it. A bare acknowledgement is the one exception. It gets a 👍 reaction instead. Unaddressed messages get nothing, except a greeting to the room, once per room per 5 minutes (`reply_kind` in `src/handler.rs`), and unprompted comments if those are turned on.
- Classification (`classify_message` in `src/classify.rs`) records intent, sentiment, and entities, and spots room greetings. It doesn't run for `!commands`, and if it fails the message is still logged and still answered if addressed.
- Chat replies (`claude-opus-5-5`) see the last 50 messages with timestamps, display names and reactions, including the bot's own replies, the message being replied to, saved facts, and notes on older chat. They can web search up to 3 times per reply when facts might be wrong or stale, at $0.01 per search, and read up to 3 links from the conversation.
- Memory (`src/room_memory.rs`): chat replies have `remember` and `forget` tools. Facts stay in the room they were learned in, and `!memories` lists them. Room summaries are saved alongside them.
- Images: when addressed, the bot can see an attached image, one it's replying to, or one posted in the last 5 minutes. Images are sent to Claude but not kept by the bot, and unaddressed ones cost nothing.
- Links: pages linked in chat are fetched when posted, with no Claude cost, and the title, description, and start of the text are saved with the message, so the bot knows what was shared without being asked. Replies get the page text for links in the message, in the message it replies to, or posted in the last 5 minutes. Set `fetch_links = false` to turn this off. Only public addresses on ports 80 and 443 are fetched: private addresses are refused in the resolver and on redirects, since anyone in a room can post a URL. Fetching does reveal the bot's IP to the linked site, and links posted in encrypted rooms are fetched and saved too. Direct links to images aren't viewed, only noted as images.
- Reactions: a lone-emoji reply is sent as a reaction. Reactions from others are saved and shown in history. Removed reactions aren't tracked.
- Each room's messages are handled one at a time, in order (`src/room_queue.rs`). Quick follow-ups from the same person are answered together, after a 1.5 second wait on addressed messages.
- Unprompted comments are off by default. Set `interjection_chance` to turn them on; the bot can still decide to pass, and `interjection_cooldown_secs` limits them per room.
- Replies show a typing indicator while being generated. Replies to commands and conversation go in a thread on the original post, or stay in the thread they were asked in. Greetings and unprompted comments are plain replies outside a thread.
- Private conversations: invite the bot to a room and it joins, if `dm_allowed_users` allows the inviter (by default, anyone on its own homeserver). Other invites are declined. With one person, every message is answered with a normal message, and a bare acknowledgement gets a 👍. With a few people, up to 10 members counting the bot, it answers everything except messages that mention someone or reply to someone else, and uses replies so it's clear who it's answering. Neither uses threads unless someone starts one. Above 10 members it behaves as in any other room. Memory stays with the room, so nothing crosses between chats. Only the inviter is checked, so they vouch for anyone they add. These rooms are left off the status page, but they are stored in the database like everything else.
- Status page: bot and Claude usage details, plus activity counted from the database: message, reply, thread, image, link, and reaction totals, per-room and per-person tables, the last 14 days, and breakdowns of reply kinds, intents, and sentiment. Private conversations appear only as a total.
- Threads: once the bot has posted in a thread, every message there counts as addressed to it, unless the message mentions someone or replies to someone else. A bare acknowledgement gets no reply. The reply sees the whole thread, up to its last 50 messages, plus any images and links in it, however old. Each follow-up is a full chat reply, so a busy thread costs more than room chat.
- Claude API token usage and an estimated USD cost are tracked per command/reply type, saved so they survive restarts, and shown on the status page (`src/usage.rs`). Estimates come from a hardcoded pricing table, not fetched live, and leave out cache tokens. Set `cost_limit_usd` to stop Claude calls at a spending cap.
- The status/debugging server (`src/status_server.rs`) has **no authentication** and binds to `127.0.0.1` by default — only widen `http_listen_addr` deliberately, and put it behind your own auth/reverse proxy if you do.
- `threatflux-anthropic-sdk` is a third-party community crate, not an Anthropic-maintained SDK — pin/review it accordingly. Claude calls use its raw request method (`src/llm.rs`), since its typed builders don't cover the newer API fields.

## Roadmap

Ideas for later, not commitments. See [CHANGELOG.md](CHANGELOG.md) for what's shipped.

### Quality

- Build a test set from real replies, so prompt and model changes can be measured
- Use 👍/👎 reactions on bot messages as feedback on which replies landed
- Tune tone, reply length, and search use from live results

### Features

- Reminders: "gordy, remind me Friday to..."
- Scheduled posts, such as a daily digest or a catch-up on what someone missed

### Reliability

- Handle edited messages, which currently keep their original text in the log
- Automatic backups of the `data` directory
- Track removed reactions

### Cost and performance

- Per-user rate limits
- Prompt caching, which means moving the timestamp out of the system prompt
- Cheaper classification, by skipping messages that clearly don't need it or running it after the reply
- Pruning old prompts from the database, which are most of its size
- Full-text search with SQLite FTS5, as history grows
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
