use std::collections::{HashMap, HashSet};
use std::path::Path;

use anthropic_sdk::types::{ContentBlock, MessageCreateBuilder};
use anthropic_sdk::Anthropic;
use anyhow::{Context, Result};
use matrix_sdk::room::Room;
use serde::Deserialize;
use tracing::warn;

use crate::classify::{self, CommandInfo};
use crate::message_log::{LoggedMessage, MessageLogger};

const SKILL_FILE_NAME: &str = "SKILL.md";
const FRONTMATTER_DELIMITER: &str = "---";
const KNOWN_TOOLS: &[&str] = &["message_log", "room_info", "current_time"];
const RESERVED_NAME: &str = "help";
const DEFAULT_MESSAGE_LOG_COUNT: usize = 5;
const MAX_MESSAGE_LOG_COUNT: usize = 20;

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    usage: Option<String>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    args: Vec<ArgSpec>,
}

/// One argument a skill expects in `command.args`, validated at invocation time by
/// `resolve_args`. Declaring `args` at all switches a skill from "pass whatever the
/// classifier extracted straight through" to "validate and default explicitly" — see
/// `execute`.
#[derive(Debug, Clone, Deserialize)]
struct ArgSpec {
    name: String,
    #[serde(rename = "type")]
    kind: ArgType,
    #[serde(default)]
    #[allow(dead_code)] // not surfaced anywhere yet; kept for skill authors' documentation value
    description: Option<String>,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    default: Option<serde_json::Value>,
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ArgType {
    String,
    Integer,
    Number,
    Boolean,
}

impl ArgType {
    fn label(self) -> &'static str {
        match self {
            ArgType::String => "string",
            ArgType::Integer => "integer",
            ArgType::Number => "number",
            ArgType::Boolean => "boolean",
        }
    }

    fn matches(self, value: &serde_json::Value) -> bool {
        match self {
            ArgType::String => value.is_string(),
            ArgType::Integer => value.is_i64() || value.is_u64(),
            ArgType::Number => value.is_number(),
            ArgType::Boolean => value.is_boolean(),
        }
    }
}

/// A single loaded command, parsed from a `SKILL.md` file: the metadata shown in
/// `help` (and the status server's `/api/skills`), and the prompt body sent to
/// Claude as the system prompt when the command is invoked.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub usage: Option<String>,
    pub tools: Vec<String>,
    pub aliases: Vec<String>,
    pub model: Option<String>,
    args: Vec<ArgSpec>,
    pub prompt: String,
}

/// Commands loaded from `skills/<name>/SKILL.md` files, keyed by lowercased name.
///
/// A single skill file with bad frontmatter, an unknown `tools` entry, or a missing
/// `SKILL.md` is skipped with a `tracing::warn!` rather than failing the whole load —
/// one author's mistake in one file shouldn't take down every other working skill or
/// the bot's Matrix connection. The only failures that propagate (matching
/// `Config::validate`'s fail-loud precedent) are I/O errors reading `dir` itself, and
/// two skills — or a skill's alias — sharing a name: an ambiguous dispatch table is
/// worse than refusing to start, since silently picking one means commands would
/// dispatch to a skill the operator didn't necessarily intend, with no visible error
/// at all.
#[derive(Debug)]
pub struct SkillRegistry {
    skills: HashMap<String, Skill>,
    aliases: HashMap<String, String>,
}

