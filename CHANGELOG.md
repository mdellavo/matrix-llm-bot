# Changelog

Notable changes to matrix-llm-bot, built from the git history. Each version covers
one day of commits, newest first.

## 0.16.0 - 2026-11-06

### Added

- `!changelog [version]`: what changed in the running version, or in another version or on a day
- `!stats`: the status page's numbers in chat, with this room's activity and the bot's Claude usage and cost
- `notify_on_start`: users the bot messages privately each time it starts, with the version it's running

### Changed

- `!rt` also gives the rating, runtime or seasons, genres, director, cast with their characters, and a trailer link, and posts the poster and a sheet of cast and crew photos with names and roles before its reply
- `!strain` posts a grid of photos of the strain from Leafly before its reply, in place of a link to one image
- `!strain` also gives the strain's Leafly rating and quotes from its highlighted user reviews
- The bot knows which version it's running and where its code and changelog are, in chat replies and commands alike, so it can answer what it is and what's new

## 0.15.0 - 2026-11-05

### Added

- `!pp`, carried over from the old bot: an animated GIF of a randomly sized pp, drawn by the bot and posted as an image
- `!rt <title> [year]`: a movie's or show's Rotten Tomatoes scores and critics consensus. When titles share a name, the most-reviewed one is chosen, and a year picks another

### Changed

- Pages read for replies are scrolled to the bottom before being read, so comments and other content that load on scroll are included
- `docker-compose.example.yaml` with the bot and the headless browser, and the browser options in the README's configuration table
- Each page read for a reply is logged, and the headless browser's protocol warnings are quiet by default

## 0.14.0 - 2026-11-04

### Added

- Headless browser rendering: with `browser_url` set, linked pages and pages read for replies are rendered in a headless Chrome so their JavaScript runs. Chrome reaches the internet only through a proxy in the bot that applies the usual address checks
- Edited messages: an edit updates the stored message, and if it's for the bot, the bot answers the new version and edits its earlier reply to match

### Changed

- Chat replies read pages with their own `read_page` tool, which uses the bot's fetcher and browser user agent, instead of Claude's web fetch, which Reddit blocks. Web fetch is kept for PDFs
- Linked pages are fetched with a desktop browser user agent, so sites like Reddit return their real page instead of an empty one

## 0.13.0 - 2026-11-02

### Added

- Help from the docs: asked about itself in chat or with `!help <question>`, the bot reads its README and changelog from GitHub and answers from them

## 0.12.0 - 2026-11-01

### Added

- Threads: replies to commands and conversation go in a thread on the original post
- Follow-ups in a thread are answered without addressing the bot, using the thread's messages, images, and links
- Status page activity stats: totals, rooms, people, the last 14 days, reply kinds, intents, sentiment, reactions, database size, and average cost per request and reply
- Replies can see images linked directly in chat, not just attached ones
- Private conversations with one or more people: the bot accepts invites from allowed users (`dm_allowed_users`) and answers without being addressed, and these rooms stay off the status page

### Changed

- Chat replies use prompt caching, and the history they see moves in steps of 25 messages to make that work
- Usage and cost estimates include cache reads and writes, and the status page shows hit rate and net saving
- Burst merging also applies to quick follow-ups in the same thread
- The README now covers every feature and config option, and lists known limitations

## 0.11.0 - 2026-10-31

### Added

- Long-term memory: chat replies can save and delete facts, kept per room, and `!memories` lists them
- Reading links from the conversation with web fetch
- Link awareness: pages posted in chat are fetched and logged, limited to public addresses (`fetch_links`)
- Image understanding for attached, replied-to, and just-posted images
- Reactions: lone-emoji replies are posted as reactions, addressed thanks get a 👍, and people's reactions appear in history
- `!search` over the room's whole log, and a `web_search` tool for skills, used by `!define`
- Optional unprompted comments, off by default (`interjection_chance`, `interjection_cooldown_secs`)
- Notes on older chat, which persist across restarts along with saved facts
- Display names in history and logs
- Web search whenever facts may be wrong or stale, capped at 3 per reply

### Changed

- Models: `claude-opus-5-5` for chat and greetings, `claude-sonnet-5-5` for skills that need judgment, and `claude-haiku-5-5` for classification and everything else
- Addressing is decided in code first: mentions, replies to the bot, and messages starting with its name are answered whatever the classifier decided
- `!commands` are parsed without a model
- Chat replies see the last 50 messages, with timestamps, plus the message being replied to
- Rewrote the chat and greeting prompts
- Replies are sent as real Matrix replies
- Each room's messages are handled in order, and quick follow-ups from the same person are answered together
- All Claude calls go through one layer (`src/llm.rs`)
- Messages, reactions, facts, room summaries, and usage now live in one SQLite database (`database_path`), with older files imported automatically on startup
- Status page numbers use commas, costs round to cents, and uptime shows days

### Fixed

- The bot couldn't see its own replies, so it couldn't follow up on what it had said
- Skills couldn't see the bot's replies either
- Directed greetings could go unanswered during the greeting cooldown
- Paused and cut-off replies were posted incomplete, and refusals went unhandled
- Narration before a search was posted, and cited sentences were split apart
- Messages were lost from history when classification failed
- Updated out-of-date model prices

## 0.10.0 - 2026-08-17

### Added

- Chat and greeting replies know the bot's identity and the room's name, topic, and current time

## 0.9.0 - 2026-08-07

### Changed

- Chat web search is limited to explicit user requests

## 0.8.0 - 2026-07-31

### Fixed

- `classify_message` failing with a 400 error, "tool_choice.type: Field required"

## 0.7.0 - 2026-07-30

### Added

- Claude API usage persists across restarts
- Throttling after a rate limit
- Hard cost limit for Claude API calls (`cost_limit_usd`)
- Web search tool for chat
- Web search usage breakdown on the status page

## 0.6.0 - 2026-07-29

### Changed

- Switched to `threatflux-anthropic-sdk` with rustls and dropped vendored OpenSSL

## 0.5.0 - 2026-07-25

### Added

- Ignored-users config
- Dedicated model for chat and greeting replies
- Reply logging
- Classification grounded in recent room history
- Classification and reply prompts recorded and shown on the status page

## 0.4.0 - 2026-07-22

### Fixed

- Ambiguous speaker attribution in chat history context

## 0.3.0 - 2026-07-21

### Added

- Usage cost estimates
- Greeting behavior ported from the older gordy bot

### Changed

- Shorter greeting replies and a simpler README

### Fixed

- Docker build failing on missing pkg-config and OpenSSL

## 0.2.0 - 2026-07-20

### Added

- Commands ported from the older gordy bot
- Claude usage tracking
- Claude-generated chat replies
- Dockerfile for building and deploying with Docker Compose

### Changed

- User experience polish

## 0.1.0 - 2026-07-18

### Added

- Initial release: Matrix bot with Claude-powered message classification and skills
