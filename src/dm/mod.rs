//! `!dm`: a role-playing game run by the bot as Dungeon Master.
//!
//! `!dm @friend … <premise>` creates a private room for the party and invites them.
//! Each message a player sends there is their character's action. Once every player
//! who has joined has acted, or a minute after the first action, the bot plays the
//! round: one Claude call that rolls real dice, changes the world through checked
//! tools, and ends with the narration. The narration is posted in the game room under
//! the map, and a one-line summary with a small map goes to a thread on the `!dm`
//! message, so the room it started from can follow along.

mod map;
mod sprites;
mod state;
mod store;
mod tools;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use matrix_sdk::attachment::{AttachmentConfig, AttachmentInfo, BaseImageInfo};
use matrix_sdk::room::reply::{EnforceThread, Reply};
use matrix_sdk::ruma::api::client::room::create_room::v3::{Request as CreateRoomRequest, RoomPreset};
use matrix_sdk::ruma::events::InitialStateEvent;
use matrix_sdk::ruma::events::relation::Thread;
use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
use matrix_sdk::ruma::events::room::message::{
    AddMentions, MessageType, OriginalSyncRoomMessageEvent, Relation, ReplyWithinThread, RoomMessageEventContent,
    TextMessageEventContent,
};
use matrix_sdk::ruma::{EventId, OwnedUserId, RoomId, UInt, UserId};
use matrix_sdk::{Client, Room, RoomMemberships};
use threatflux_anthropic_sdk::Client as Anthropic;
use tracing::{info, warn};

use self::state::GameState;
use self::store::{Action, Game, Player, Round};
use self::tools::{Ending, RoundTools};
use crate::db::Database;
use crate::direct_rooms::DirectRooms;
use crate::ignore::IgnoredUsers;
use crate::llm::{self, CompletionRequest};
use crate::message_log::ReplyImage;
use crate::throttle::Throttle;
use crate::usage::UsageTracker;

/// Storytelling is the point, so the game uses the chat model.
const MODEL: &str = crate::handler::CHAT_MODEL;
const LABEL: &str = "dm";
const PROMPT: &str = include_str!("prompt.md");
const MAX_TOKENS: u32 = 8000;

/// How long a round waits for the rest of the party after the first action.
const ROUND_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_ACTIVE_GAMES: usize = 3;
const MAX_PARTY: usize = 6;
const MAX_ROUNDS: u32 = 150;
const MAX_ACTION_CHARS: usize = 500;
/// Rounds shown in full to Claude. Older ones are only their summaries.
const RECENT_ROUNDS: usize = 5;

const USAGE: &str = "Start a game with `!dm @friend @friend <what happens first>`, like \
    `!dm @foo @bar you found a cave in the woods`. I'll make a room for the party and run the game there.";
const GAME_ROOM_HELP: &str = "Say what your character does, and I'll play the round once everyone has acted, or a minute \
    after the first action. `!dm go` plays it now, `!dm map` shows the map, `!dm status` shows the party, and \
    `!dm end` ends the game, for whoever started it.";

pub struct Dm {
    client: Client,
    anthropic: Arc<Anthropic>,
    usage_tracker: Arc<UsageTracker>,
    rate_limit: Arc<Throttle>,
    database: Arc<Database>,
    direct_rooms: Arc<DirectRooms>,
    ignored_users: Arc<IgnoredUsers>,
    /// The round each game's timer is waiting to play.
    timers: Mutex<HashMap<i64, u32>>,
    /// Held while a game's round is played, so two can't overlap.
    locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
}

pub struct DmParts {
    pub client: Client,
    pub anthropic: Arc<Anthropic>,
    pub usage_tracker: Arc<UsageTracker>,
    pub rate_limit: Arc<Throttle>,
    pub database: Arc<Database>,
    pub direct_rooms: Arc<DirectRooms>,
    pub ignored_users: Arc<IgnoredUsers>,
}

impl Dm {
    pub fn new(parts: DmParts) -> Arc<Self> {
        let DmParts { client, anthropic, usage_tracker, rate_limit, database, direct_rooms, ignored_users } = parts;
        Arc::new(Self {
            client,
            anthropic,
            usage_tracker,
            rate_limit,
            database,
            direct_rooms,
            ignored_users,
            timers: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
        })
    }

    /// Picks up games that were waiting when the bot stopped: an opening scene that
    /// was never played, or actions that never got their round.
    pub fn resume(self: &Arc<Self>) {
        let games = match store::active(&self.database) {
            Ok(games) => games,
            Err(err) => {
                warn!(?err, "failed to load games to resume");
                return;
            }
        };
        for game in games {
            let waiting = store::pending_actions(&self.database, game.id).map(|actions| !actions.is_empty()).unwrap_or(false);
            if game.round == 0 {
                self.play_soon(game.id);
            } else if waiting {
                self.start_timer(game.id, game.round);
            }
        }
    }