impl SkillRegistry {
    pub fn load(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create skills dir at {}", dir.display()))?;

        let mut skills = HashMap::new();

        let entries = std::fs::read_dir(dir)
            .with_context(|| format!("failed to read skills dir at {}", dir.display()))?;

        for entry in entries {
            let entry = entry.with_context(|| format!("failed to read entry in {}", dir.display()))?;
            let path = entry.path();

            if !path.is_dir() {
                continue;
            }

            let skill_file = path.join(SKILL_FILE_NAME);
            if !skill_file.is_file() {
                warn!(dir = %path.display(), "skipping skill dir with no SKILL.md");
                continue;
            }

            let skill = match load_skill_file(&skill_file) {
                Ok(skill) => skill,
                Err(err) => {
                    warn!(?err, file = %skill_file.display(), "skipping invalid skill file");
                    continue;
                }
            };

            let key = skill.name.to_lowercase();
            if let Some(existing) = skills.insert(key, skill) {
                anyhow::bail!(
                    "duplicate skill name {:?} (already loaded from a different directory) — \
                     rename one of them so command dispatch isn't ambiguous",
                    existing.name
                );
            }
        }

        // Second pass: register aliases now that every skill's canonical name is known,
        // so a collision is caught regardless of directory iteration order.
        let mut aliases = HashMap::new();
        for skill in skills.values() {
            for alias in &skill.aliases {
                let key = alias.to_lowercase();
                anyhow::ensure!(
                    !skills.contains_key(&key),
                    "alias {alias:?} on skill {:?} collides with another skill's name",
                    skill.name
                );
                if let Some(owner) = aliases.insert(key, skill.name.to_lowercase())
                    && owner != skill.name.to_lowercase()
                {
                    anyhow::bail!(
                        "alias {alias:?} is used by both {:?} and {:?} — rename one",
                        owner,
                        skill.name
                    );
                }
            }
        }

        Ok(Self { skills, aliases })
    }

    /// Looks up a skill by name or alias, case-insensitively.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        let key = name.to_lowercase();
        self.skills
            .get(&key)
            .or_else(|| self.aliases.get(&key).and_then(|canonical| self.skills.get(canonical)))
    }

    /// All loaded skills, sorted by name — used by the `help` command and the status
    /// server's `/api/skills`.
    pub fn list(&self) -> Vec<&Skill> {
        let mut skills: Vec<&Skill> = self.skills.values().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }
}

fn load_skill_file(path: &Path) -> Result<Skill> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    parse_skill(&raw)
}

/// Parses a `SKILL.md` file's contents: a YAML frontmatter block delimited by `---`
/// lines, followed by the prompt body.
fn parse_skill(raw: &str) -> Result<Skill> {
    let mut lines = raw.lines();

    loop {
        match lines.next() {
            Some(line) if line.trim().is_empty() => continue,
            Some(line) if line.trim() == FRONTMATTER_DELIMITER => break,
            Some(other) => anyhow::bail!("expected `---` to open frontmatter, found {other:?}"),
            None => anyhow::bail!("missing frontmatter delimiter (file is empty or blank)"),
        }
    }

    let mut frontmatter_lines = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == FRONTMATTER_DELIMITER {
            closed = true;
            break;
        }
        frontmatter_lines.push(line);
    }
    anyhow::ensure!(closed, "unterminated frontmatter (missing closing `---`)");

    let frontmatter: SkillFrontmatter = serde_yaml::from_str(&frontmatter_lines.join("\n"))
        .context("failed to parse frontmatter")?;

    anyhow::ensure!(!frontmatter.name.trim().is_empty(), "skill name must not be empty");
    anyhow::ensure!(
        !frontmatter.name.eq_ignore_ascii_case(RESERVED_NAME),
        "skill name {:?} is reserved for the built-in help command",
        frontmatter.name
    );
    for tool in &frontmatter.tools {
        anyhow::ensure!(
            KNOWN_TOOLS.contains(&tool.as_str()),
            "unknown tool {:?} (known tools: {:?})",
            tool,
            KNOWN_TOOLS
        );
    }
    for alias in &frontmatter.aliases {
        anyhow::ensure!(!alias.trim().is_empty(), "alias must not be empty");
        anyhow::ensure!(
            !alias.eq_ignore_ascii_case(RESERVED_NAME),
            "alias {alias:?} is reserved for the built-in help command"
        );
        anyhow::ensure!(
            !alias.eq_ignore_ascii_case(&frontmatter.name),
            "alias {alias:?} duplicates the skill's own name"
        );
    }

    let mut seen_arg_names = HashSet::new();
    for arg in &frontmatter.args {
        anyhow::ensure!(!arg.name.trim().is_empty(), "argument name must not be empty");
        anyhow::ensure!(seen_arg_names.insert(arg.name.clone()), "duplicate argument name {:?}", arg.name);
        if arg.min.is_some() || arg.max.is_some() {
            anyhow::ensure!(
                matches!(arg.kind, ArgType::Integer | ArgType::Number),
                "argument {:?} declares min/max but its type ({}) isn't numeric",
                arg.name,
                arg.kind.label()
            );
        }
        if let (Some(min), Some(max)) = (arg.min, arg.max) {
            anyhow::ensure!(min <= max, "argument {:?} has min ({min}) greater than max ({max})", arg.name);
        }
        if let Some(default) = &arg.default {
            anyhow::ensure!(
                arg.kind.matches(default),
                "argument {:?} default value {default} does not match its declared type ({})",
                arg.name,
                arg.kind.label()
            );
        }
    }

    let prompt: String = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    anyhow::ensure!(!prompt.is_empty(), "skill prompt body must not be empty");

    Ok(Skill {
        name: frontmatter.name,
        description: frontmatter.description,
        usage: frontmatter.usage,
        tools: frontmatter.tools,
        aliases: frontmatter.aliases,
        model: frontmatter.model,
        args: frontmatter.args,
        prompt,
    })
}

