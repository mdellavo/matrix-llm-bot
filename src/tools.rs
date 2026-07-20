use std::collections::HashMap;

use anyhow::{Context, Result};
use rand::seq::IndexedRandom;
use serde::Deserialize;

const UD_DEFINE_URL: &str = "https://api.urbandictionary.com/v0/define";
const UD_RANDOM_URL: &str = "https://api.urbandictionary.com/v0/random";
const OMDB_URL: &str = "http://www.omdbapi.com/";
const LEAFLY_SEARCH_URL: &str = "https://consumer-api.leafly.com/api/search/v1";
const LEAFLY_STRAIN_URL_PREFIX: &str = "https://www.leafly.com/strains/";

/// Shared HTTP client (built once, not per call — reqwest's own guidance) and the
/// optional OMDb API key, for the tool-calling skills (`ud`, `imdb`, `strain`).
/// Built once in `Bot::new` and registered as event-handler context, mirroring
/// `MessageLogger`'s `Arc`-registration pattern.
pub struct ToolClients {
    http: reqwest::Client,
    omdb_api_key: Option<String>,
}

impl ToolClients {
    /// `omdb_api_key` is normalized so a blank/whitespace-only configured key is
    /// treated the same as an absent one.
    pub fn new(omdb_api_key: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client with a fixed timeout should always build");
        Self {
            http,
            omdb_api_key: omdb_api_key.filter(|key| !key.trim().is_empty()),
        }
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn has_omdb_key(&self) -> bool {
        self.omdb_api_key.is_some()
    }

    /// Panics if `has_omdb_key()` is false — callers must check first (see
    /// `skills::execute`'s `imdb_lookup` dispatch block).
    pub fn omdb_api_key(&self) -> &str {
        self.omdb_api_key.as_deref().expect("caller must check has_omdb_key() first")
    }
}

/// Picks one of `choices` uniformly at random and formats a context line for Claude
/// to phrase naturally. Empty input returns a "no choices" line rather than
/// panicking — defense in depth; `random`'s `SKILL.md` declares `min: 2` on
/// `choices`, so `resolve_args` should already have rejected an empty/too-short
/// list before this is ever called.
pub fn random_choice_context(choices: &[String]) -> String {
    match choices.choose(&mut rand::rng()) {
        Some(choice) => format!("Randomly selected: {choice}\n\n"),
        None => "No choices were given to pick from.\n\n".to_string(),
    }
}

#[derive(Debug, Deserialize)]
struct UdResponse {
    #[serde(default)]
    list: Vec<UdEntry>,
}

#[derive(Debug, Deserialize)]
struct UdEntry {
    word: String,
    definition: String,
}

/// `term = None` hits Urban Dictionary's own `/v0/random` endpoint (its native
/// random-word behavior, not a bot-side default) — see the `ud` skill's `term` arg,
/// which has no `default:` for exactly this reason.
///
/// `Ok(_)` covers both "found" and "the API responded but had no results" — only a
/// transport/HTTP-status/JSON-parse failure is `Err`.
pub async fn urban_dictionary(http: &reqwest::Client, term: Option<&str>) -> Result<String> {
    let response = match term {
        Some(term) => http.get(UD_DEFINE_URL).query(&[("term", term)]).send().await,
        None => http.get(UD_RANDOM_URL).send().await,
    }
    .context("urban dictionary request failed")?
    .error_for_status()
    .context("urban dictionary returned an error status")?;

    let parsed: UdResponse = response.json().await.context("failed to parse urban dictionary response")?;

    match parsed.list.first() {
        Some(entry) => Ok(format!("Urban Dictionary result for \"{}\":\n{}\n\n", entry.word, entry.definition)),
        None => Ok(format!(
            "No Urban Dictionary result was found{}.\n\n",
            term.map(|t| format!(" for \"{t}\"")).unwrap_or_default()
        )),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct OmdbResponse {
    title: Option<String>,
    year: Option<String>,
    plot: Option<String>,
    poster: Option<String>,
    #[serde(rename = "imdbID")]
    imdb_id: Option<String>,
    response: Option<String>,
    error: Option<String>,
}

/// Looks up a title via OMDb's `t=` (best-title-match) endpoint rather than `s=`
/// (search-list): the skill wants a single top result to summarize, and `t=`
/// returns full details in one call.
///
/// Caller (`skills::execute`) checks `ToolClients::has_omdb_key` before calling
/// this and short-circuits with a "not configured" reply if absent.
pub async fn imdb_lookup(http: &reqwest::Client, api_key: &str, title: &str) -> Result<String> {
    let response = http
        .get(OMDB_URL)
        .query(&[("apikey", api_key), ("t", title)])
        .send()
        .await
        .context("OMDb request failed")?
        .error_for_status()
        .context("OMDb returned an error status")?;

    let parsed: OmdbResponse = response.json().await.context("failed to parse OMDb response")?;

    if parsed.response.as_deref() == Some("False") {
        return Ok(format!(
            "No IMDb match was found for \"{title}\"{}.\n\n",
            parsed.error.map(|e| format!(" ({e})")).unwrap_or_default()
        ));
    }

    Ok(format!(
        "IMDb result for \"{title}\":\nTitle: {}\nYear: {}\nPlot: {}\nPoster: {}\nIMDb: {}\n\n",
        parsed.title.as_deref().unwrap_or("(unknown)"),
        parsed.year.as_deref().unwrap_or("(unknown)"),
        parsed.plot.as_deref().unwrap_or("(none provided)"),
        parsed.poster.as_deref().unwrap_or("(none)"),
        parsed.imdb_id.map(|id| format!("https://www.imdb.com/title/{id}/")).unwrap_or_else(|| "(none)".to_string()),
    ))
}

#[derive(Debug, Default, Deserialize)]
struct LeaflyResponse {
    #[serde(default)]
    hits: LeaflyHits,
}

#[derive(Debug, Default, Deserialize)]
struct LeaflyHits {
    #[serde(default)]
    strain: Vec<LeaflyStrain>,
}

#[derive(Debug, Clone, Deserialize)]
struct LeaflyStrain {
    name: Option<String>,
    subtitle: Option<String>,
    slug: Option<String>,
    phenotype: Option<String>,
    category: Option<String>,
    #[serde(rename = "topEffect")]
    top_effect: Option<String>,
    #[serde(rename = "shortDescriptionPlain")]
    short_description_plain: Option<String>,
    #[serde(rename = "nugImage")]
    nug_image: Option<String>,
    #[serde(default)]
    cannabinoids: HashMap<String, LeaflyCannabinoid>,
    #[serde(default)]
    effects: HashMap<String, LeaflyScoredTrait>,
    #[serde(default)]
    terps: HashMap<String, LeaflyScoredTrait>,
}

/// One entry of the strain's `cannabinoids` map (keyed `"thc"`/`"cbd"`/etc.) — only
/// the median (`percentile50`) potency is surfaced, everything else on this API
/// response is a range Claude doesn't need.
#[derive(Debug, Clone, Deserialize)]
struct LeaflyCannabinoid {
    percentile50: Option<f64>,
}

/// One entry of the strain's `effects` or `terps` map — a named trait (e.g. an
/// effect like "relaxed" or a terpene like "myrcene") with a relative strength
/// `score`, used to rank which are most worth mentioning.
#[derive(Debug, Clone, Deserialize)]
struct LeaflyScoredTrait {
    name: String,
    score: Option<f64>,
}

/// Names of the `n` highest-`score` entries in an effects/terpenes map, for
/// picking out what's actually worth mentioning rather than dumping the whole map.
/// Ties (and missing scores, sorted last) break on name for determinism.
fn top_traits(traits: &HashMap<String, LeaflyScoredTrait>, n: usize) -> Vec<String> {
    let mut sorted: Vec<&LeaflyScoredTrait> = traits.values().collect();
    sorted.sort_by(|a, b| {
        b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.name.cmp(&b.name))
    });
    sorted.into_iter().take(n).map(|t| t.name.clone()).collect()
}

/// Faithful port of gordy's `split_akas`: strips a leading `"aka "` label off a
/// strain's `subtitle` and splits the remainder on `", "`.
///
/// Gordy's Python used `subtitle.lstrip("aka ")`, which — a well-known `str.lstrip`
/// gotcha — strips any leading run of the *characters* `a`/`k`/`' '`, not the
/// literal 4-character prefix. That's almost certainly an accident, not intended
/// behavior, so this is a literal `strip_prefix("aka ")` instead: strictly safer,
/// since it can't over-strip a subtitle that happens to start with those
/// characters for unrelated reasons. Deliberate deviation, not a faithfulness gap.
fn split_akas(subtitle: &str) -> Vec<String> {
    subtitle.strip_prefix("aka ").unwrap_or(subtitle).split(", ").map(str::to_string).collect()
}

fn strain_matches(strain: &LeaflyStrain, query: &str) -> bool {
    let name = strain.name.as_deref().unwrap_or("");
    let subtitle = strain.subtitle.as_deref().unwrap_or("");
    std::iter::once(name.to_string()).chain(split_akas(subtitle)).any(|candidate| candidate.eq_ignore_ascii_case(query))
}

/// Faithful port of gordy's `match()` disambiguation loop: among the (up to 5)
/// search hits, keeps the LAST one whose name or any parsed AKA exactly
/// (case-insensitively) matches `query` — NOT the first. This is intentional in
/// gordy (its author added it specifically because naive first-hit selection was
/// wrong for aka'd strain names), so it's ported unchanged, not "fixed."
fn select_strain<'a>(strains: &'a [LeaflyStrain], query: &str) -> Option<&'a LeaflyStrain> {
    strains.iter().rev().find(|strain| strain_matches(strain, query))
}

