# matrix-llm-bot

A Rust Matrix chat bot built on [`matrix-sdk`](https://github.com/matrix-org/matrix-rust-sdk), with TLS and E2E encryption support out of the box.

It joins configured rooms and any private conversation an allowed user invites it to, records what's said in SQLite, and uses Claude to classify each message and hold a conversation:

- **Chat:** replies when addressed, in threads, with room history, memory, web search, linked pages, and images.
- **Commands:** `!commands` run **skills**, which are prompt files modeled on Claude Code's Skills.
- **Help:** ask about the bot in chat or with `!help <question>`, and it answers from this README and the changelog.
- **Private conversations:** one-on-one and small group chats where the bot answers without being addressed.
- **Status page:** a built-in web page with activity stats, Claude costs, and a message browser. `!stats` posts its numbers in chat.
- **Notices and alerts:** private messages to the people in `notify_users` each time the bot starts, with what's new since their last one, and when something needs attention.

## Setup

1. Copy the example config and fill in your bot account's credentials:

   ```sh
   cp config.toml.example config.toml
   ```

2. Edit `config.toml`:
   - `homeserver_url` — your Matrix homeserver
   - `username` / `password` — the bot account's login credentials
   - `rooms` — room IDs (`!abc:example.org`) or aliases (`#room:example.org`) to join and monitor

   See [Configuration](#configuration) for every option.

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

The bot runs as a container built from the `Dockerfile`, with an optional second
container for a headless browser. [`docker-compose.example.yaml`](docker-compose.example.yaml)
has both. Without the browser, this Compose service is all you need. It builds straight
from GitHub, with the config and state kept on the host:

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
- Check `dm_allowed_users`. By default, anyone on the bot's homeserver can start a private
  conversation, and every message there is a paid Claude call.

**Keep the `data` directory.** It holds the bot's database (`bot.db`) and its Matrix
device and encryption keys (`store/`), so back it up. If `store/` is lost, the bot logs in
as a new device and loses access to encrypted history. Back up `bot.db` with
`sqlite3 bot.db ".backup backup.db"` rather than copying the file while the bot runs, since
recent writes can be in its `-wal` file.

### Rendering pages with a headless browser

Many sites, Reddit among them, build their pages with JavaScript, so a plain fetch gets an
empty shell. With a headless Chrome configured, the bot renders `https` pages in it first,
including pages read with `read_page`, and falls back to the plain fetch if rendering fails.
Chrome runs in its own container with no internet route of its own. Every connection it
makes goes through a proxy inside the bot that applies the same public-address checks as
any other fetch. That matters because Chrome runs whatever scripts a posted page carries,
and without the proxy those scripts could reach your own network.

Add the browser and an internal network to the Compose file, as in
[`docker-compose.example.yaml`](docker-compose.example.yaml):

```yaml
services:
  matrix-llm-bot:
    # ...as above, plus:
    networks: [default, browser]

  chrome:
    image: chromedp/headless-shell:156.0.8078.36
    restart: unless-stopped
    networks: [browser]
    shm_size: 512m
    mem_limit: 1g
    security_opt: [no-new-privileges:true]

networks:
  browser:
    internal: true
```

Then set this in `config.toml`, and start Chrome before restarting the bot with
`docker compose up -d chrome`:

```toml
browser_url = "http://chrome:9222"
browser_proxy_url = "http://matrix-llm-bot:3128"
```

`internal: true` is what keeps Chrome off the internet, so don't leave it out. The proxy
only accepts connections from the `browser_url` host, and only tunnels `https` to public
addresses on ports 80 and 443. Plain `http` pages aren't rendered.

Pages read with `read_page` are scrolled to the bottom three times before being read,
since sites like Reddit only load comments as they come into view. Previews aren't, to
keep them quick. Rendering adds a few seconds to each linked page, and up to 2 pages
render at once. The bot connects to Chrome on its first render, so the two can be
restarted separately. After Chrome restarts, the next page falls back to the plain fetch
and the one after that reconnects.

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
and then `starting sync loop`, and users in `notify_users` get a message with the version
that's running and what's new in it.

To roll back, retag the previous image as `latest` and run the `up` command again:

```sh
docker tag matrix-llm-bot-image:previous matrix-llm-bot-image:latest
docker compose up -d --no-deps matrix-llm-bot
```

Chrome is a prebuilt image pinned to a version. To update it, change the tag and run
`docker compose pull chrome && docker compose up -d chrome`. Keep `USER_AGENT` in
`src/links.rs` on the same major version, so the user agent matches the browser sending it.

An older build refuses a database with a newer schema, so if the update migrated it, restore
a `bot.db` backup taken before the update as well. Anything logged after that backup is lost.

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
- `model` (optional) — overrides which Claude model runs this skill's prompt; defaults to the classifier's cheaper model. Skills that need judgment (`digest`, `standup`, `topics`, `vibe`, `define`, `search`) use `claude-sonnet-5-5`.
- `tools` (optional) — capabilities the skill needs; the bot fetches the relevant context and injects it before calling Claude:
  - `message_log` — that room's recent messages, including the bot's replies. 5 by default and at most 20.
  - `message_search` — the best 40 matches for the `query` argument from the room's whole log.
  - `room_info` — the room's ID, name, and topic.
  - `current_time` — the current UTC time.
  - `random_choice` — a true random pick (real Rust randomness, not an LLM guess).
  - `urban_dictionary` / `imdb_lookup` / `leafly_strain` — outbound HTTP calls to Urban Dictionary, OMDb, and Leafly, respectively (see `src/tools.rs`). `leafly_strain` also reads the strain's Leafly rating and a few of the user reviews Leafly highlights, and posts a sheet of up to six photos of the strain just before the reply: Leafly's product shot, if it has a real one, then the user photos it highlights (see `src/photo_sheet.rs`).
  - `rotten_tomatoes` — a title's Tomatometer, Popcornmeter, critics consensus, director, cast, and trailer, read from Rotten Tomatoes' own search and title pages, since it has no public API. Its poster and a sheet of cast and crew photos, with names and roles, are posted as images just before the reply (see `src/rotten_tomatoes.rs` and `src/cast_sheet.rs`).
  - `web_search` — lets Claude search the web, up to twice per run.
- `args` (optional) — declares expected arguments (name, type, required/default/bounds). Types are `string`, `integer`, `number`, `boolean`, and `array`. Arguments are validated before Claude is ever called.

A `!command`'s args are parsed from the text after its name, without a model: they fill the skill's declared `args` in order, with the last one taking the rest of the line. Lists split on commas or " or ", or on spaces if there are neither.

Thirteen skills ship as worked examples under `skills/`. There are six built-ins too:

- `!help` lists commands, or answers `!help <question>` (see [Help](#help)).
- `!dm @friend … <premise>` starts a role-playing game in its own room, with the bot as
  Dungeon Master (see [Role-playing games](#role-playing-games)).
- `!memories` lists saved facts.
- `!pp` posts an animated GIF of a randomly sized pp, drawn by the bot with no Claude call.
  Its font and emoji images are credited in `assets/pp/README.md`.
- `!stats` posts the status page's numbers with no Claude call: message, reply, and reaction totals, what kinds of replies and messages there are, Claude requests, tokens, cache use, and cost, and this room's activity and most active people. Rooms and people are only named for the room it's asked in.
- `!changelog` posts what changed in the running version, from the `CHANGELOG.md` it was built with, with no Claude call. `!changelog 0.12` or `!changelog 2026-11-01` picks another version.

An unknown command gets a reply pointing to `!help`.

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
| `rt` | `tomatoes`, `rottentomatoes` | `rotten_tomatoes` | reading a site with no API, and a trailing year to pick between titles of the same name |

`random`, `ud`, `imdb`, and `strain` are ported from *gordy*, a separate Matrix bot referenced read-only during development; `imdb` was re-pointed from gordy's scraping approach to the documented OMDb API.

A skill file with bad frontmatter or an unrecognized `tools`/`args` entry is skipped with a warning at startup — everything else still loads. Two skills sharing a `name`, or an alias colliding with another skill's name/alias, fails startup instead, since the bot can't resolve that ambiguity on your behalf.

## How the bot behaves

### When it replies

Who a message is for is decided in code first, whatever the classifier made of it. In a
normal room, the bot replies to:

- A message that @mentions it, replies to one of its messages, or starts with its name
  (`gordy: ...`, `gordy, ...`, `gordy what...`). The name comes from `username`, and
  capitalization doesn't matter.
- A `!command`.
- A follow-up in a thread it's part of (see [Threads](#threads)).
- A greeting to the room, once per room every 5 minutes.
- Other messages, only if unprompted comments are turned on. Set `interjection_chance`
  to enable them. The bot can still decide to pass, and `interjection_cooldown_secs`
  limits them per room.

A bare acknowledgement such as "thanks" or "lol" gets no reply. If it was addressed to the
bot, it gets a 👍 reaction instead.

Each room's messages are handled one at a time, in order. Quick follow-ups from the same
person are answered together, after a 1.5 second wait on messages getting a chat reply.
Messages sent while the bot was offline aren't answered.

The bot shows a typing indicator while it works, except for unprompted comments. A reply
that is only an emoji is sent as a reaction.

### Threads

Replies to commands and conversation go in a thread on the original post, or stay in the
thread they were asked in. Greetings and unprompted comments are plain replies outside
a thread, unless the greeting was said inside one.

Once the bot has posted in a thread, every message there counts as addressed to it,
unless the message mentions someone or replies to someone else. The reply sees up to the
last 50 messages of the thread, plus any images and links in it, however old. Replying
to one of its threaded messages from a client without threads still lands in that
thread. Each follow-up is a full chat reply, so a busy thread costs more than room chat.

`!commands` run in a thread still read room history, not the thread.

### Private conversations

Invite the bot to a room and it joins if `dm_allowed_users` allows the inviter, which by
default means anyone on its own homeserver. Other invites are declined, including
invites from `ignored_users`, and invites that arrived while it was offline are answered
at startup.

| Room | What the bot answers | How |
|---|---|---|
| Just you and the bot | Every message, and a bare acknowledgement gets a 👍 | A normal message |
| Up to 10 members, counting the bot | Everything except messages that mention someone or reply to someone else | A reply, so it's clear who it's answering |
| More than 10 members | Same as a normal room | Same as a normal room |

Neither kind of private chat uses threads unless someone starts one. Only the inviter is
checked, so they vouch for anyone they add. Every message is a full chat reply, so set
`cost_limit_usd`. These rooms are left off the status page, but they are stored in the
database like everything else.

### Chat replies

Chat replies and greetings use `claude-opus-5-5`. A chat reply sees:

- About the last 50 room messages (37 to 61, see [Prompt caching](#prompt-caching)), with
  timestamps, display names, reactions, what links turned out to be, and the bot's own
  replies.
- The thread, the message being replied to, and the current time.
- Saved facts and notes on older chat.
- Linked pages and images, described below.

Chat replies can search the web up to 3 times per reply, at $0.01 per search. They read
pages with `read_page`, up to 6 per reply and 20,000 characters each. It runs in the bot
through the same fetcher and address checks as posted links, and renders pages in the
headless browser when one is set up.
Claude's own web fetch tool, which Reddit and some other sites block, is kept only for
what `read_page` can't read, like PDFs, up to 3 per reply. Greetings and unprompted comments see recent
history, and unprompted comments see facts and notes too, but neither gets web, memory,
or image access.

If Claude refuses a request, the bot uses Anthropic's fallback models where available,
and otherwise says it won't answer. A reply that runs out of tokens is posted with a
note that it was cut off.

### Help

Ask the bot for help in chat, or with `!help <question>`, and it reads this README and the
changelog from the `main` branch on GitHub and answers from them. It decides when it needs
them, using a `read_bot_docs` tool, so `!help <question>` is a normal chat reply. Plain
`!help` still just lists commands.

Every prompt, for chat replies and commands alike, also tells the bot the version it's
running and links to this repository and its changelog, from `version` and `repository` in
`Cargo.toml`. So it can say what it is, where its code is, and what's new without reading
the docs first.

Copies are kept in memory for an hour. If GitHub can't be reached, an expired copy is used,
and with no copy at all the bot says it can't load the docs. Because the docs come from
`main`, a merged change can be described before it's deployed. A question that reads the
README adds about 5,000 tokens of input.

### Memory

Chat replies have `remember` and `forget` tools. Ask in chat, for example
`gordy: remember I'm on US Eastern time`. Facts stay in the room they were learned in,
up to 200 per room and 300 characters each, and `!memories` lists them.

Messages too old for the history are summarized into notes by `claude-haiku-5-5`. The
notes start once 15 older messages exist, refresh after 25 more, and survive restarts.

### Images

When replying in chat, the bot can see:

- An attached image, or one in the message it's replying to.
- The newest image in the thread.
- An image posted in the room's last message, up to 5 minutes old.
- Direct links to JPEG, PNG, GIF, and WebP files, up to 3 per reply.

Attached images and linked files are limited to 3.75MB. A larger attachment falls back to
its thumbnail. Images are sent to Claude but not kept by the bot, and images nobody asks
about cost nothing.

### Links

Pages linked in chat are fetched when posted, up to 3 per message, with no Claude cost.
The title, description, and start of the text are saved with the message, so the bot knows
what was shared without being asked. Replies get the page text for links in the message,
in the message it replies to, in the thread, or posted in the last 5 minutes, up to 3
pages. Other media is only noted by type. Set `fetch_links = false` to turn all of this
off, including image downloads.

Pages are requested with a desktop Chrome user agent, since some sites, like Reddit, send
an empty page to anything that doesn't look like a browser. Sites that check more than the
user agent may still block it. Pages built with JavaScript only have their text when a
headless browser is set up, as described under
[Rendering pages with a headless browser](#rendering-pages-with-a-headless-browser).

Only public addresses on ports 80 and 443 are fetched. Private addresses are refused in the
resolver and on redirects, since anyone in a room can post a URL. Fetching reveals the
bot's IP to the linked site, and links posted in encrypted rooms are fetched and saved too.

### Classification

`claude-haiku-5-5` records each message's intent, sentiment, and entities, using the last
10 messages as context, and spots room greetings. It doesn't run for `!commands` or
images. If it fails, the message is still logged and still answered if addressed.

### Edited messages

An edit updates the stored message in place, so history shows the new text. If the edited
message is for the bot, by being addressed, a command, or a follow-up in its thread, the
bot answers the new version and edits its earlier reply to match, or replies for the first
time if it hadn't before. The model is told what the message said before. Edits that
aren't for the bot only update history, without being classified again, and edits that
don't change the text are ignored. If the new version no longer needs an answer, the
earlier reply stays as it was.

### Role-playing games

`!dm @foo @bar you found a cave in the woods` starts a game. The bot names it from the
premise, like `cave-in-the-woods-x7k2`, creates an encrypted private room with that name,
and invites whoever sent the command and the people they mentioned, up to 6 players.
Only people `dm_allowed_users` allows, and not `ignored_users`, are invited. It replies in
a thread on the `!dm` message, then sets the scene in the game room.

In the game room, each message from a player is their character's action, and the bot
reacts with ✅ to show it was counted. A round is played once every player who has joined
has acted, or 60 seconds after the first action. `!dm go` plays it now, `!dm map` reposts
the map, `!dm status` shows each character's health and what they carry, and `!dm end`
ends the game, for whoever started it. Other people in the room, and other commands,
aren't part of the game.

Each round is one call to `claude-opus-5-5`, with the DM instructions in
`src/dm/prompt.md`, the premise, the last 5 rounds in full and one-line summaries of older
ones, the world as it stands, and the round's actions. Claude plays it with three tools:

- `roll_dice` rolls real dice, like `1d20+3`, so results aren't made up. The rolls are
  listed under the round's story.
- `update_state` changes the world: the map, up to 24 by 16 squares, and the creatures,
  items, and places on it, their health, what they carry, and the party's goal. Changes are checked, and a
  wrong one is refused and explained to Claude, so the map always matches the story.
  Players' characters start with 12 hp, and only their own players move them.
- `end_round` gives the story for the game room and a one-line summary.

The bot draws the map from that state each round, with tiles and sprites from Kenney's
[Tiny Town](https://kenney.nl/assets/tiny-town) and
[Tiny Dungeon](https://kenney.nl/assets/tiny-dungeon) and Clint Bellanger's
[Tiny Creatures](https://clintbellanger.itch.io/tiny-creatures), all CC0 (see
`assets/dm/README.md`). Grass, paths, stone floors, walls, doors, cave mouths, and trees
come from the tile sheets, and water, mountains, and glowing spots are drawn to match.
Claude gives each creature, item, and place a look from about 70 names, like `wolf`,
`skeleton`, `potion`, or `gravestone`, and can change a player's when they say what their
character is. Players start as adventurers in the order they joined. Anything without a
close look is a lettered token: red for monsters, pink for other characters, numbered gold
squares for items, and numbered pink diamonds for places. Creatures that share an initial
are numbered, like G1 and G2 for two grey wolves, and on the full-size map each sprite
wears its label in a badge, coloured like its token, so it matches the key. A key beside the map names every creature with its health, every item and place,
and each kind of ground shown, and a strip under it gives the round and the party's goal.
Each round posts the story in the game room as formatted text, with a "Round 3" heading,
the names of new creatures, items, and places in bold, and the dice rolled as a list, then
the map as its own message under it. The thread on the `!dm` message gets the game's name
and round in bold with the summary, then a smaller map, at the tiles' own size and without
badges, with its key still full size. Text and map are separate messages because image
captions don't show on every client.

Games, players, waiting actions, and every round are kept in the database, so a restart
picks up where it left off. A round that fails keeps its actions for `!dm go` to retry. At
most 3 games run at once, a game ends after 150 rounds, and the cost limit pauses games
like everything else. A round costs about 4¢. Game rooms are left off the status page and
out of `!stats`, and their messages are kept in the game's own tables rather than the
message log.

Every game also keeps a full log in the `game_log` table, for working out afterwards how
the DM played and what each round cost:

- the start, with the premise and party, and each player's action and `!dm` command, with
  who sent it
- each round's prompt: the system prompt and the user turn Claude was given
- every request to Claude and its full response, including token usage and stop reason.
  Only a round's first request repeats the system prompt and tool definitions
- every tool call: each roll, change to the world, and the ending, with what it said back,
  including changes that were refused and why
- each round's story, summary, rolls, and actions, and the world after it
- rounds that failed or were paused, and why, and the game's end

Export a game's log as JSON lines, one entry per line after a first line describing the
game, by its name or number. Without one, it lists the games:

```sh
docker exec matrix-llm-bot matrix-llm-bot export-game                  # list games
docker exec matrix-llm-bot matrix-llm-bot export-game cave-in-the-woods-x7k2 > game.jsonl
```

The export reads the database directly and doesn't log in to Matrix, so it's safe to run
while the bot is up. The log holds everything said in the game room and every prompt, so
treat it like the rest of the database.

### Reactions

Reactions from others are saved and shown in history, including reactions to the bot's own
replies. Removed reactions aren't tracked.

### Prompt caching

Chat replies cache their tools, system prompt, facts, notes, and settled history for 5
minutes. Web searches and tool calls within a reply read from it too. Classification,
greetings, and skills aren't cached, because they're too small or too rare to pay back the
25% write premium.

History moves in steps of 25 messages, because a window sliding one message at a time
would never match the cache. Saving a fact, refreshing the notes, or a new reaction in the
settled history causes a miss on that part. The current time goes in the user turn, never
the system prompt, or nothing would ever match.

### Limits and failures

- `cost_limit_usd` stops Claude calls at a lifetime spending cap, and the bot says so when
  asked for something. Unset, nothing caps spend.
- After a rate limit from Claude, calls pause for `rate_limit_backoff_secs`, and the bot
  says so.
- `ignored_users` are dropped before anything else: no classification, logging, reply,
  or reaction tracking.
- If a reply fails to generate, the bot posts a short apology. An unprompted comment
  just stays quiet.

### Alerts

The people in `notify_users` get a private message, starting with ⚠️, when:

- Claude spend passes 50% and 90% of `cost_limit_usd`, and when it reaches the limit and
  Claude calls stop. Spend already past a share when the bot starts doesn't alert again.
- 5 Claude calls in a row fail, quoting the latest error. Once one works again, a second
  message says so.
- The sync loop stops for good and the bot exits, for its container to restart. That one
  is sent straight away and given 10 seconds. The start notice follows the restart.

Every alert is also logged as a warning, and without `notify_users` that's all that
happens.

## Configuration

Required in `config.toml`, plus `ANTHROPIC_API_KEY` in the environment:

| Option | Meaning |
|---|---|
| `homeserver_url` | The Matrix homeserver, starting with `http://` or `https://` |
| `username`, `password` | The bot account's login. The name people address it by comes from `username` |
| `rooms` | At least one room ID or alias to join at startup |

Optional:

| Option | Default | Meaning |
|---|---|---|
| `device_name` | `matrix-llm-bot` | Device name shown for this login |
| `store_path` | `./data/store` | Matrix session and encryption state |
| `database_path` | `./data/bot.db` | The bot's SQLite database |
| `http_listen_addr` | `127.0.0.1:8080` | Address of the status page |
| `skills_dir` | `./skills` | Directory of commands |
| `omdb_api_key` | none | Enables the `imdb` skill |
| `ignored_users` | none | Users whose messages are dropped entirely |
| `dm_allowed_users` | the bot's homeserver | Who may start a private conversation: `@user:example.org` or `:example.org`. Setting it replaces the default |
| `notify_users` | nobody | Users to message privately: each time the bot starts, with the version it's running and the changelog lines added since their last notice, and with [alerts](#alerts). Written as `@user:example.org`. The bot reuses the private room it shares with each, or creates one. `notify_on_start`, its old name, still works |
| `cost_limit_usd` | none | Lifetime cap on estimated Claude spend |
| `rate_limit_backoff_secs` | `60` | How long to pause after a rate limit |
| `interjection_chance` | `0` | Chance, from 0 to 1, of considering an unprompted comment |
| `interjection_cooldown_secs` | `1800` | Minimum time between unprompted comments in a room |
| `fetch_links` | `true` | Fetch linked pages and images |
| `browser_url` | none | A headless Chrome's DevTools address, like `http://chrome:9222`, for rendering pages. See [Rendering pages with a headless browser](#rendering-pages-with-a-headless-browser) |
| `browser_proxy_url` | none | The bot's proxy as Chrome reaches it, like `http://matrix-llm-bot:3128`. Required with `browser_url` |
| `browser_proxy_listen` | `0.0.0.0:3128` | Where the proxy listens for Chrome |
| `message_log_dir`, `usage_state_path`, `memory_path` | under `./data` | Where older versions kept data, read once for import |

`config.toml.example` has the same options with comments.

## Status page

The status server has **no authentication** and binds to `127.0.0.1` by default. Only widen
`http_listen_addr` deliberately, and put it behind your own auth or reverse proxy if you do.
The page shows:

- Bot details: the version, with links to the changelog and source, the chat and
  classifier models, uptime, whether Claude calls are rate limited, whether links are
  fetched and pages rendered, and who gets notices and alerts.
- Activity counted from the database: message, reply, thread, image, link, and reaction
  totals, per-room and per-person tables, the last 14 days, and breakdowns of reply kinds,
  intents, sentiment, and reactions. Saved facts and database size are shown too.
- Claude usage per reply type: tokens, requests, estimated cost, web searches, cache hit
  rate, and net saving from caching.
- A room browser with each message's classification, reply, and the full prompts sent.
- Every command: the built-ins, then the loaded skills.

Private conversations appear only as a total. The same data is available as JSON from
`/api/status`, `/api/stats`, `/api/usage`, `/api/skills`, and
`/api/rooms/{room_id}/messages?limit=N`.

Costs are estimates from a hardcoded pricing table in `src/usage.rs`, not fetched live. They
include cache reads and writes and web search fees, and models missing from the table
aren't counted.

## Data and security

- **Database:** messages, prompts, reactions, facts, room summaries, private rooms, usage
  totals, what each start notice said, and `!dm` games live in one SQLite database
  (`src/db.rs`). Every message is recorded, whether or not the bot replies, including in
  private conversations and encrypted rooms, and it's stored unencrypted. Messages in game
  rooms are kept as the game's actions instead. The schema migrates automatically at startup.
- **Older data:** files from older versions (`data/messages/*.jsonl`, `memory.json`,
  `usage.json`) are imported once at startup and then ignored, so they can be deleted once
  you're happy with the import.
- **Matrix state:** the session and encryption keys stay in `store_path`, which matrix-sdk
  manages. The saved `session.json` and `device_id` keep restarts on the same device.
- **Device trust:** verification and cross-signing aren't implemented, so the bot trusts
  every device it encounters. See `Bot::new` in `src/bot.rs` before relying on this for
  rooms you care about.
- **Secrets:** `config.toml` and `data/` are git-ignored.
- **Anthropic SDK:** `threatflux-anthropic-sdk` is a third-party community crate, not
  maintained by Anthropic, so pin and review it accordingly. Claude calls use its raw
  request method (`src/llm.rs`), since its typed builders don't cover the newer API fields.

## Known limitations

- Editing a message that was merged into a burst gets it a separate answer, since the
  burst's reply is stored on its last message.
- Message types other than text and images are ignored.
- Links posted before linked images were supported won't load as images from history.
- If Claude rejects an image, for example because of its dimensions, the whole reply fails.

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

- Automatic backups of the `data` directory
- Track removed reactions

### Cost and performance

- Per-user rate limits
- Cheaper classification, by skipping messages that clearly don't need it or running it after the reply
- Pruning old prompts from the database, which are most of its size
- Full-text search with SQLite FTS5, as history grows
- Fuller cost tracking: spend per day and per person, on the status page and in `!stats`

### Security and admin

- Login for the status page
- Device verification instead of trusting all devices
- Stronger limits on prompt injection from web results and room text
- Admin commands to ignore a user or reload skills from chat

### Deployment

- Keep the bot's service definition in the deploy config
- Deploy pinned tags or commits, not `main`
- CI for tests and clippy on every push
- Alerts for sync failures, Claude errors, and spend
- Replace the third-party Anthropic SDK with a small HTTP client, now that calls are raw HTTP
