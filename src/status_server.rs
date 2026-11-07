use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::get,
};
use matrix_sdk::Client;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::db::Database;
use crate::direct_rooms::DirectRooms;
use crate::message_log::MessageLogger;
use crate::skills::SkillRegistry;
use crate::throttle::Throttle;
use crate::tools::ToolClients;
use crate::usage::UsageTracker;

const DEFAULT_MESSAGES_LIMIT: usize = 20;
const MAX_MESSAGES_LIMIT: usize = 200;

/// Shared state for the status/debugging HTTP server. Read-only from the server's
/// perspective — it queries the same `Client`, `MessageLogger`, `SkillRegistry`,
/// `ToolClients`, and `UsageTracker` the bot itself uses, rather than duplicating
/// any state.
pub struct AppState {
    pub start_time: Instant,
    pub client: Client,
    pub message_log: Arc<MessageLogger>,
    pub skills: Arc<SkillRegistry>,
    pub tool_clients: Arc<ToolClients>,
    pub usage_tracker: Arc<UsageTracker>,
    pub rate_limit: Arc<Throttle>,
    pub direct_rooms: Arc<DirectRooms>,
    pub database: Arc<Database>,
    pub homeserver_url: String,
    /// See `Config::fetch_links`.
    pub fetch_links: bool,
    /// Whether a headless browser is set up (`Config::browser_url`).
    pub browser_rendering: bool,
    /// See `Config::notify_on_start`.
    pub notify_on_start: Vec<String>,
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(api_status))
        .route("/api/rooms/{room_id}/messages", get(api_room_messages))
        .route("/api/skills", get(api_skills))
        .route("/api/usage", get(api_usage))
        .route("/api/stats", get(api_stats))
        .with_state(state)
}

/// Binds and serves the status/debugging HTTP server until it errors or is aborted.
///
/// The page and its JSON API expose logged message content with no authentication —
/// see `Config::http_listen_addr`'s docs for why the default is loopback-only.
pub async fn serve(addr: SocketAddr, state: Arc<AppState>) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind status/debug HTTP server to {addr}"))?;

    info!(%addr, "status/debug HTTP server listening");
    axum::serve(listener, router(state)).await.context("status/debug HTTP server failed")?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    bot_user_id: Option<String>,
    homeserver_url: String,
    version: &'static str,
    repository: &'static str,
    chat_model: &'static str,
    classifier_model: &'static str,
    fetch_links: bool,
    browser_rendering: bool,
    notify_on_start: Vec<String>,
    uptime_seconds: u64,
    /// Whether an OMDb API key is configured — the `imdb` skill needs one to work.
    omdb_configured: bool,
    /// Seconds left in an active Claude API rate-limit cooldown (see
    /// `Throttle`) — `None` when the bot isn't currently throttled.
    rate_limited_seconds_remaining: Option<u64>,
    rooms: Vec<RoomSummary>,
}

#[derive(Debug, Serialize)]
struct RoomSummary {
    room_id: String,
    display_name: Option<String>,
}

/// Private conversations stay off this page, since it has no login. If that can't be
/// checked, the room is treated as private.
fn is_private(state: &AppState, room_id: &str) -> bool {
    state.direct_rooms.contains(room_id).unwrap_or(true)
}

async fn api_status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let rooms = state
        .client
        .joined_rooms()
        .into_iter()
        .filter(|room| !is_private(&state, room.room_id().as_str()))
        .map(|room| RoomSummary {
            room_id: room.room_id().to_string(),
            display_name: room.name(),
        })
        .collect();

    Json(StatusResponse {
        bot_user_id: state.client.user_id().map(|id| id.to_string()),
        homeserver_url: state.homeserver_url.clone(),
        version: env!("CARGO_PKG_VERSION"),
        repository: env!("CARGO_PKG_REPOSITORY"),
        chat_model: crate::handler::CHAT_MODEL,
        classifier_model: crate::classify::MODEL,
        fetch_links: state.fetch_links,
        browser_rendering: state.browser_rendering,
        notify_on_start: state.notify_on_start.clone(),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        omdb_configured: state.tool_clients.has_omdb_key(),
        rate_limited_seconds_remaining: state.rate_limit.remaining().map(|remaining| remaining.as_secs()),
        rooms,
    })
}

#[derive(Debug, Deserialize)]
struct MessagesQuery {
    limit: Option<usize>,
}

