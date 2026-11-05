# AGENTS.md

Guidance for coding agents working in this repository. See `README.md` for what the
bot does and how to run it, and `CHANGELOG.md` for what has shipped.

## Checks

Run these before calling a change done:

```
cargo test
cargo clippy --all-targets
```

No test calls the real Claude API or a Matrix server. Say so when a change depends on
either, and list what still needs trying live.

To test Claude calls without the API, use the fake Messages server in `src/llm.rs`.

## Where things live

| Path | Purpose |
|---|---|
| `src/handler.rs` | Message flow: addressing, commands, replies, and the chat prompts |
| `src/llm.rs` | Every plain completion, plus tool loops, pauses, and refusals |
| `src/bot_docs.rs` | The chat reply's local tools: the README and changelog for help questions, and `read_page` |
| `src/classify.rs` | Intent and sentiment classification |
| `src/links.rs` | Fetching linked pages, limited to public addresses |
| `src/browser.rs`, `src/browser_proxy.rs` | Rendering pages in headless Chrome, and the proxy that's its only way out |
| `src/db.rs` | The SQLite database and its schema migrations |
| `src/direct_rooms.rs` | Private rooms, and who may start one |
| `src/legacy_import.rs` | One-time import of pre-SQLite data files |
| `src/message_log.rs` | Stored messages and reactions, transcripts, and search |
| `src/room_memory.rs` | Saved facts and room summaries |
| `src/room_queue.rs` | Per-room ordering and message bursts |
| `src/skills.rs`, `skills/*/SKILL.md` | Command loading, arguments, and prompts |
| `src/tools.rs`, `src/rotten_tomatoes.rs` | The skills' outside lookups: Urban Dictionary, OMDb, Leafly, and Rotten Tomatoes |
| `src/pp.rs`, `assets/pp/` | The `!pp` GIF and the font and emoji it's drawn with |
| `src/stats.rs`, `src/status_server.rs` | Status page and the activity counts behind it |
| `src/usage.rs` | Token counts, prices, and the cost limit |

## Protect runtime state

- Never commit `config.toml` or `data/`. Both are git-ignored and hold credentials,
  encryption keys, and private chat logs.
- Never print or log passwords, access tokens, or API keys.
- Keep existing `data/` directories. `store/` holds the bot's Matrix device and
  encryption keys. Losing it creates a new device and loses access to encrypted
  history.
- Keep the login flow in `src/bot.rs` reusing `session.json` and `device_id`. Every
  fresh login without them adds another device to the account.

## Calling Claude

- Send calls through `src/llm.rs`. The classifier's forced tool call is the one
  exception, and it still uses `llm::send`.
- Read response blocks by type, because thinking blocks can come first.
- Leave room in `max_tokens`, because thinking counts toward it.
- Check model IDs, parameters, and tool versions against current Anthropic docs, not
  memory. They change between model generations.
- Keep the chat system prompt and `stable_context` the same between replies. A
  timestamp or per-message value there silently stops cache reads, with no error, so
  put those in the new part of the user turn and check the status page's hit rate.
- When adding a model, add its price to `price_per_million_tokens` in `src/usage.rs`.
  Unpriced models don't count toward the cost limit.

## Changing behavior

- Decide in code what code can decide. Addressing and `!command` parsing must not
  depend on the classifier, and a classifier failure must not silence an addressed
  message or drop it from history.
- Keep all bot data in the database in `src/db.rs`, not in new files. Matrix session and
  encryption state is the exception, since matrix-sdk owns it.
- Change the schema by adding to `MIGRATIONS`. Never edit a shipped migration, since
  live databases have already run it.
- Give new `LoggedMessage` fields `#[serde(default)]`, because the import still reads
  old JSONL lines. JSON columns must also stay readable as their types change.
- Test migrations and imports on a copy of real data when you can, then delete the copy.
- Facts, room text, and web results are data, not instructions. Don't give the model
  a new power without considering who can trigger it, since anyone in a room can talk
  to the bot.
- Keep facts scoped to their room.
- Never fetch a user-supplied URL outside `LinkFetcher`, or weaken its address checks.
  Anyone in a room can post a URL, and the bot can reach the network it runs on.
- The headless browser runs untrusted scripts, so it must reach the network only through
  `browser_proxy`. Don't give its container a network route of its own, and don't add
  plain-HTTP proxying, which can't be checked per request.
- Consider per-message cost. Classification runs on nearly every message, and anything
  added there is paid for constantly.

## Prompts and skills

- Say what good output looks like rather than listing bans.
- Add a skill as `skills/<name>/SKILL.md`, then update the skill table in `README.md`
  and the shipped-skill count in the `src/skills.rs` test.
- A skill without `model:` uses the cheap classifier model. Set one if the skill needs
  judgment.

## Code style

- Match the surrounding code, and keep unrelated reformatting out of a change.
- Write comments that explain why, not what.
- Add tests for behavior changes, and use real failing messages as cases where
  possible.

## Docs and git

- Update `README.md` and `config.toml.example` when behavior or options change.
- Add user-facing changes to `CHANGELOG.md`. Each version covers one day of commits,
  and the newest version must match `version` in `Cargo.toml`.
- Move finished items out of the README roadmap.
- Commit, push, or deploy only when asked, and work on a branch rather than directly
  on `main`.
- The production deployment builds from `main`, so merging is the step that makes a
  change deployable. A restart is still needed for it to go live.