    /// `!dm` outside a game room: starts a game, returning the reply for the thread.
    pub async fn start(self: &Arc<Self>, room: &Room, event: &OriginalSyncRoomMessageEvent, args: &str) -> String {
        match self.try_start(room, event, args).await {
            Ok(reply) => reply,
            Err(err) => {
                warn!(?err, room = %room.room_id(), "failed to start a game");
                "Sorry, I couldn't set up the game.".to_string()
            }
        }
    }

    async fn try_start(self: &Arc<Self>, room: &Room, event: &OriginalSyncRoomMessageEvent, args: &str) -> Result<String> {
        if args.trim().is_empty() || args.trim().eq_ignore_ascii_case("help") {
            return Ok(USAGE.to_string());
        }
        if self.usage_tracker.over_cost_limit() {
            return Ok(crate::usage::COST_LIMIT_REPLY.to_string());
        }
        if store::active(&self.database)?.len() >= MAX_ACTIVE_GAMES {
            return Ok(format!("There are already {MAX_ACTIVE_GAMES} games going. End one with `!dm end` in its room first."));
        }

        let own_id = self.client.user_id().context("not logged in")?.to_owned();
        let own_server = own_id.server_name().as_str().to_string();
        let mentioned: Vec<OwnedUserId> = event
            .content
            .mentions
            .as_ref()
            .map(|mentions| mentions.user_ids.iter().filter(|user| **user != own_id && **user != event.sender).cloned().collect())
            .unwrap_or_default();
        let mut party = vec![event.sender.clone()];
        let mut left_out = Vec::new();
        for user in mentioned {
            let allowed = !self.ignored_users.contains(user.as_str()) && self.direct_rooms.allows(user.as_str(), &own_server);
            if allowed && party.len() < MAX_PARTY {
                party.push(user);
            } else {
                left_out.push(user);
            }
        }

        let mut names = Vec::new();
        for user in &party {
            names.push(display_name(room, user).await);
        }
        let mut known: Vec<String> = party.iter().map(|user| user.to_string()).collect();
        known.extend(party.iter().map(|user| format!("@{}", user.localpart())));
        known.extend(names.iter().cloned());
        let premise = strip_mentions(args, &known);
        if premise.is_empty() {
            return Ok(USAGE.to_string());
        }

        let name = self.unique_name(&premise)?;
        let players = players(&party, &names);
        let state = GameState::new(&players.iter().map(|player| (player.user_id.clone(), player.name.clone(), player.entity_id.clone())).collect::<Vec<_>>());

        let mut request = CreateRoomRequest::new();
        request.name = Some(name.clone());
        request.topic = Some(format!("A game run by the bot: {premise}"));
        request.invite = party.clone();
        request.preset = Some(RoomPreset::PrivateChat);
        request.initial_state = vec![InitialStateEvent::with_empty_state_key(RoomEncryptionEventContent::with_recommended_defaults()).to_raw_any()];
        let game_room = self.client.create_room(request).await.context("failed to create the game room")?;
        // Kept off the status page and out of `!stats`, like a private conversation.
        self.direct_rooms.add(game_room.room_id().as_str(), event.sender.as_str())?;

        let thread_root = match &event.content.relates_to {
            Some(Relation::Thread(thread)) => thread.event_id.clone(),
            _ => event.event_id.clone(),
        };
        let id = store::create(&self.database, &store::NewGame {
            name: &name,
            premise: &premise,
            owner: event.sender.as_str(),
            main_room_id: room.room_id().as_str(),
            thread_root_id: thread_root.as_str(),
            game_room_id: game_room.room_id().as_str(),
            players: &players,
            state: &state,
        })?;
        info!(game = id, %name, room = %game_room.room_id(), players = players.len(), "started a game");
        self.play_soon(id);

        let mut reply = format!(
            "**{name}** is starting for {}. Accept the invite to its room to play, and I'll post each round's story in this thread.",
            join_names(&names)
        );
        if !left_out.is_empty() {
            let left_out: Vec<String> = left_out.iter().map(|user| user.to_string()).collect();
            reply.push_str(&format!(" I couldn't invite {}.", left_out.join(", ")));
        }
        Ok(reply)
    }