/// Validates and defaults `provided` against `specs`, returning a resolved args map on
/// success or a user-facing error message (missing required argument, wrong type, or
/// out-of-range value) on failure. Called only when a skill declares `args` at all —
/// skills that don't are passed the classifier's raw extracted args unvalidated,
/// preserving the original free-form behavior.
fn resolve_args(
    specs: &[ArgSpec],
    provided: &serde_json::Value,
) -> std::result::Result<serde_json::Map<String, serde_json::Value>, String> {
    let provided_obj = provided.as_object();
    let mut resolved = serde_json::Map::new();

    for spec in specs {
        let value = provided_obj.and_then(|obj| obj.get(&spec.name));
        match value {
            Some(value) => {
                if !spec.kind.matches(value) {
                    return Err(format!("argument '{}' must be a {}, got {value}", spec.name, spec.kind.label()));
                }
                if let Some(n) = value.as_f64() {
                    if let Some(min) = spec.min
                        && n < min
                    {
                        return Err(format!("argument '{}' must be at least {min}", spec.name));
                    }
                    if let Some(max) = spec.max
                        && n > max
                    {
                        return Err(format!("argument '{}' must be at most {max}", spec.name));
                    }
                }
                resolved.insert(spec.name.clone(), value.clone());
            }
            None if spec.required => {
                return Err(format!("missing required argument '{}'", spec.name));
            }
            None => {
                if let Some(default) = &spec.default {
                    resolved.insert(spec.name.clone(), default.clone());
                }
            }
        }
    }

    Ok(resolved)
}