pub async fn leafly_strain(http: &reqwest::Client, query: &str) -> Result<String> {
    let response = http
        .get(LEAFLY_SEARCH_URL)
        .query(&[("q", query), ("filter[all_strains]", "true"), ("take", "5")])
        .send()
        .await
        .context("Leafly request failed")?
        .error_for_status()
        .context("Leafly returned an error status")?;

    let parsed: LeaflyResponse = response.json().await.context("failed to parse Leafly response")?;

    match select_strain(&parsed.hits.strain, query) {
        Some(strain) => Ok(format_strain_context(query, strain)),
        None => Ok(format!("No exact Leafly strain match was found for \"{query}\" among the top results.\n\n")),
    }
}

/// Renders the data Claude needs to write a short, vivid strain summary — a
/// condensed set of facts (not the full API response) for `skills/strain/SKILL.md`
/// to draw the reply from. `short_description_plain` is only included when
/// Leafly actually provided one; the prompt is expected to compose its own
/// description from the other fields either way (Leafly leaves this blank for
/// plenty of strains, e.g. less common ones).
fn format_strain_context(query: &str, strain: &LeaflyStrain) -> String {
    let name = strain.name.as_deref().unwrap_or("(unknown)");
    let url = format!("{LEAFLY_STRAIN_URL_PREFIX}{}", strain.slug.as_deref().unwrap_or(""));
    let category = strain.category.as_deref().or(strain.phenotype.as_deref()).unwrap_or("(unknown)");
    let top_effect = strain.top_effect.as_deref().unwrap_or("(unknown)");
    let thc = strain
        .cannabinoids
        .get("thc")
        .and_then(|c| c.percentile50)
        .map(|pct| format!("{pct:.1}%"))
        .unwrap_or_else(|| "(unknown)".to_string());
    let cbd = strain
        .cannabinoids
        .get("cbd")
        .and_then(|c| c.percentile50)
        .map(|pct| format!("{pct:.1}%"))
        .unwrap_or_else(|| "(unknown)".to_string());
    let effects = top_traits(&strain.effects, 3);
    let effects_line = if effects.is_empty() { "(none reported)".to_string() } else { effects.join(", ") };
    let terps = top_traits(&strain.terps, 3);
    let terps_line = if terps.is_empty() { "(none reported)".to_string() } else { terps.join(", ") };
    let image = strain.nug_image.as_deref().unwrap_or("(none)");

    let mut context = format!(
        "Leafly strain result for \"{query}\" (exact match):\n\
         Name: {name}\n\
         Leafly URL: {url}\n\
         Category: {category}\n\
         Top reported effect: {top_effect}\n\
         THC: {thc} | CBD: {cbd}\n\
         Notable effects: {effects_line}\n\
         Notable aromas: {terps_line}\n\
         Image: {image}\n"
    );

    if let Some(description) = strain.short_description_plain.as_deref().filter(|d| !d.trim().is_empty()) {
        context.push_str(&format!("Leafly's own description: {description}\n"));
    }

    context.push('\n');
    context
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_choice_context_always_picks_a_member() {
        let choices = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        for _ in 0..100 {
            let context = random_choice_context(&choices);
            assert!(
                choices.iter().any(|choice| context.contains(choice.as_str())),
                "context {context:?} should mention one of {choices:?}"
            );
        }
    }

    #[test]
    fn random_choice_context_handles_empty_input() {
        let context = random_choice_context(&[]);
        assert!(context.contains("No choices"), "{context}");
    }

    #[test]
    fn split_akas_strips_prefix_and_splits() {
        assert_eq!(
            split_akas("aka Girl Scout Cookies, GSC"),
            vec!["Girl Scout Cookies".to_string(), "GSC".to_string()]
        );
    }

    #[test]
    fn split_akas_without_prefix_is_single_element() {
        assert_eq!(split_akas("Just A Subtitle"), vec!["Just A Subtitle".to_string()]);
    }

    #[test]
    fn split_akas_handles_empty_subtitle() {
        assert_eq!(split_akas(""), vec!["".to_string()]);
    }

    fn strain(name: &str, subtitle: &str) -> LeaflyStrain {
        LeaflyStrain {
            name: Some(name.to_string()),
            subtitle: Some(subtitle.to_string()),
            slug: Some(name.to_lowercase().replace(' ', "-")),
            phenotype: None,
            category: None,
            top_effect: None,
            short_description_plain: None,
            nug_image: None,
            cannabinoids: HashMap::new(),
            effects: HashMap::new(),
            terps: HashMap::new(),
        }
    }

    fn scored_trait(name: &str, score: f64) -> LeaflyScoredTrait {
        LeaflyScoredTrait { name: name.to_string(), score: Some(score) }
    }

    #[test]
    fn select_strain_matches_exact_name() {
        let strains = vec![strain("Blue Dream", ""), strain("Sour Diesel", "")];
        let selected = select_strain(&strains, "Sour Diesel").expect("should match by name");
        assert_eq!(selected.name.as_deref(), Some("Sour Diesel"));
    }

    #[test]
    fn select_strain_matches_aka() {
        let strains = vec![strain("Girl Scout Cookies", "aka GSC")];
        let selected = select_strain(&strains, "GSC").expect("should match by aka");
        assert_eq!(selected.name.as_deref(), Some("Girl Scout Cookies"));
    }

    #[test]
    fn select_strain_is_case_insensitive() {
        let strains = vec![strain("Blue Dream", "")];
        assert!(select_strain(&strains, "blue dream").is_some());
    }

    #[test]
    fn select_strain_keeps_last_match_among_multiple_hits() {
        let strains = vec![strain("Og Kush", "aka OG"), strain("Original Gangster", "aka OG")];
        let selected = select_strain(&strains, "OG").expect("should match one of the two");
        assert_eq!(
            selected.name.as_deref(),
            Some("Original Gangster"),
            "should keep the LAST matching hit, not the first — faithful port of gordy's quirk"
        );
    }

    #[test]
    fn select_strain_returns_none_when_no_hit_matches() {
        let strains = vec![strain("Blue Dream", ""), strain("Sour Diesel", "")];
        assert!(select_strain(&strains, "Purple Haze").is_none());
    }

    #[test]
    fn top_traits_ranks_by_score_descending() {
        let mut traits = HashMap::new();
        traits.insert("relaxed".to_string(), scored_trait("relaxed", 1.5));
        traits.insert("happy".to_string(), scored_trait("happy", 2.5));
        traits.insert("sleepy".to_string(), scored_trait("sleepy", 0.5));
        assert_eq!(top_traits(&traits, 2), vec!["happy".to_string(), "relaxed".to_string()]);
    }

    #[test]
    fn top_traits_handles_empty_map() {
        assert!(top_traits(&HashMap::new(), 3).is_empty());
    }

    #[test]
    fn format_strain_context_links_name_and_omits_missing_description() {
        let mut s = strain("Blue Dream", "");
        s.cannabinoids.insert("thc".to_string(), LeaflyCannabinoid { percentile50: Some(21.0) });
        s.effects.insert("happy".to_string(), scored_trait("happy", 2.0));

        let context = format_strain_context("Blue Dream", &s);
        assert!(context.contains("Leafly URL: https://www.leafly.com/strains/blue-dream"), "{context}");
        assert!(context.contains("THC: 21.0%"), "{context}");
        assert!(context.contains("happy"), "{context}");
        assert!(!context.contains("Leafly's own description"), "{context}");
    }

    #[test]
    fn format_strain_context_includes_description_when_present() {
        let mut s = strain("Blue Dream", "");
        s.short_description_plain = Some("A balanced hybrid.".to_string());

        let context = format_strain_context("Blue Dream", &s);
        assert!(context.contains("Leafly's own description: A balanced hybrid."), "{context}");
    }
}