    /// Handles a message in a game room, returning false for any other room.
    pub async fn on_message(self: &Arc<Self>, room: &Room, event: &OriginalSyncRoomMessageEvent) -> bool {
        let game = match store::by_room(&self.database, room.room_id().as_str()) {
            Ok(Some(game)) => game,
            Ok(None) => return false,
            Err(err) => {
                warn!(?err, room = %room.room_id(), "failed to look up a game");
                return false;
            }
        };
        // Edits don't change an action already taken.
        if matches!(event.content.relates_to, Some(Relation::Replacement(_))) {
            return true;
        }
        let MessageType::Text(text) = &event.content.msgtype else { return true };
        let body = text.body.trim();
        if let Err(err) = self.game_message(room, event, &game, body).await {
            warn!(?err, game = game.id, "failed to handle a game message");
        }
        true
    }

    async fn game_message(self: &Arc<Self>, room: &Room, event: &OriginalSyncRoomMessageEvent, game: &Game, body: &str) -> Result<()> {
        if game.status != store::ACTIVE {
            if body.starts_with("!dm") {
                say(room, "This game is over. Start another with `!dm` in the room it came from.").await;
            }
            return Ok(());
        }
        if let Some(command) = body.strip_prefix("!dm") {
            return self.game_command(room, event, game, command.trim()).await;
        }
        // Other commands, and anyone not playing, aren't part of the game.
        if body.starts_with('!') || body.is_empty() {
            return Ok(());
        }
        let players = store::players(&self.database, game.id)?;
        if !players.iter().any(|player| player.user_id == event.sender.as_str()) {
            return Ok(());
        }

        let text: String = body.chars().take(MAX_ACTION_CHARS).collect();
        store::add_action(&self.database, game.id, &Action { user_id: event.sender.to_string(), text, sent_at_ms: event.origin_server_ts.get().into() })?;
        crate::handler::send_reaction(room, &event.event_id, "✅").await;

        let acted: Vec<String> = store::pending_actions(&self.database, game.id)?.into_iter().map(|action| action.user_id).collect();
        let joined = joined_players(room, &players).await;
        if !joined.is_empty() && joined.iter().all(|user| acted.contains(user)) {
            self.play_soon(game.id);
        } else {
            self.start_timer(game.id, game.round);
        }
        Ok(())
    }

    async fn game_command(self: &Arc<Self>, room: &Room, event: &OriginalSyncRoomMessageEvent, game: &Game, command: &str) -> Result<()> {
        match command.to_ascii_lowercase().as_str() {
            "go" => {
                if store::pending_actions(&self.database, game.id)?.is_empty() {
                    say(room, "Nobody has acted yet this round.").await;
                } else {
                    self.play_soon(game.id);
                }
            }
            "map" => {
                let state = game.state.clone();
                let round = game.round;
                match tokio::task::spawn_blocking(move || map::render(&state, round)).await? {
                    Ok((full, _)) => post_image(room, &full, None, None).await,
                    Err(err) => warn!(?err, game = game.id, "failed to draw the map"),
                }
            }
            "status" => say(room, &status(game)).await,
            "end" if game.owner == event.sender.as_str() => {
                store::end(&self.database, game.id)?;
                say(room, "**The end.** The game was ended by whoever started it. Thanks for playing.").await;
                if let Some(main) = self.room(&game.main_room_id) {
                    post_text(&main, &format!("**{}** ended after {} rounds.", game.name, game.round), Some(&game.thread_root_id)).await;
                }
            }
            "end" => say(room, "Only whoever started the game can end it.").await,
            _ => say(room, GAME_ROOM_HELP).await,
        }
        Ok(())
    }