async fn api_room_messages(
    State(state): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    Query(query): Query<MessagesQuery>,
) -> impl IntoResponse {
    if is_private(&state, &room_id) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let limit = query.limit.unwrap_or(DEFAULT_MESSAGES_LIMIT).clamp(1, MAX_MESSAGES_LIMIT);

    match state.message_log.recent_with_prompts(&room_id, limit) {
        Ok(entries) => Json(entries).into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

#[derive(Debug, Serialize)]
struct SkillSummary {
    name: String,
    description: String,
    usage: Option<String>,
    tools: Vec<String>,
    aliases: Vec<String>,
    model: Option<String>,
    /// Handled in code, like `!stats`, rather than by a skill file.
    builtin: bool,
}

/// Every command: the built-ins, then the loaded skills.
async fn api_skills(State(state): State<Arc<AppState>>) -> Json<Vec<SkillSummary>> {
    let builtins = crate::handler::BUILTIN_COMMANDS.iter().map(|command| SkillSummary {
        name: command.name.to_string(),
        description: command.description.to_string(),
        usage: command.usage.map(String::from),
        tools: Vec::new(),
        aliases: Vec::new(),
        model: None,
        builtin: true,
    });
    let skills = builtins
        .chain(state.skills.list().into_iter().map(|skill| SkillSummary {
            name: skill.name.clone(),
            description: skill.description.clone(),
            usage: skill.usage.clone(),
            tools: skill.tools.clone(),
            aliases: skill.aliases.clone(),
            model: skill.model.clone(),
            builtin: false,
        }))
        .collect();

    Json(skills)
}

/// Claude API token/request usage over the bot's whole life, kept in the database (see
/// `UsageTracker`): overall totals plus a breakdown by label, such as `"classify"`,
/// `"chat"`, or a skill's name.
async fn api_usage(State(state): State<Arc<AppState>>) -> Json<crate::usage::UsageSnapshot> {
    Json(state.usage_tracker.snapshot())
}

/// Activity counted from the database. See `crate::stats`.
async fn api_stats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match crate::stats::collect(&state.database, chrono::Utc::now().timestamp_millis()) {
        Ok(stats) => Json(stats).into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>matrix-llm-bot — status</title>
<style>
  :root { color-scheme: light dark; }
  body { font-family: system-ui, sans-serif; margin: 2rem; max-width: 960px; }
  h1 { font-size: 1.25rem; }
  dl { display: grid; grid-template-columns: max-content 1fr; gap: 0.25rem 1rem; }
  dt { font-weight: 600; }
  dd { margin: 0; }
  table { border-collapse: collapse; width: 100%; margin-top: 1rem; font-size: 0.85rem; }
  th, td { border: 1px solid #8884; padding: 0.35rem 0.5rem; text-align: left; vertical-align: top; }
  th { background: #8881; }
  select, input, button { font: inherit; }
  .controls { margin: 1rem 0; display: flex; gap: 0.5rem; align-items: center; }
  .details-row td { background: rgba(136, 136, 136, 0.06); }
  .details-block { margin-bottom: 0.75rem; }
  .details-block:last-child { margin-bottom: 0; }
  .details-block pre { white-space: pre-wrap; word-break: break-word; max-height: 300px; overflow: auto; background: #8881; padding: 0.5rem; border-radius: 4px; margin: 0.25rem 0; }
  .details-block summary { cursor: pointer; }
  .tiles { display: grid; grid-template-columns: repeat(auto-fill, minmax(9.5rem, 1fr)); gap: 0.5rem; margin: 1rem 0; }
  .tile { border: 1px solid #8884; border-radius: 6px; padding: 0.5rem 0.75rem; }
  .tile b { display: block; font-size: 1.35rem; font-weight: 600; font-variant-numeric: tabular-nums; }
  .tile span { font-size: 0.8rem; opacity: 0.75; }
  .columns { display: grid; grid-template-columns: repeat(auto-fit, minmax(16rem, 1fr)); gap: 1rem; }
  .columns h3 { font-size: 0.95rem; margin: 1rem 0 0; }
  td.num, th.num { text-align: right; font-variant-numeric: tabular-nums; }
  .table-scroll { overflow-x: auto; }
</style>
</head>
<body>
  <h1>matrix-llm-bot — status &amp; debug</h1>
  <dl>
    <dt>Bot user</dt><dd id="bot-user">…</dd>
    <dt>Version</dt><dd id="version">…</dd>
    <dt>Homeserver</dt><dd id="homeserver">…</dd>
    <dt>Chat model</dt><dd id="chat-model">…</dd>
    <dt>Classifier model</dt><dd id="model">…</dd>
    <dt>Uptime</dt><dd id="uptime">…</dd>
    <dt>OMDb configured</dt><dd id="omdb-configured">…</dd>
    <dt>Link fetching</dt><dd id="fetch-links">…</dd>
    <dt>Page rendering</dt><dd id="browser-rendering">…</dd>
    <dt>Start notices</dt><dd id="notify-on-start">…</dd>
    <dt>Claude API rate limit</dt><dd id="rate-limited">…</dd>
  </dl>

  <h2>Activity</h2>
  <div class="tiles" id="stat-tiles"></div>
  <p id="stats-note"></p>
  <div class="table-scroll"><table>
    <thead>
      <tr><th>Room</th><th class="num">Messages</th><th class="num">Replies</th><th class="num">People</th><th class="num">Threads</th><th>Last message</th></tr>
    </thead>
    <tbody id="stats-rooms"></tbody>
  </table></div>
  <div class="columns">
    <div>
      <h3>Last 14 days (UTC)</h3>
      <table>
        <thead><tr><th>Day</th><th class="num">Messages</th><th class="num">Replies</th></tr></thead>
        <tbody id="stats-daily"></tbody>
      </table>
    </div>
    <div>
      <h3>Most active people</h3>
      <table>
        <thead><tr><th>Person</th><th class="num">Messages</th><th class="num">Replies</th></tr></thead>
        <tbody id="stats-people"></tbody>
      </table>
    </div>
    <div><h3>Replies by kind</h3><table><tbody id="stats-reply-kinds"></tbody></table></div>
    <div><h3>Intents</h3><table><tbody id="stats-intents"></tbody></table></div>
    <div><h3>Sentiment</h3><table><tbody id="stats-sentiments"></tbody></table></div>
    <div><h3>Reactions</h3><table><tbody id="stats-reactions"></tbody></table></div>
  </div>

  <h2>Claude API usage</h2>
  <dl>
    <dt>Total tokens</dt><dd id="usage-total-tokens">…</dd>
    <dt>Input tokens</dt><dd id="usage-input-tokens">…</dd>
    <dt>Output tokens</dt><dd id="usage-output-tokens">…</dd>
    <dt>Cache read tokens</dt><dd id="usage-cache-read">…</dd>
    <dt>Cache write tokens</dt><dd id="usage-cache-write">…</dd>
    <dt>Cache hit rate</dt><dd id="usage-cache-rate">…</dd>
    <dt>Saved by caching</dt><dd id="usage-cache-saved">…</dd>
    <dt>Requests</dt><dd id="usage-requests">…</dd>
    <dt>Estimated cost</dt><dd id="usage-cost">…</dd>
    <dt>Average per request</dt><dd id="usage-cost-per-request">…</dd>
    <dt>Average per reply</dt><dd id="usage-cost-per-reply">…</dd>
    <dt>Cost limit</dt><dd id="usage-cost-limit">…</dd>
    <dt>Web searches</dt><dd id="usage-web-searches">…</dd>
    <dt>Web search cost</dt><dd id="usage-web-search-cost">…</dd>
  </dl>
  <p id="usage-unpriced-note" style="display: none;"></p>
  <p id="usage-cost-limit-note" style="display: none;"></p>
  <table>
    <thead>
      <tr><th>Label</th><th>Input tokens</th><th>Output tokens</th><th>Total tokens</th><th>Cache read</th><th>Requests</th><th>Est. cost</th><th>Web searches</th><th>Web search cost</th></tr>
    </thead>
    <tbody id="usage-body"></tbody>
  </table>

  <div class="controls">
    <label for="room-select">Room</label>
    <select id="room-select"></select>
    <label for="limit">Limit</label>
    <input id="limit" type="number" value="20" min="1" max="200" style="width: 5em;">
    <button id="refresh">Refresh</button>
  </div>

  <table>
    <thead>
      <tr><th>Logged at</th><th>Sender</th><th>Intent</th><th>Sentiment</th><th>Reply?</th><th>Body</th><th>Details</th></tr>
    </thead>
    <tbody id="messages-body"></tbody>
  </table>

  <h2>Commands</h2>
  <table>
    <thead>
      <tr><th>Name</th><th>Aliases</th><th>Usage</th><th>Tools</th><th>Model</th><th>Description</th></tr>
    </thead>
    <tbody id="skills-body"></tbody>
  </table>

<script>
function escapeHtml(str) {
  const div = document.createElement('div');
  div.textContent = str ?? '';
  return div.innerHTML;
}

function formatUptime(seconds) {
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = Math.floor(seconds % 60);
  return d > 0 ? `${formatCount(d)}d ${h}h ${m}m` : `${h}h ${m}m ${s}s`;
}

function formatCount(value) {
  return Number(value).toLocaleString('en-US');
}

// A prompt (system + user turn) actually sent to Claude, rendered as two
// collapsible <details> blocks — or a note when no prompt was recorded, either
// because the log line predates prompt logging or no Claude call was made
// (e.g. a canned "unknown command" reply).
function renderPrompt(prompt) {
  if (!prompt) return '<p><em>(no prompt recorded)</em></p>';
  return `
    <details><summary>System prompt</summary><pre>${escapeHtml(prompt.system)}</pre></details>
    <details><summary>User turn</summary><pre>${escapeHtml(prompt.user)}</pre></details>
  `;
}

// The classification and response prompts/output for one message, shown in
// the details row a "Details" button toggles open — the raw classify JSON is
// collapsed behind its own link since it's rarely needed and can be sizable.
function renderMessageDetails(m, idx) {
  const rawJsonId = `raw-json-${idx}`;
  const rawJsonBlock = m.classify_raw_json
    ? `<button type="button" class="show-raw-json" data-target="${rawJsonId}">Show raw classify JSON</button>
       <pre class="raw-json" id="${rawJsonId}" style="display: none;">${escapeHtml(JSON.stringify(m.classify_raw_json, null, 2))}</pre>`
    : '<p><em>(no raw classify JSON recorded)</em></p>';

  let responseBlock;
  if (m.response == null) {
    responseBlock = '<p><em>(no response generated)</em></p>';
  } else {
    responseBlock = `<pre>${escapeHtml(m.response)}</pre>${
      m.response_prompt ? renderPrompt(m.response_prompt) : '<p><em>(canned reply — no Claude call)</em></p>'
    }`;
  }

  return `
    <div class="details-block">
      <strong>Classification prompt</strong>
      ${renderPrompt(m.classify_prompt)}
      ${rawJsonBlock}
    </div>
    <div class="details-block">
      <strong>Response${m.response_label ? ` (${escapeHtml(m.response_label)})` : ''}</strong>
      ${responseBlock}
    </div>
  `;
}

async function loadMessages(roomId) {
  if (!roomId) return;
  const limit = document.getElementById('limit').value || 20;
  const res = await fetch(`/api/rooms/${encodeURIComponent(roomId)}/messages?limit=${encodeURIComponent(limit)}`);
  const tbody = document.getElementById('messages-body');
  tbody.innerHTML = '';
  if (!res.ok) {
    tbody.innerHTML = `<tr><td colspan="7">Failed to load messages: ${escapeHtml(await res.text())}</td></tr>`;
    return;
  }
  const messages = await res.json();
  messages.forEach((m, idx) => {
    const row = document.createElement('tr');
    row.innerHTML = `
      <td>${escapeHtml(m.logged_at)}</td>
      <td>${escapeHtml(m.sender)}</td>
      <td>${escapeHtml(m.analysis.intent)}</td>
      <td>${escapeHtml(m.analysis.sentiment)}</td>
      <td>${m.analysis.requires_response ? 'yes' : 'no'}</td>
      <td>${escapeHtml(m.body)}</td>
      <td><button type="button" class="toggle-details" data-target="details-${idx}">Details</button></td>
    `;
    tbody.appendChild(row);

    const detailsRow = document.createElement('tr');
    detailsRow.className = 'details-row';
    detailsRow.id = `details-${idx}`;
    detailsRow.style.display = 'none';
    detailsRow.innerHTML = `<td colspan="7">${renderMessageDetails(m, idx)}</td>`;
    tbody.appendChild(detailsRow);
  });
}

// Delegated on the (repeatedly rebuilt) tbody itself, rather than per-button,
// so listeners never need re-attaching after `loadMessages` clears/repopulates it.
document.getElementById('messages-body').addEventListener('click', (e) => {
  const toggleBtn = e.target.closest('.toggle-details');
  if (toggleBtn) {
    const row = document.getElementById(toggleBtn.dataset.target);
    if (row) row.style.display = row.style.display === 'none' ? '' : 'none';
    return;
  }
  const rawLink = e.target.closest('.show-raw-json');
  if (rawLink) {
    e.preventDefault();
    const pre = document.getElementById(rawLink.dataset.target);
    if (pre) pre.style.display = pre.style.display === 'none' ? '' : 'none';
  }
});

async function loadSkills() {
  const res = await fetch('/api/skills');
  const tbody = document.getElementById('skills-body');
  tbody.innerHTML = '';
  if (!res.ok) {
    tbody.innerHTML = `<tr><td colspan="6">Failed to load commands: ${escapeHtml(await res.text())}</td></tr>`;
    return;
  }
  const skills = await res.json();
  if (skills.length === 0) {
    tbody.innerHTML = '<tr><td colspan="6">(no commands loaded)</td></tr>';
    return;
  }
  for (const skill of skills) {
    const row = document.createElement('tr');
    row.innerHTML = `
      <td>${escapeHtml(skill.name)}</td>
      <td>${escapeHtml((skill.aliases ?? []).join(', '))}</td>
      <td>${escapeHtml(skill.usage ?? '')}</td>
      <td>${skill.builtin ? '<i>built-in</i>' : escapeHtml((skill.tools ?? []).join(', '))}</td>
      <td>${escapeHtml(skill.model ?? '')}</td>
      <td>${escapeHtml(skill.description)}</td>
    `;
    tbody.appendChild(row);
  }
}

function formatCost(usd) {
  return usd.toLocaleString('en-US', { style: 'currency', currency: 'USD' });
}

// Costs per call are often under a cent, where rounding to cents would show $0.00.
function formatSmallCost(usd) {
  return usd.toLocaleString('en-US', { style: 'currency', currency: 'USD', maximumFractionDigits: 4 });
}

function formatBytes(bytes) {
  const units = ['B', 'KB', 'MB', 'GB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toLocaleString('en-US', { maximumFractionDigits: 1 })} ${units[unit]}`;
}

function formatTime(ms) {
  return ms == null ? 'never' : new Date(ms).toLocaleString();
}

function formatPercent(part, whole) {
  return whole > 0 ? `${(part / whole * 100).toLocaleString('en-US', { maximumFractionDigits: 1 })}%` : '—';
}

function fillRows(id, columns, rows, render) {
  const tbody = document.getElementById(id);
  tbody.innerHTML = rows.length > 0
    ? rows.map(render).join('')
    : `<tr><td colspan="${columns}">(nothing yet)</td></tr>`;
}

function countRow(whole) {
  return (entry) => `<tr><td>${escapeHtml(entry.name)}</td><td class="num">${formatCount(entry.count)}</td><td class="num">${formatPercent(entry.count, whole)}</td></tr>`;
}

function sumCounts(entries) {
  return entries.reduce((sum, entry) => sum + entry.count, 0);
}

let roomNames = {};
let replyCount = null;
let lastCost = null;

function showCostPerReply() {
  if (replyCount == null || lastCost == null) return;
  document.getElementById('usage-cost-per-reply').textContent = replyCount > 0 ? formatSmallCost(lastCost / replyCount) : '—';
}

async function loadStats() {
  const res = await fetch('/api/stats');
  const tiles = document.getElementById('stat-tiles');
  if (!res.ok) {
    tiles.textContent = `Failed to load stats: ${await res.text()}`;
    return;
  }
  const stats = await res.json();
  const t = stats.totals;

  const tileValues = [
    ['Messages', formatCount(t.messages)],
    ['Last 24 hours', formatCount(t.messages_24h)],
    ['Last 7 days', formatCount(t.messages_7d)],
    ['Replies sent', formatCount(t.replies)],
    ['Reply rate', formatPercent(t.replies, t.messages)],
    ['People', formatCount(t.people)],
    ['Threads', formatCount(t.threads)],
    ['Images', formatCount(t.images)],
    ['Links', formatCount(t.links)],
    ['Reactions', formatCount(t.reactions)],
    ['Saved facts', formatCount(t.facts)],
    ['Room summaries', formatCount(t.room_summaries)],
    ['Private chats', formatCount(t.private_conversations)],
    ['Database size', formatBytes(stats.database_bytes)],
  ];
  tiles.innerHTML = tileValues.map(([label, value]) => `<div class="tile"><b>${escapeHtml(value)}</b><span>${escapeHtml(label)}</span></div>`).join('');

  document.getElementById('stats-note').textContent =
    `First message ${formatTime(t.first_message_ms)}. Last message ${formatTime(t.last_message_ms)}. Private chats are counted above and left out of everything else here.`;

  fillRows('stats-rooms', 6, stats.rooms, (room) => `<tr>
    <td>${escapeHtml(roomNames[room.room_id] ?? room.room_id)}</td>
    <td class="num">${formatCount(room.messages)}</td>
    <td class="num">${formatCount(room.replies)}</td>
    <td class="num">${formatCount(room.people)}</td>
    <td class="num">${formatCount(room.threads)}</td>
    <td>${escapeHtml(formatTime(room.last_message_ms))}</td>
  </tr>`);
  fillRows('stats-daily', 3, stats.daily, (day) => `<tr><td>${escapeHtml(day.day)}</td><td class="num">${formatCount(day.messages)}</td><td class="num">${formatCount(day.replies)}</td></tr>`);
  fillRows('stats-people', 3, stats.people, (person) => `<tr><td>${escapeHtml(person.name)}</td><td class="num">${formatCount(person.messages)}</td><td class="num">${formatCount(person.replies)}</td></tr>`);
  fillRows('stats-reply-kinds', 3, stats.reply_kinds, countRow(sumCounts(stats.reply_kinds)));
  fillRows('stats-intents', 3, stats.intents, countRow(sumCounts(stats.intents)));
  fillRows('stats-sentiments', 3, stats.sentiments, countRow(sumCounts(stats.sentiments)));
  fillRows('stats-reactions', 3, stats.reactions, countRow(sumCounts(stats.reactions)));

  replyCount = t.replies;
  showCostPerReply();
}

async function loadUsage() {
  const res = await fetch('/api/usage');
  const tbody = document.getElementById('usage-body');
  tbody.innerHTML = '';
  if (!res.ok) {
    tbody.innerHTML = `<tr><td colspan="9">Failed to load usage: ${escapeHtml(await res.text())}</td></tr>`;
    return;
  }
  const usage = await res.json();
  const cachedInput = usage.cache_read_tokens + usage.cache_write_tokens;
  document.getElementById('usage-total-tokens').textContent = formatCount(usage.input_tokens + usage.output_tokens + cachedInput);
  document.getElementById('usage-cache-read').textContent = formatCount(usage.cache_read_tokens);
  document.getElementById('usage-cache-write').textContent = formatCount(usage.cache_write_tokens);
  document.getElementById('usage-cache-rate').textContent = formatPercent(usage.cache_read_tokens, usage.input_tokens + cachedInput);
  document.getElementById('usage-cache-saved').textContent = formatSmallCost(usage.cache_saved_usd);
  document.getElementById('usage-input-tokens').textContent = formatCount(usage.input_tokens);
  document.getElementById('usage-output-tokens').textContent = formatCount(usage.output_tokens);
  document.getElementById('usage-requests').textContent = formatCount(usage.request_count);
  document.getElementById('usage-cost').textContent = formatCost(usage.estimated_cost_usd);
  document.getElementById('usage-cost-per-request').textContent = usage.request_count > 0 ? formatSmallCost(usage.estimated_cost_usd / usage.request_count) : '—';
  lastCost = usage.estimated_cost_usd;
  showCostPerReply();
  document.getElementById('usage-cost-limit').textContent = usage.cost_limit_usd == null ? 'none' : formatCost(usage.cost_limit_usd);
  document.getElementById('usage-web-searches').textContent = formatCount(usage.web_search_requests);
  document.getElementById('usage-web-search-cost').textContent = formatCost(usage.web_search_cost_usd);

  const unpricedNote = document.getElementById('usage-unpriced-note');
  if (usage.unpriced_requests > 0) {
    unpricedNote.style.display = '';
    unpricedNote.textContent = `Note: ${formatCount(usage.unpriced_requests)} request(s) used a model with no known price and are not included in the cost estimate above.`;
  } else {
    unpricedNote.style.display = 'none';
  }

  const costLimitNote = document.getElementById('usage-cost-limit-note');
  if (usage.cost_limit_reached) {
    costLimitNote.style.display = '';
    costLimitNote.textContent = 'Cost limit reached — the bot has stopped making Claude API calls until the limit is raised or usage state is reset.';
  } else {
    costLimitNote.style.display = 'none';
  }

  if (usage.by_label.length === 0) {
    tbody.innerHTML = '<tr><td colspan="9">(no Claude API calls recorded yet)</td></tr>';
    return;
  }
  for (const entry of usage.by_label) {
    const row = document.createElement('tr');
    row.innerHTML = `
      <td>${escapeHtml(entry.label)}</td>
      <td>${formatCount(entry.input_tokens)}</td>
      <td>${formatCount(entry.output_tokens)}</td>
      <td>${formatCount(entry.input_tokens + entry.output_tokens + entry.cache_read_tokens + entry.cache_write_tokens)}</td>
      <td>${formatCount(entry.cache_read_tokens)}</td>
      <td>${formatCount(entry.request_count)}</td>
      <td>${formatCost(entry.estimated_cost_usd)}${entry.unpriced_requests > 0 ? ' *' : ''}</td>
      <td>${formatCount(entry.web_search_requests)}</td>
      <td>${formatCost(entry.web_search_cost_usd)}</td>
    `;
    tbody.appendChild(row);
  }
}

async function loadStatus(selectFirstRoom) {
  const res = await fetch('/api/status');
  const data = await res.json();
  document.getElementById('bot-user').textContent = data.bot_user_id ?? '(not logged in)';
  document.getElementById('homeserver').textContent = data.homeserver_url;
  const version = document.getElementById('version');
  version.innerHTML = `${escapeHtml(data.version)} · <a href="${escapeHtml(data.repository)}/blob/main/CHANGELOG.md">changelog</a> · <a href="${escapeHtml(data.repository)}">source</a>`;
  document.getElementById('chat-model').textContent = data.chat_model;
  document.getElementById('model').textContent = data.classifier_model;
  document.getElementById('fetch-links').textContent = data.fetch_links ? 'on' : 'off';
  document.getElementById('browser-rendering').textContent = data.browser_rendering ? 'headless browser' : 'off (plain fetch only)';
  document.getElementById('notify-on-start').textContent = data.notify_on_start.length ? data.notify_on_start.join(', ') : 'nobody';
  document.getElementById('uptime').textContent = formatUptime(data.uptime_seconds);
  document.getElementById('omdb-configured').textContent = data.omdb_configured ? 'yes' : 'no';
  document.getElementById('rate-limited').textContent = data.rate_limited_seconds_remaining == null
    ? 'not throttled'
    : `throttled — retrying in ${data.rate_limited_seconds_remaining}s`;

  roomNames = Object.fromEntries(data.rooms.filter((room) => room.display_name).map((room) => [room.room_id, room.display_name]));

  const roomSelect = document.getElementById('room-select');
  const previousValue = roomSelect.value;
  roomSelect.innerHTML = '';
  for (const room of data.rooms) {
    const opt = document.createElement('option');
    opt.value = room.room_id;
    opt.textContent = room.display_name ? `${room.display_name} (${room.room_id})` : room.room_id;
    roomSelect.appendChild(opt);
  }

  if (selectFirstRoom && data.rooms.length > 0) {
    roomSelect.value = data.rooms[0].room_id;
    loadMessages(roomSelect.value);
  } else if (previousValue) {
    roomSelect.value = previousValue;
  }
}

document.getElementById('room-select').addEventListener('change', (e) => loadMessages(e.target.value));
document.getElementById('refresh').addEventListener('click', async () => {
  await loadStatus(false);
  loadMessages(document.getElementById('room-select').value);
  loadStats();
  loadUsage();
});

loadStatus(true).then(loadStats);
loadSkills();
loadUsage();
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::classify::{Intent, MessageAnalysis, Sentiment};
    use crate::message_log::{GeneratedReply, LoggedMessage, MessageLogParams, PromptRecord};
    use crate::usage::UsageTracker;

    fn test_usage(input_tokens: u32, output_tokens: u32) -> threatflux_anthropic_sdk::models::Usage {
        threatflux_anthropic_sdk::models::Usage {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation: None,
            server_tool_use: None,
            inference_geo: None,
            service_tier: None,
        }
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("matrix-llm-bot-test-{label}-{nanos}"))
    }

    fn test_analysis(summary: &str) -> MessageAnalysis {
        MessageAnalysis {
            intent: Intent::Chitchat,
            confidence: 0.9,
            requires_response: false,
            summary: summary.to_string(),
            sentiment: Sentiment::Neutral,
            entities: vec![],
            command: None,
        }
    }

    /// Boots a real server on an OS-assigned port and drives it with real HTTP
    /// requests end-to-end: index page, `/api/status`, and `/api/rooms/{id}/messages`
    /// backed by real `MessageLogger` entries on disk.
    #[tokio::test]
    async fn serves_status_page_and_room_messages() {
        // No network I/O happens here: `.homeserver_url(...)` skips homeserver
        // discovery entirely, so this succeeds even with nothing listening at the URL.
        let client = matrix_sdk::Client::builder()
            .homeserver_url("http://example.invalid")
            .build()
            .await
            .expect("client build performs no network I/O for a plain homeserver_url");

        let database = Arc::new(crate::db::Database::open_in_memory());
        let message_log = Arc::new(MessageLogger::new(Arc::clone(&database)));

        let room_id = "!test:example.org";
        let classify_prompt = PromptRecord { system: "classify system".to_string(), user: "classify user".to_string() };
        let classify_raw_json = serde_json::json!({"intent": "chitchat"});
        message_log
            .log(MessageLogParams {
                room_id,
                event_id: "$event1",
                sender: "@alice:example.org",
                sender_name: None,
                origin_server_ts_ms: 1_000,
                body: "hello there",
                has_image: false,
                links: &[],
                analysis: &test_analysis("alice says hi"),
                classify_prompt: Some(&classify_prompt),
                classify_raw_json: Some(&classify_raw_json),
                response: None,
                response_event_id: None,
                thread_root_id: None,
            })
            .expect("log message 1");
        let reply = GeneratedReply {
            text: "hi alice!".to_string(),
            prompt: Some(PromptRecord { system: "chat system".to_string(), user: "chat user".to_string() }),
            label: "chat".to_string(),
            image: None,
            illustrations: Vec::new(),
        };
        message_log
            .log(MessageLogParams {
                room_id,
                event_id: "$event2",
                sender: "@bob:example.org",
                sender_name: None,
                origin_server_ts_ms: 2_000,
                body: "hi alice",
                has_image: false,
                links: &[],
                analysis: &test_analysis("bob says hi"),
                classify_prompt: Some(&classify_prompt),
                classify_raw_json: Some(&classify_raw_json),
                response: Some(&reply),
                response_event_id: None,
                thread_root_id: None,
            })
            .expect("log message 2");

        let skills_dir = unique_temp_dir("status-server-skills");
        let skills = Arc::new(SkillRegistry::load(&skills_dir).expect("load skills"));
        let tool_clients = Arc::new(ToolClients::new(None));
        let direct_database = Arc::clone(&database);
        let stats_database = Arc::clone(&database);
        let usage_tracker = Arc::new(UsageTracker::open(database, None).expect("open usage tracker"));
        usage_tracker.record("classify", "claude-haiku-4-5", &test_usage(120, 30));
        let rate_limit = Arc::new(Throttle::new(std::time::Duration::from_secs(60)));
        let direct_rooms = Arc::new(DirectRooms::new(direct_database, Vec::new()));
        direct_rooms.add("!private:example.org", "@alice:example.org").expect("add private room");

        let state = Arc::new(AppState {
            start_time: Instant::now(),
            client,
            message_log,
            skills,
            tool_clients,
            usage_tracker,
            rate_limit,
            direct_rooms,
            database: stats_database,
            homeserver_url: "http://example.invalid".to_string(),
            fetch_links: true,
            browser_rendering: false,
            notify_on_start: vec!["@admin:example.org".to_string()],
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, router(state)).await.expect("test server");
        });

        let http = reqwest::Client::new();
        let base = format!("http://{addr}");

        let index_resp = http.get(&base).send().await.expect("GET /");
        assert!(index_resp.status().is_success());
        let index_body = index_resp.text().await.expect("index body");
        assert!(index_body.contains("matrix-llm-bot"));

        let status_resp = http.get(format!("{base}/api/status")).send().await.expect("GET /api/status");
        assert!(status_resp.status().is_success());
        let status: serde_json::Value = status_resp.json().await.expect("status json");
        assert_eq!(status["classifier_model"], crate::classify::MODEL);
        assert_eq!(status["chat_model"], crate::handler::CHAT_MODEL);
        assert_eq!(status["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(status["repository"], "https://github.com/mdellavo/matrix-llm-bot");
        assert_eq!((status["fetch_links"].clone(), status["browser_rendering"].clone()), (true.into(), false.into()));
        assert_eq!(status["notify_on_start"], serde_json::json!(["@admin:example.org"]));
        assert_eq!(status["homeserver_url"], "http://example.invalid");
        assert_eq!(status["omdb_configured"], false, "no OMDb key was configured for this test");
        assert!(status["rate_limited_seconds_remaining"].is_null(), "not throttled in this test");
        // The client never synced, so it has no joined rooms — the route still works.
        assert_eq!(status["rooms"].as_array().expect("rooms array").len(), 0);

        let stats_resp = http.get(format!("{base}/api/stats")).send().await.expect("GET /api/stats");
        assert!(stats_resp.status().is_success());
        let stats: serde_json::Value = stats_resp.json().await.expect("stats json");
        assert_eq!(stats["totals"]["private_conversations"], 1);
        assert!(stats["totals"]["messages"].as_u64().expect("message count") > 0);

        let private_resp = http
            .get(format!("{base}/api/rooms/!private:example.org/messages"))
            .send()
            .await
            .expect("GET a private room's messages");
        assert_eq!(private_resp.status(), reqwest::StatusCode::NOT_FOUND);

        let messages_resp = http
            .get(format!("{base}/api/rooms/{room_id}/messages?limit=10"))
            .send()
            .await
            .expect("GET /api/rooms/{room_id}/messages");
        assert!(messages_resp.status().is_success());
        let messages: Vec<LoggedMessage> = messages_resp.json().await.expect("messages json");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].sender, "@alice:example.org");
        assert_eq!(messages[0].analysis.summary, "alice says hi");
        assert_eq!(messages[0].classify_prompt.as_ref().expect("classify prompt").system, "classify system");
        assert_eq!(messages[0].classify_raw_json.as_ref().expect("classify raw json")["intent"], "chitchat");
        assert!(messages[0].response.is_none(), "message 1 was logged with no response");
        assert_eq!(messages[1].sender, "@bob:example.org");
        assert_eq!(messages[1].response.as_deref(), Some("hi alice!"));
        assert_eq!(messages[1].response_label.as_deref(), Some("chat"));
        assert_eq!(messages[1].response_prompt.as_ref().expect("response prompt").user, "chat user");

        let empty_room_resp = http
            .get(format!("{base}/api/rooms/!nobody-here:example.org/messages"))
            .send()
            .await
            .expect("GET messages for unknown room");
        assert!(empty_room_resp.status().is_success());
        let empty: Vec<LoggedMessage> = empty_room_resp.json().await.expect("empty messages json");
        assert!(empty.is_empty());

        let skills_resp = http.get(format!("{base}/api/skills")).send().await.expect("GET /api/skills");
        assert!(skills_resp.status().is_success());
        let skills_json: serde_json::Value = skills_resp.json().await.expect("skills json");
        // No skill files in this test, so only the built-ins.
        let commands = skills_json.as_array().expect("skills array");
        let names: Vec<&str> = commands.iter().map(|command| command["name"].as_str().expect("name")).collect();
        assert_eq!(names, ["help", "stats", "changelog", "memories", "pp"]);
        assert!(commands.iter().all(|command| command["builtin"] == true));

        let usage_resp = http.get(format!("{base}/api/usage")).send().await.expect("GET /api/usage");
        assert!(usage_resp.status().is_success());
        let usage: serde_json::Value = usage_resp.json().await.expect("usage json");
        assert_eq!(usage["input_tokens"], 120);
        assert_eq!(usage["output_tokens"], 30);
        assert_eq!(usage["request_count"], 1);
        // 120 input tokens @ $1.00/M + 30 output tokens @ $5.00/M for claude-haiku-4-5.
        assert!((usage["estimated_cost_usd"].as_f64().expect("cost") - 0.00027).abs() < 1e-9);
        assert_eq!(usage["unpriced_requests"], 0);
        assert_eq!(usage["web_search_requests"], 0, "classify never uses the web search tool");
        assert_eq!(usage["web_search_cost_usd"], 0.0);
        let by_label = usage["by_label"].as_array().expect("by_label array");
        assert_eq!(by_label.len(), 1);
        assert_eq!(by_label[0]["label"], "classify");
        assert_eq!(by_label[0]["input_tokens"], 120);
        assert_eq!(by_label[0]["web_search_requests"], 0);
        let _ = std::fs::remove_dir_all(&skills_dir);
    }
}