/// Runs a skill: sends its prompt as the system prompt to Claude (using `skill.model`
/// if set, else `classify::MODEL`), along with the user's raw message text, validated
/// command arguments, and any context its declared `tools` ask for (recent room
/// history for `message_log`, room name/topic for `room_info`, the current UTC time
/// for `current_time`) — and returns the generated reply.
///
/// Never propagates an error up into the message-send path — on any failure this
/// logs a `tracing::warn!` and returns a friendly fallback string instead, matching
/// how `classify_message` failures are already handled one level up in `on_room_message`.
pub async fn execute(
    anthropic: &Anthropic,
    message_log: &MessageLogger,
    room: &Room,
    skill: &Skill,
    message_text: &str,
    command: &CommandInfo,
) -> String {
    let resolved_args = if skill.args.is_empty() {
        command.args.as_object().cloned().unwrap_or_default()
    } else {
        match resolve_args(&skill.args, &command.args) {
            Ok(resolved) => resolved,
            Err(message) => return format!("Invalid arguments: {message}"),
        }
    };

    let room_id = room.room_id().as_str();
    let mut user_turn = String::new();

    if skill.tools.iter().any(|tool| tool == "message_log") {
        let count = resolved_args
            .get("count")
            .and_then(|value| value.as_u64())
            .map(|count| count as usize)
            .unwrap_or(DEFAULT_MESSAGE_LOG_COUNT)
            .clamp(1, MAX_MESSAGE_LOG_COUNT);

        match message_log.recent(room_id, count) {
            Ok(entries) if entries.is_empty() => {
                user_turn.push_str("Recent messages in this room: (none logged yet)\n\n");
            }
            Ok(entries) => {
                user_turn.push_str(
                    "Recent messages in this room (oldest first; sentiment and any \
                     tagged entities from classification are shown in brackets):\n",
                );
                for entry in &entries {
                    user_turn.push_str(&format!("- {}{}: {}\n", entry.sender, format_entry_tags(entry), entry.body));
                }
                user_turn.push('\n');
            }
            Err(err) => {
                warn!(?err, room_id, skill = %skill.name, "failed to read message log for skill");
            }
        }
    }

    if skill.tools.iter().any(|tool| tool == "room_info") {
        user_turn.push_str(&format!(
            "Room info: id={room_id}, name={}, topic={}\n\n",
            room.name().as_deref().unwrap_or("(none)"),
            room.topic().as_deref().unwrap_or("(none)"),
        ));
    }

    if skill.tools.iter().any(|tool| tool == "current_time") {
        user_turn.push_str(&format!("Current time (UTC): {}\n\n", chrono::Utc::now().to_rfc3339()));
    }

    user_turn.push_str(&format!("Message: {message_text}"));
    if !resolved_args.is_empty() {
        let args_value = serde_json::Value::Object(resolved_args);
        user_turn.push_str(&format!("\nParsed command arguments: {args_value}"));
    }

    let model = skill.model.as_deref().unwrap_or(classify::MODEL);

    match run_prompt(anthropic, model, &skill.prompt, &user_turn).await {
        Ok(reply) => reply,
        Err(err) => {
            warn!(?err, skill = %skill.name, "skill prompt failed");
            "Sorry, that command failed.".to_string()
        }
    }
}

/// Formats a logged message's sentiment and any tagged entities (already computed by
/// the classifier) as a trailing `[...]` tag, e.g. `[positive, entities: topic:launch]`
/// — so skills like `vibe`/`topics` can reason about mood or themes across recent
/// messages without a separate tool or LLM call.
fn format_entry_tags(entry: &LoggedMessage) -> String {
    let mut tags = format!("{:?}", entry.analysis.sentiment).to_lowercase();
    if !entry.analysis.entities.is_empty() {
        let entities: Vec<String> = entry
            .analysis
            .entities
            .iter()
            .map(|entity| format!("{}:{}", format!("{:?}", entity.kind).to_lowercase(), entity.value))
            .collect();
        tags.push_str(&format!(", entities: {}", entities.join(", ")));
    }
    format!(" [{tags}]")
}