    fn start_timer(self: &Arc<Self>, game_id: i64, round: u32) {
        {
            let mut timers = self.timers.lock().expect("timers mutex poisoned");
            if timers.get(&game_id) == Some(&round) {
                return;
            }
            timers.insert(game_id, round);
        }
        let dm = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(ROUND_TIMEOUT).await;
            let still_waiting = {
                let mut timers = dm.timers.lock().expect("timers mutex poisoned");
                let waiting = timers.get(&game_id) == Some(&round);
                if waiting {
                    timers.remove(&game_id);
                }
                waiting
            };
            if still_waiting {
                dm.play(game_id, Some(round)).await;
            }
        });
    }

    fn play_soon(self: &Arc<Self>, game_id: i64) {
        let dm = Arc::clone(self);
        tokio::spawn(async move { dm.play(game_id, None).await });
    }

    /// Plays the game's next round, if it has one waiting. `only_round` is for a timer
    /// set during that round, which shouldn't play a later one.
    async fn play(self: &Arc<Self>, game_id: i64, only_round: Option<u32>) {
        let lock = Arc::clone(self.locks.lock().expect("locks mutex poisoned").entry(game_id).or_default());
        let _held = lock.lock().await;
        if let Err(err) = self.play_locked(game_id, only_round).await {
            warn!(?err, game = game_id, "failed to play a round");
        }
    }

    async fn play_locked(self: &Arc<Self>, game_id: i64, only_round: Option<u32>) -> Result<()> {
        let Some(game) = store::by_id(&self.database, game_id)? else { return Ok(()) };
        if game.status != store::ACTIVE || only_round.is_some_and(|round| round != game.round) {
            return Ok(());
        }
        let Some(room) = self.room(&game.game_room_id) else {
            warn!(game = game.id, "the game room isn't joined");
            return Ok(());
        };
        let actions = store::take_actions(&self.database, game.id)?;
        if actions.is_empty() && game.round > 0 {
            return Ok(());
        }
        // A new round makes any timer for the last one pointless.
        self.timers.lock().expect("timers mutex poisoned").remove(&game.id);

        if game.round >= MAX_ROUNDS {
            store::end(&self.database, game.id)?;
            say(&room, &format!("**The end.** That's {MAX_ROUNDS} rounds, as long as a game can go. Thanks for playing.")).await;
            return Ok(());
        }
        let paused = if self.usage_tracker.over_cost_limit() {
            Some("I've hit my spending limit, so the game is paused until it's raised.")
        } else if self.rate_limit.remaining().is_some() {
            Some("I'm being rate limited, so the round will wait. Send `!dm go` in a minute.")
        } else {
            None
        };
        if let Some(paused) = paused {
            store::restore_actions(&self.database, game.id, &actions)?;
            say(&room, paused).await;
            return Ok(());
        }

        let typing = crate::handler::TypingIndicator::start(room.clone());
        let played = self.run_round(&game, &actions).await;
        typing.stop(&room).await;
        let (ending, state, rolls) = match played {
            Ok(played) => played,
            Err(err) => {
                warn!(?err, game = game.id, "the round failed");
                store::restore_actions(&self.database, game.id, &actions)?;
                say(&room, "I lost the thread there. Send `!dm go` to try the round again.").await;
                return Ok(());
            }
        };

        let round = Round { round: game.round, actions, narration: ending.narration.clone(), summary: ending.summary.clone() };
        store::finish_round(&self.database, &game, &round, &state, ending.game_over)?;

        let drawn_state = state.clone();
        let drawn_round = game.round + 1;
        let maps = match tokio::task::spawn_blocking(move || map::render(&drawn_state, drawn_round)).await {
            Ok(Ok(maps)) => Some(maps),
            Ok(Err(err)) => {
                warn!(?err, game = game.id, "failed to draw the map");
                None
            }
            Err(err) => {
                warn!(?err, game = game.id, "map task failed");
                None
            }
        };

        let mut narration = ending.narration.clone();
        if !rolls.is_empty() {
            narration.push_str(&format!("\n\n🎲 {}", rolls.join(" · ")));
        }
        if ending.game_over {
            narration.push_str("\n\n**The end.** Thanks for playing.");
        }
        match &maps {
            Some((full, _)) => post_image(&room, full, Some(&narration), None).await,
            None => post_text(&room, &narration, None).await,
        }

        if let Some(main) = self.room(&game.main_room_id) {
            let mut line = format!("**{}**, round {}: {}", game.name, game.round + 1, ending.summary);
            if ending.game_over {
                line.push_str(" **The end.**");
            }
            match &maps {
                Some((_, small)) => post_image(&main, small, Some(&line), Some(&game.thread_root_id)).await,
                None => post_text(&main, &line, Some(&game.thread_root_id)).await,
            }
        }
        info!(game = game.id, round = game.round + 1, over = ending.game_over, "played a round");
        Ok(())
    }

    async fn run_round(&self, game: &Game, actions: &[Action]) -> Result<(Ending, GameState, Vec<String>)> {
        let players = store::players(&self.database, game.id)?;
        let rounds = store::rounds(&self.database, game.id)?;
        let user = round_prompt(game, &players, &rounds, actions);
        claude_round(&self.anthropic, &self.usage_tracker, &self.rate_limit, &game.state, &user).await
    }

    fn room(&self, room_id: &str) -> Option<Room> {
        RoomId::parse(room_id).ok().and_then(|id| self.client.get_room(&id))
    }

    fn unique_name(&self, premise: &str) -> Result<String> {
        let base = slug(premise);
        for _ in 0..10 {
            let name = format!("{base}-{}", suffix());
            if !store::name_taken(&self.database, &name)? {
                return Ok(name);
            }
        }
        anyhow::bail!("no free game name for {base}")
    }
}

/// One Claude call, with the tools to roll, change the world, and end the round.
async fn claude_round(
    anthropic: &Anthropic,
    usage_tracker: &UsageTracker,
    rate_limit: &Throttle,
    state: &GameState,
    user: &str,
) -> Result<(Ending, GameState, Vec<String>)> {
    let tools = RoundTools::new(state.clone());
    let request = CompletionRequest {
        model: MODEL,
        system: PROMPT,
        user,
        cache: true,
        max_tokens: MAX_TOKENS,
        local_tools: Some(&tools),
        label: LABEL,
        ..Default::default()
    };
    let answered = llm::complete(anthropic, &request, usage_tracker, rate_limit).await;
    let ending = tools.ending.lock().expect("ending mutex poisoned").take();
    // Once the round is ended there's often nothing left to say, which `complete`
    // treats as a failed reply. The ending is what counts.
    let text = match answered {
        Ok(text) => text,
        Err(_) if ending.is_some() => String::new(),
        Err(err) => return Err(err),
    };
    let ending = match ending {
        Some(ending) => ending,
        // It told the story without ending the round properly. The story still counts.
        None if !text.trim().is_empty() => Ending { narration: text.trim().to_string(), summary: first_sentence(text.trim()), game_over: false },
        None => anyhow::bail!("the round ended without a story"),
    };
    let state = tools.state.into_inner().expect("state mutex poisoned");
    let rolls = tools.rolls.into_inner().expect("rolls mutex poisoned");
    Ok((ending, state, rolls))
}

/// What Claude gets each round: the game, the party, the story so far, the world,
/// and this round's actions.
fn round_prompt(game: &Game, players: &[Player], rounds: &[Round], actions: &[Action]) -> String {
    let name_of = |user_id: &str| players.iter().find(|player| player.user_id == user_id).map_or(user_id, |player| player.name.as_str()).to_string();
    let mut prompt = format!("Game: {}\nPremise: {}\n\nParty:\n", game.name, game.premise);
    for player in players {
        prompt.push_str(&format!("- {}, whose character is \"{}\" on the map\n", player.name, player.entity_id));
    }

    let older = rounds.len().saturating_sub(RECENT_ROUNDS);
    if older > 0 {
        prompt.push_str("\nThe story so far:\n");
        for round in &rounds[..older] {
            prompt.push_str(&format!("- Round {}: {}\n", round.round + 1, round.summary));
        }
    }
    for round in &rounds[older..] {
        prompt.push_str(&format!("\nRound {}:\n", round.round + 1));
        for action in &round.actions {
            prompt.push_str(&format!("[{}] {}\n", name_of(&action.user_id), action.text));
        }
        prompt.push_str(&format!("You narrated: {}\n", round.narration));
    }

    prompt.push_str(&format!("\nThe world now:\n{}\n", serde_json::to_string(&game.state).unwrap_or_default()));
    if game.round == 0 {
        prompt.push_str(
            "\nThis is the opening. Set the scene from the premise: draw the map with set_map, put the party and \
             anything they can see on it, set their first goal, and end on something to act on.\n",
        );
    } else {
        prompt.push_str(&format!("\nRound {}. What the party does:\n", game.round + 1));
        for action in actions {
            prompt.push_str(&format!("[{}] {}\n", name_of(&action.user_id), action.text));
        }
        let idle: Vec<String> = players
            .iter()
            .filter(|player| !actions.iter().any(|action| action.user_id == player.user_id))
            .map(|player| player.name.clone())
            .collect();
        if !idle.is_empty() {
            prompt.push_str(&format!("No action from: {}\n", idle.join(", ")));
        }
    }
    prompt
}

fn status(game: &Game) -> String {
    let mut lines = vec![format!("**{}**, round {}", game.name, game.round)];
    if !game.state.quest.is_empty() {
        lines.push(format!("Goal: {}", game.state.quest));
    }
    for entity in game.state.entities.iter().filter(|entity| entity.kind == state::Kind::Player) {
        let hp = match (entity.hp, entity.max_hp) {
            (Some(0), _) => "fallen".to_string(),
            (Some(hp), Some(max)) => format!("{hp}/{max} hp"),
            _ => "unhurt".to_string(),
        };
        let items = if entity.items.is_empty() { String::new() } else { format!(", carrying {}", entity.items.join(", ")) };
        lines.push(format!("- {}: {hp}{items}", entity.name));
    }
    lines.join("\n")
}

/// Players' characters, each with a short map ID made from their name.
fn players(party: &[OwnedUserId], names: &[String]) -> Vec<Player> {
    let mut players: Vec<Player> = Vec::new();
    for (user, name) in party.iter().zip(names) {
        let base = {
            let id: String = name.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect();
            if id.is_empty() { user.localpart().to_lowercase() } else { id }
        };
        let mut entity_id = base.clone();
        let mut n = 2;
        while players.iter().any(|player| player.entity_id == entity_id) {
            entity_id = format!("{base}{n}");
            n += 1;
        }
        players.push(Player { user_id: user.to_string(), name: name.clone(), entity_id });
    }
    players
}