/// A plain (non-tool-forced) completion call: `system` becomes the skill's prompt,
/// `user_message` the constructed user turn, and the first text block of the
/// response is returned. Unlike `classify_message`, this omits `.tools()`/
/// `.tool_choice()` entirely — there's no schema to fill in, just a generated reply.
async fn run_prompt(client: &Anthropic, model: &str, system: &str, user_message: &str) -> Result<String> {
    let params = MessageCreateBuilder::new(model, 1024)
        .system(system)
        .user(user_message)
        .build();

    let message = client
        .messages()
        .create(params)
        .await
        .context("skill prompt request to Claude failed")?;

    for block in message.content {
        if let ContentBlock::Text { text } = block {
            return Ok(text);
        }
    }

    anyhow::bail!("model response did not include a text block")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("matrix-llm-bot-test-{label}-{nanos}"))
    }

    fn write_skill(dir: &Path, name: &str, contents: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join(SKILL_FILE_NAME), contents).unwrap();
    }

    const VALID_SKILL: &str = "---\n\
name: greet\n\
description: Say hello.\n\
usage: \"greet\"\n\
---\n\
Say a friendly hello to the user.\n";

    #[test]
    fn parses_valid_frontmatter_and_body() {
        let skill = parse_skill(VALID_SKILL).expect("should parse");
        assert_eq!(skill.name, "greet");
        assert_eq!(skill.description, "Say hello.");
        assert_eq!(skill.usage.as_deref(), Some("greet"));
        assert!(skill.tools.is_empty());
        assert!(skill.aliases.is_empty());
        assert!(skill.model.is_none());
        assert_eq!(skill.prompt, "Say a friendly hello to the user.");
    }

    #[test]
    fn parses_flow_style_tools_list() {
        let raw = "---\nname: x\ndescription: d\ntools: [message_log]\n---\nbody\n";
        let skill = parse_skill(raw).expect("should parse");
        assert_eq!(skill.tools, vec!["message_log".to_string()]);
    }

    #[test]
    fn parses_new_tool_names() {
        let raw = "---\nname: x\ndescription: d\ntools: [room_info, current_time]\n---\nbody\n";
        let skill = parse_skill(raw).expect("should parse");
        assert_eq!(skill.tools, vec!["room_info".to_string(), "current_time".to_string()]);
    }

    #[test]
    fn handles_crlf_line_endings() {
        let raw = "---\r\nname: x\r\ndescription: d\r\n---\r\nbody\r\n";
        let skill = parse_skill(raw).expect("should parse");
        assert_eq!(skill.name, "x");
        assert_eq!(skill.prompt, "body");
    }

    #[test]
    fn rejects_missing_opening_delimiter() {
        let err = parse_skill("name: x\ndescription: d\n---\nbody\n").unwrap_err();
        assert!(err.to_string().contains("expected `---`"), "{err}");
    }

    #[test]
    fn rejects_unterminated_frontmatter() {
        let err = parse_skill("---\nname: x\ndescription: d\nbody with no closing delimiter\n")
            .unwrap_err();
        assert!(err.to_string().contains("unterminated"), "{err}");
    }

    #[test]
    fn rejects_missing_required_fields() {
        let err = parse_skill("---\nname: x\n---\nbody\n").unwrap_err();
        assert!(err.to_string().contains("failed to parse frontmatter"), "{err}");
    }

    #[test]
    fn rejects_unknown_tool() {
        let raw = "---\nname: x\ndescription: d\ntools:\n  - not_a_real_tool\n---\nbody\n";
        let err = parse_skill(raw).unwrap_err();
        assert!(err.to_string().contains("unknown tool"), "{err}");
    }

    #[test]
    fn rejects_empty_prompt_body() {
        let err = parse_skill("---\nname: x\ndescription: d\n---\n   \n").unwrap_err();
        assert!(err.to_string().contains("prompt body must not be empty"), "{err}");
    }

    #[test]
    fn rejects_reserved_help_name() {
        let raw = "---\nname: HELP\ndescription: d\n---\nbody\n";
        let err = parse_skill(raw).unwrap_err();
        assert!(err.to_string().contains("reserved"), "{err}");
    }

    #[test]
    fn parses_model_override() {
        let raw = "---\nname: x\ndescription: d\nmodel: claude-sonnet-5\n---\nbody\n";
        let skill = parse_skill(raw).expect("should parse");
        assert_eq!(skill.model.as_deref(), Some("claude-sonnet-5"));
    }

    #[test]
    fn parses_aliases() {
        let raw = "---\nname: history\ndescription: d\naliases:\n  - recent\n  - hist\n---\nbody\n";
        let skill = parse_skill(raw).expect("should parse");
        assert_eq!(skill.aliases, vec!["recent".to_string(), "hist".to_string()]);
    }

    #[test]
    fn rejects_alias_matching_own_name() {
        let raw = "---\nname: history\ndescription: d\naliases:\n  - HISTORY\n---\nbody\n";
        let err = parse_skill(raw).unwrap_err();
        assert!(err.to_string().contains("duplicates the skill's own name"), "{err}");
    }

    #[test]
    fn rejects_reserved_alias() {
        let raw = "---\nname: x\ndescription: d\naliases:\n  - help\n---\nbody\n";
        let err = parse_skill(raw).unwrap_err();
        assert!(err.to_string().contains("reserved"), "{err}");
    }

    #[test]
    fn rejects_duplicate_arg_name() {
        let raw = "---\nname: x\ndescription: d\nargs:\n  - name: count\n    type: integer\n  - name: count\n    type: string\n---\nbody\n";
        let err = parse_skill(raw).unwrap_err();
        assert!(err.to_string().contains("duplicate argument name"), "{err}");
    }

    #[test]
    fn rejects_min_max_on_non_numeric_arg() {
        let raw = "---\nname: x\ndescription: d\nargs:\n  - name: q\n    type: string\n    min: 1\n---\nbody\n";
        let err = parse_skill(raw).unwrap_err();
        assert!(err.to_string().contains("isn't numeric"), "{err}");
    }

    #[test]
    fn rejects_default_type_mismatch() {
        let raw = "---\nname: x\ndescription: d\nargs:\n  - name: count\n    type: integer\n    default: \"five\"\n---\nbody\n";
        let err = parse_skill(raw).unwrap_err();
        assert!(err.to_string().contains("does not match its declared type"), "{err}");
    }

    #[test]
    fn load_skips_invalid_skills_but_keeps_valid_ones() {
        let dir = unique_temp_dir("skills-mixed");
        write_skill(&dir, "greet", VALID_SKILL);
        write_skill(&dir, "broken", "not frontmatter at all\n");
        write_skill(&dir, "help", "---\nname: help\ndescription: d\n---\nbody\n");
        std::fs::create_dir_all(dir.join("no-skill-file")).unwrap();

        let registry = SkillRegistry::load(&dir).expect("load should succeed despite bad entries");
        assert_eq!(registry.list().len(), 1);
        assert!(registry.get("greet").is_some());
        assert!(registry.get("GREET").is_some(), "lookup should be case-insensitive");
        assert!(registry.get("broken").is_none());
        assert!(registry.get("help").is_none(), "help is reserved, not a loadable skill");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_fails_on_duplicate_names() {
        let dir = unique_temp_dir("skills-dupe");
        write_skill(&dir, "one", VALID_SKILL);
        write_skill(&dir, "two", VALID_SKILL); // same `name: greet` inside

        let err = SkillRegistry::load(&dir).unwrap_err();
        assert!(err.to_string().contains("duplicate skill name"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_creates_missing_directory() {
        let dir = unique_temp_dir("skills-missing");
        assert!(!dir.exists());

        let registry = SkillRegistry::load(&dir).expect("load should create the dir");
        assert!(dir.exists());
        assert!(registry.list().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_is_sorted_by_name() {
        let dir = unique_temp_dir("skills-sorted");
        write_skill(&dir, "zzz", "---\nname: zzz\ndescription: d\n---\nbody\n");
        write_skill(&dir, "aaa", "---\nname: aaa\ndescription: d\n---\nbody\n");

        let registry = SkillRegistry::load(&dir).expect("load");
        let names: Vec<&str> = registry.list().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["aaa", "zzz"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_resolves_alias_to_canonical_skill() {
        let dir = unique_temp_dir("skills-alias");
        write_skill(
            &dir,
            "history",
            "---\nname: history\ndescription: d\naliases:\n  - recent\n---\nbody\n",
        );

        let registry = SkillRegistry::load(&dir).expect("load");
        assert!(registry.get("recent").is_some());
        assert!(registry.get("RECENT").is_some(), "alias lookup should be case-insensitive");
        assert_eq!(registry.get("recent").unwrap().name, "history");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_fails_when_alias_collides_with_a_skill_name() {
        let dir = unique_temp_dir("skills-alias-collides-name");
        write_skill(&dir, "a", "---\nname: a\ndescription: d\naliases:\n  - b\n---\nbody\n");
        write_skill(&dir, "b", "---\nname: b\ndescription: d\n---\nbody\n");

        let err = SkillRegistry::load(&dir).unwrap_err();
        assert!(err.to_string().contains("collides with another skill's name"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_fails_when_two_skills_share_an_alias() {
        let dir = unique_temp_dir("skills-alias-collides-alias");
        write_skill(&dir, "a", "---\nname: a\ndescription: d\naliases:\n  - x\n---\nbody\n");
        write_skill(&dir, "b", "---\nname: b\ndescription: d\naliases:\n  - x\n---\nbody\n");

        let err = SkillRegistry::load(&dir).unwrap_err();
        assert!(err.to_string().contains("is used by both"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_args_fills_defaults_and_validates_bounds() {
        let specs = vec![ArgSpec {
            name: "count".to_string(),
            kind: ArgType::Integer,
            description: None,
            required: false,
            default: Some(serde_json::json!(5)),
            min: Some(1.0),
            max: Some(20.0),
        }];

        let resolved = resolve_args(&specs, &serde_json::Value::Null).expect("defaults should apply");
        assert_eq!(resolved.get("count"), Some(&serde_json::json!(5)));

        let resolved = resolve_args(&specs, &serde_json::json!({"count": 10})).expect("valid value");
        assert_eq!(resolved.get("count"), Some(&serde_json::json!(10)));

        let err = resolve_args(&specs, &serde_json::json!({"count": 100})).unwrap_err();
        assert!(err.contains("at most 20"), "{err}");

        let err = resolve_args(&specs, &serde_json::json!({"count": "many"})).unwrap_err();
        assert!(err.contains("must be a integer") || err.contains("must be a"), "{err}");
    }

    #[test]
    fn resolve_args_reports_missing_required_argument() {
        let specs = vec![ArgSpec {
            name: "query".to_string(),
            kind: ArgType::String,
            description: None,
            required: true,
            default: None,
            min: None,
            max: None,
        }];

        let err = resolve_args(&specs, &serde_json::Value::Null).unwrap_err();
        assert!(err.contains("missing required argument 'query'"), "{err}");
    }

    /// Regression test for the example skills shipped in the repo's `skills/`
    /// directory — catches a shipped `SKILL.md` silently rotting (e.g. after a
    /// `KNOWN_TOOLS`/`ArgType` rename) without needing a live Anthropic API call.
    #[test]
    fn shipped_example_skills_load_successfully() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
        let registry = SkillRegistry::load(&dir).expect("shipped skills dir should load");

        let history = registry.get("history").expect("history skill should be present");
        assert_eq!(history.name, "history");
        assert!(history.tools.iter().any(|tool| tool == "message_log"));
        assert!(registry.get("recent").is_some(), "history's `recent` alias should resolve");

        let room = registry.get("room").expect("room skill should be present");
        assert!(room.tools.iter().any(|tool| tool == "room_info"));
        assert!(room.tools.iter().any(|tool| tool == "current_time"));

        assert!(registry.get("digest").is_some(), "digest skill should be present");
        assert!(registry.get("summary").is_some(), "digest's `summary` alias should resolve");
        assert!(registry.get("vibe").is_some(), "vibe skill should be present");
        assert!(registry.get("topics").is_some(), "topics skill should be present");
        assert!(registry.get("standup").is_some(), "standup skill should be present");

        let define = registry.get("define").expect("define skill should be present");
        assert!(define.tools.is_empty(), "define should be a pure-prompt skill with no tools");

        assert_eq!(registry.list().len(), 7, "expected all seven shipped skills to load");
    }
}