/// The premise without the people mentioned at its start.
fn strip_mentions(args: &str, known: &[String]) -> String {
    let mut rest = args.trim();
    loop {
        let before = rest;
        for name in known.iter().filter(|name| !name.is_empty()) {
            if let Some(after) = rest.strip_prefix(name.as_str()) {
                rest = after.trim_start_matches([':', ',']).trim_start();
            }
        }
        if rest == before {
            break;
        }
    }
    rest.trim().to_string()
}

/// A room name from the premise's first words, without the opening "you found a".
fn slug(premise: &str) -> String {
    const SKIP: &[&str] = &["you", "we", "they", "the", "party", "a", "an", "found", "find", "discover", "discovered", "are", "enter", "entered", "come", "across", "upon"];
    let words: Vec<String> = premise
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .skip_while(|word| SKIP.contains(&word.as_str()))
        .take(5)
        .collect();
    let mut slug = words.join("-");
    if slug.is_empty() {
        slug = "adventure".to_string();
    }
    slug.chars().take(40).collect::<String>().trim_end_matches('-').to_string()
}

fn suffix() -> String {
    const LETTERS: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    (0..4).map(|_| LETTERS[rand::random_range(0..LETTERS.len())] as char).collect()
}

fn first_sentence(text: &str) -> String {
    let end = text.find(['.', '!', '?']).map_or(text.len(), |at| at + 1);
    text[..end].chars().take(150).collect()
}

fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

async fn display_name(room: &Room, user: &UserId) -> String {
    match room.get_member_no_sync(user).await {
        Ok(Some(member)) => member.display_name().unwrap_or(user.localpart()).to_string(),
        _ => user.localpart().to_string(),
    }
}

async fn joined_players(room: &Room, players: &[Player]) -> Vec<String> {
    match room.members(RoomMemberships::JOIN).await {
        Ok(members) => players
            .iter()
            .filter(|player| members.iter().any(|member| member.user_id().as_str() == player.user_id))
            .map(|player| player.user_id.clone())
            .collect(),
        Err(err) => {
            warn!(?err, room = %room.room_id(), "failed to list the game room's members");
            Vec::new()
        }
    }
}

async fn say(room: &Room, text: &str) {
    post_text(room, text, None).await;
}

/// Posts text, in the thread at `thread_root` if given.
async fn post_text(room: &Room, text: &str, thread_root: Option<&str>) {
    let mut content = RoomMessageEventContent::text_markdown(text);
    if let Some(root) = thread_root.and_then(|root| EventId::parse(root).ok()) {
        content.relates_to = Some(Relation::Thread(Thread::plain(root.clone(), root)));
    }
    if let Err(err) = room.send(content).await {
        warn!(?err, room = %room.room_id(), "failed to post a game message");
    }
}

/// Posts an image with `caption` as its text, in the thread at `thread_root` if given.
/// Clients that don't show captions show the caption as the image's name.
async fn post_image(room: &Room, image: &ReplyImage, caption: Option<&str>, thread_root: Option<&str>) {
    let info = AttachmentInfo::Image(BaseImageInfo {
        width: Some(UInt::from(image.width)),
        height: Some(UInt::from(image.height)),
        size: UInt::try_from(image.data.len()).ok(),
        is_animated: Some(false),
        blurhash: None,
    });
    let mut config = AttachmentConfig::new().info(info).caption(caption.map(TextMessageEventContent::markdown));
    if let Some(root) = thread_root.and_then(|root| EventId::parse(root).ok()) {
        config = config.reply(Some(Reply {
            event_id: root,
            enforce_thread: EnforceThread::Threaded(ReplyWithinThread::No),
            add_mentions: AddMentions::No,
        }));
    }
    if let Err(err) = room.send_attachment(image.filename.clone(), &image.mime, image.data.clone(), config).await {
        warn!(?err, room = %room.room_id(), "failed to post a map");
        if let Some(caption) = caption {
            post_text(room, caption, thread_root).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_games_from_their_premise() {
        assert_eq!(slug("you found a cave in the woods"), "cave-in-the-woods");
        assert_eq!(slug("The party enters the Tomb of Horrors!"), "enters-the-tomb-of-horrors");
        assert_eq!(slug("a dragon's hoard, guarded by 3 kobolds"), "dragon-s-hoard-guarded-by");
        assert_eq!(slug("!!!"), "adventure");
        let suffix = suffix();
        assert_eq!(suffix.len(), 4);
        assert!(suffix.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()), "{suffix}");
    }

    #[test]
    fn takes_the_party_off_the_front_of_the_premise() {
        let known = ["@foo:x".to_string(), "@bar".to_string(), "Bar Baz".to_string()];
        assert_eq!(strip_mentions("@foo:x @bar you found a cave", &known), "you found a cave");
        assert_eq!(strip_mentions("Bar Baz: you found a cave", &known), "you found a cave");
        assert_eq!(strip_mentions("you found a cave, @bar", &known), "you found a cave, @bar");
        assert_eq!(strip_mentions("@foo:x", &known), "");
    }

    #[test]
    fn gives_each_player_a_map_id() {
        let party: Vec<OwnedUserId> = ["@procyon:x", "@foo:x", "@bar:x"].iter().map(|id| UserId::parse(*id).unwrap()).collect();
        let names = ["Procyon".to_string(), "Foo!".to_string(), "procyon".to_string()];
        let ids: Vec<String> = players(&party, &names).into_iter().map(|player| player.entity_id).collect();
        assert_eq!(ids, ["procyon", "foo", "procyon2"]);
        let emoji = players(&party[..1], &["🦝".to_string()]);
        assert_eq!(emoji[0].entity_id, "procyon");
    }

    fn game(round: u32) -> Game {
        let state = GameState::new(&[("@a:x".to_string(), "Alice".to_string(), "alice".to_string()), ("@b:x".to_string(), "Bob".to_string(), "bob".to_string())]);
        Game {
            id: 1,
            name: "cave-xxxx".to_string(),
            premise: "a cave".to_string(),
            owner: "@a:x".to_string(),
            main_room_id: "!main:x".to_string(),
            thread_root_id: "$root".to_string(),
            game_room_id: "!game:x".to_string(),
            status: store::ACTIVE.to_string(),
            round,
            state,
        }
    }

    fn party() -> Vec<Player> {
        vec![
            Player { user_id: "@a:x".to_string(), name: "Alice".to_string(), entity_id: "alice".to_string() },
            Player { user_id: "@b:x".to_string(), name: "Bob".to_string(), entity_id: "bob".to_string() },
        ]
    }

    #[test]
    fn opens_with_the_premise_and_the_world() {
        let prompt = round_prompt(&game(0), &party(), &[], &[]);
        assert!(prompt.starts_with("Game: cave-xxxx\nPremise: a cave\n\nParty:\n- Alice, whose character is \"alice\" on the map\n"), "{prompt}");
        assert!(prompt.contains("\"entities\""), "{prompt}");
        assert!(prompt.contains("This is the opening."), "{prompt}");
    }

    #[test]
    fn plays_on_from_the_story_so_far() {
        let action = |user: &str, text: &str| Action { user_id: user.to_string(), text: text.to_string(), sent_at_ms: 0 };
        let rounds: Vec<Round> = (0..7)
            .map(|round| Round { round, actions: vec![action("@a:x", &format!("act {round}"))], narration: format!("story {round}"), summary: format!("summary {round}") })
            .collect();
        let prompt = round_prompt(&game(7), &party(), &rounds, &[action("@a:x", "attack the closest wolf")]);
        // Older rounds are only their summaries, and the last five are in full.
        assert!(prompt.contains("The story so far:\n- Round 1: summary 0\n- Round 2: summary 1\n"), "{prompt}");
        assert!(!prompt.contains("story 1\n"), "{prompt}");
        assert!(prompt.contains("\nRound 3:\n[Alice] act 2\nYou narrated: story 2\n"), "{prompt}");
        assert!(prompt.contains("\nRound 8. What the party does:\n[Alice] attack the closest wolf\nNo action from: Bob\n"), "{prompt}");
    }

    #[test]
    fn shows_the_party() {
        let mut game = game(3);
        game.state.quest = "Find the glow".to_string();
        game.state.entities[1].hp = Some(0);
        game.state.entities[0].items.push("torch".to_string());
        assert_eq!(status(&game), "**cave-xxxx**, round 3\nGoal: Find the glow\n- Alice: 12/12 hp, carrying torch\n- Bob: fallen");
    }

    mod against_a_fake_server {
        use std::sync::{Arc, Mutex};

        use axum::{Json, Router, extract::State, routing::post};
        use serde_json::{Value, json};
        use threatflux_anthropic_sdk::Config;

        use super::*;

        type Seen = Arc<Mutex<Vec<Value>>>;

        /// Answers each request with the next canned response, recording what it got.
        async fn serve(responses: Vec<Value>) -> (Anthropic, Seen) {
            async fn handler(State((seen, responses)): State<(Seen, Arc<Vec<Value>>)>, Json(body): Json<Value>) -> Json<Value> {
                let mut seen = seen.lock().unwrap();
                seen.push(body);
                Json(responses[seen.len() - 1].clone())
            }
            let seen: Seen = Arc::default();
            let app = Router::new().route("/v1/messages", post(handler)).with_state((Arc::clone(&seen), Arc::new(responses)));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let base_url = format!("http://127.0.0.1:{port}/").parse().unwrap();
            (Anthropic::new(Config::new("test-key").unwrap().with_base_url(base_url)), seen)
        }

        fn response(content: Value, stop_reason: &str) -> Value {
            json!({
                "id": "msg_1", "type": "message", "role": "assistant", "model": MODEL, "content": content,
                "stop_reason": stop_reason, "stop_sequence": null, "usage": {"input_tokens": 100, "output_tokens": 100},
            })
        }

        fn tool(id: &str, name: &str, input: Value) -> Value {
            json!({"type": "tool_use", "id": id, "name": name, "input": input})
        }

        fn trackers() -> (UsageTracker, Throttle) {
            (UsageTracker::open(Arc::new(Database::open_in_memory()), None).unwrap(), Throttle::new(Duration::from_secs(60)))
        }

        #[tokio::test]
        async fn plays_a_round_with_dice_changes_and_an_ending() {
            let (anthropic, seen) = serve(vec![
                response(
                    json!([
                        tool("t1", tools::ROLL_DICE, json!({"dice": "1d20+2", "reason": "Alice attacks the wolf"})),
                        tool("t2", tools::UPDATE_STATE, json!({"changes": [{"op": "move", "id": "alice", "x": 3, "y": 3}]})),
                        // A refused change goes back to Claude, without spoiling the round.
                        tool("t3", tools::UPDATE_STATE, json!({"changes": [{"op": "move", "id": "alice", "x": 99, "y": 3}]})),
                    ]),
                    "tool_use",
                ),
                response(json!([tool("t4", tools::END_ROUND, json!({"narration": "The wolf yelps and flees.", "summary": "the party drove off a wolf"}))]), "tool_use"),
                response(json!([{"type": "text", "text": ""}]), "end_turn"),
            ])
            .await;
            let (usage, throttle) = trackers();
            let state = game(1).state;
            let (ending, state, rolls) = claude_round(&anthropic, &usage, &throttle, &state, "Round 2.").await.unwrap();

            assert_eq!((ending.narration.as_str(), ending.summary.as_str(), ending.game_over), ("The wolf yelps and flees.", "the party drove off a wolf", false));
            let alice = state.entity("alice").unwrap();
            assert_eq!((alice.x, alice.y), (3, 3));
            assert_eq!(rolls.len(), 1);
            assert!(rolls[0].starts_with("Alice attacks the wolf: 1d20+2 → "), "{rolls:?}");

            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 3);
            let first = &seen[0];
            assert_eq!(first["model"], MODEL);
            let names: Vec<&str> = first["tools"].as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
            assert_eq!(names, [tools::ROLL_DICE, tools::UPDATE_STATE, tools::END_ROUND]);
            assert!(first["system"].to_string().contains("Dungeon Master"));
            // The refused move went back as an error.
            let results = seen[1]["messages"].as_array().unwrap().last().unwrap()["content"].clone();
            assert_eq!(results[2]["is_error"], true, "{results}");
            assert_eq!(usage.snapshot().by_label[0].label, LABEL);
        }

        #[tokio::test]
        async fn a_story_without_an_ending_still_counts() {
            let (anthropic, _) = serve(vec![response(json!([{"type": "text", "text": "The cave is dark. Something moves."}]), "end_turn")]).await;
            let (usage, throttle) = trackers();
            let (ending, _, _) = claude_round(&anthropic, &usage, &throttle, &game(1).state, "Round 2.").await.unwrap();
            assert_eq!((ending.narration.as_str(), ending.summary.as_str()), ("The cave is dark. Something moves.", "The cave is dark."));

            let (anthropic, _) = serve(vec![response(json!([{"type": "text", "text": " "}]), "end_turn")]).await;
            assert!(claude_round(&anthropic, &usage, &throttle, &game(1).state, "Round 2.").await.is_err());
        }
    }

    #[test]
    fn small_helpers() {
        assert_eq!(join_names(&["A".to_string(), "B".to_string(), "C".to_string()]), "A, B and C");
        assert_eq!(join_names(&["A".to_string()]), "A");
        assert_eq!(first_sentence("The wolves flee. You win."), "The wolves flee.");
        assert_eq!(first_sentence("No stop"), "No stop");
    }
}
