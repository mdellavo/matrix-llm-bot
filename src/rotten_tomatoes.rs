//! The `rotten_tomatoes` skill tool: a movie's or show's Tomatometer and Popcornmeter.
//! Rotten Tomatoes has no public API, so this reads its search page, then the chosen
//! title's page, whose scorecard is embedded as JSON. Both are its own server-rendered
//! pages, so no browser is needed. Only Rotten Tomatoes URLs are ever fetched: the
//! title page's address comes from the search page, and is checked before it's used.

use anyhow::{Context, Result};
use tracing::warn;

use crate::links::{attribute, collapse_whitespace, decode_entities};

const SITE: &str = "https://www.rottentomatoes.com";
/// Other matches listed after the chosen one, so the reply can point out a remake or a
/// show of the same name.
const MAX_OTHER_MATCHES: usize = 4;
/// Title pages read to choose between titles with the same name.
const MAX_COMPARED: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Movie,
    Tv,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Movie => "movie",
            Kind::Tv => "TV series",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct SearchResult {
    kind: Kind,
    title: String,
    url: String,
    /// A movie's release year, or the year a show started.
    year: Option<u16>,
    /// The years a show ran, like `2022-2026`, or the movie's year.
    years: Option<String>,
    tomatometer: Option<u8>,
}

#[derive(Debug, Default, PartialEq)]
struct Scorecard {
    critic_reviews: Option<u64>,
    critics: Option<String>,
    top_critics: Option<String>,
    audience: Option<String>,
    consensus: Option<String>,
    description: Option<String>,
}

/// What a search found for a query.
#[derive(Debug, PartialEq)]
struct Matches {
    /// Indexes into the search results, in the search page's order.
    candidates: Vec<usize>,
    /// Whether the candidates have exactly the query's title, rather than being the
    /// closest thing found.
    exact: bool,
}

/// Looks `query` up and returns the context for the skill's prompt. A trailing year,
/// like `dune 1984` or `dune (1984)`, picks between titles of the same name.
pub async fn lookup(http: &reqwest::Client, query: &str) -> Result<String> {
    let query = query.trim();
    let mut found = None;
    if let Some((title, year)) = split_year(query) {
        let results = search(http, title).await?;
        let matched = matches(&results, title, Some(year));
        if matched.exact {
            found = Some((results, matched));
        }
    }
    // No year given, or no title of that year: a year can be part of a title, as in
    // "Wonder Woman 1984".
    let (results, matched) = match found {
        Some(found) => found,
        None => {
            let results = search(http, query).await?;
            let matched = matches(&results, query, None);
            (results, matched)
        }
    };
    if matched.candidates.is_empty() {
        return Ok(format!("No Rotten Tomatoes match was found for \"{query}\".\n\n"));
    }

    // Several titles share the name, like a show and its remakes. The one with the most
    // critic reviews is almost always the one people mean.
    let compared: Vec<usize> = matched.candidates.iter().copied().take(MAX_COMPARED).collect();
    let pages = futures_util::future::join_all(compared.iter().map(|&index| title_page(http, &results[index].url))).await;
    let (chosen, scorecard) = compared
        .into_iter()
        .zip(pages)
        .map(|(index, page)| match page {
            Ok(html) => (index, Some(scorecard(&html))),
            Err(err) => {
                warn!(?err, url = %results[index].url, "failed to read a Rotten Tomatoes title page");
                (index, None)
            }
        })
        .enumerate()
        // Earlier results win ties.
        .max_by_key(|(order, (_, card))| (card.as_ref().and_then(|card| card.critic_reviews), std::cmp::Reverse(*order)))
        .map(|(_, chosen)| chosen)
        .expect("candidates isn't empty");

    Ok(format_context(query, &results, chosen, matched.exact, scorecard.as_ref()))
}

async fn search(http: &reqwest::Client, title: &str) -> Result<Vec<SearchResult>> {
    let html = http
        .get(format!("{SITE}/search"))
        .query(&[("search", title)])
        .send()
        .await
        .context("Rotten Tomatoes search failed")?
        .error_for_status()
        .context("Rotten Tomatoes search returned an error status")?
        .text()
        .await
        .context("failed to read the Rotten Tomatoes search page")?;
    Ok(search_results(&html))
}

async fn title_page(http: &reqwest::Client, url: &str) -> Result<String> {
    http.get(url)
        .send()
        .await
        .context("Rotten Tomatoes title page request failed")?
        .error_for_status()
        .context("Rotten Tomatoes title page returned an error status")?
        .text()
        .await
        .context("failed to read the Rotten Tomatoes title page")
}

/// `dune 1984` or `dune (1984)` as the title and year. Only a plausible film year
/// counts, and never the whole query, since "1917" and "2012" are titles.
fn split_year(query: &str) -> Option<(&str, u16)> {
    let (title, last) = query.rsplit_once(char::is_whitespace)?;
    let year: u16 = last.trim_start_matches('(').trim_end_matches(')').parse().ok()?;
    let title = title.trim().trim_end_matches(',').trim();
    ((1880..=2100).contains(&year) && !title.is_empty()).then_some((title, year))
}

/// Movies and shows from the search page, in its order: movies first, then shows.
fn search_results(html: &str) -> Vec<SearchResult> {
    let lower = html.to_ascii_lowercase();
    let mut results = Vec::new();

    for (section, section_end) in elements(&lower, "search-page-result", 0, lower.len()) {
        let open_end = section + lower[section..].find('>').unwrap_or(0);
        let kind = match attribute(&html[section..open_end], &lower[section..open_end], "type") {
            Some("movie") => Kind::Movie,
            Some("tvSeries") => Kind::Tv,
            _ => continue,
        };
        for (row, row_end) in elements(&lower, "search-page-media-row", section, section_end) {
            if let Some(result) = search_row(html, &lower, row, row_end, kind) {
                results.push(result);
            }
        }
    }
    results
}

/// Start and end offsets of each `<name ...>...</name>` between `from` and `to`.
fn elements(lower: &str, name: &str, from: usize, to: usize) -> Vec<(usize, usize)> {
    let (open, close) = (format!("<{name}"), format!("</{name}>"));
    let mut found = Vec::new();
    let mut position = from;
    while let Some(offset) = lower[position..to].find(&open) {
        let start = position + offset;
        // `<search-page-result` is also the start of `<search-page-results`.
        if !lower[start + open.len()..].starts_with(|c: char| c.is_whitespace() || c == '>') {
            position = start + open.len();
            continue;
        }
        let end = lower[start..to].find(&close).map_or(to, |offset| start + offset + close.len());
        found.push((start, end));
        position = end;
    }
    found
}

fn search_row(html: &str, lower: &str, start: usize, end: usize, kind: Kind) -> Option<SearchResult> {
    let open_end = start + lower[start..end].find('>')?;
    let (tag, tag_lower) = (&html[start..open_end], &lower[start..open_end]);
    // Movie rows hyphenate their attribute names and show rows don't. Both carry
    // `releaseyear` and `startyear`, with whichever doesn't apply left empty.
    let attr = |names: &[&str]| {
        names.iter().find_map(|name| attribute(tag, tag_lower, name).map(str::trim).filter(|value| !value.is_empty()))
    };

    // The title link, not the thumbnail's.
    let link = lower[start..end].find("data-qa=\"info-name\"").map(|offset| start + offset)?;
    let link_start = lower[..link].rfind("<a")?;
    let link_open_end = link + lower[link..end].find('>')?;
    let href = attribute(&html[link_start..link_open_end], &lower[link_start..link_open_end], "href")?;
    let text_end = link_open_end + lower[link_open_end..end].find("</a>")?;
    let title = tidy(&html[link_open_end + 1..text_end]);
    if title.is_empty() || !is_title_url(href) {
        return None;
    }

    let start_year = attr(&["release-year", "releaseyear", "start-year", "startyear"]);
    let end_year = attr(&["end-year", "endyear"]);
    let years = match (start_year, end_year) {
        (Some(from), Some(to)) if from != to => Some(format!("{from}-{to}")),
        (Some(year), _) => Some(year.to_string()),
        (None, _) => None,
    };
    Some(SearchResult {
        kind,
        title,
        url: href.to_string(),
        year: start_year.and_then(|year| year.parse().ok()),
        years,
        tomatometer: attr(&["tomatometer-score", "tomatometerscore"]).and_then(|score| score.parse().ok()),
    })
}

/// A movie or show page on Rotten Tomatoes, the only kind of address this fetches.
fn is_title_url(href: &str) -> bool {
    let Ok(url) = url::Url::parse(href) else { return false };
    url.scheme() == "https"
        && url.host_str() == Some("www.rottentomatoes.com")
        && url.port().is_none()
        && (url.path().starts_with("/m/") || url.path().starts_with("/tv/"))
}

/// Every result with exactly the query's title. Failing that, the search page's top
/// result with a score, or just its top result. With a year, only titles from that year.
fn matches(results: &[SearchResult], title: &str, year: Option<u16>) -> Matches {
    let wanted = normalize(title);
    let in_year: Vec<usize> =
        (0..results.len()).filter(|&index| year.is_none_or(|year| results[index].year == Some(year))).collect();

    let exact: Vec<usize> = in_year.iter().copied().filter(|&index| normalize(&results[index].title) == wanted).collect();
    if !exact.is_empty() {
        return Matches { candidates: exact, exact: true };
    }
    let closest = in_year.iter().copied().find(|&index| results[index].tomatometer.is_some()).or(in_year.first().copied());
    Matches { candidates: closest.into_iter().collect(), exact: false }
}

/// Lowercase words, so "Dune: Part Two" matches "dune part two" and "&" matches "and".
fn normalize(title: &str) -> String {
    title
        .replace('&', " and ")
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The scores and critics consensus from a title page.
fn scorecard(html: &str) -> Scorecard {
    let lower = html.to_ascii_lowercase();
    let json = script_json(html, &lower, "id=\"media-scorecard-json\"");
    let field = |value: &serde_json::Value, key: &str| value[key].as_str().map(str::trim).filter(|text| !text.is_empty()).map(String::from);

    let critics = json.as_ref().and_then(|json| {
        let score = &json["criticsScore"];
        let percent = field(score, "scorePercent")?;
        let reviews = score["reviewCount"].as_u64().map(|count| format!(" from {count} critic reviews")).unwrap_or_default();
        let verdict = match (score["certified"].as_bool(), score["sentiment"].as_str()) {
            (Some(true), _) => " (Certified Fresh)",
            (_, Some("POSITIVE")) => " (Fresh)",
            (_, Some("NEGATIVE")) => " (Rotten)",
            _ => "",
        };
        Some(format!("{percent}{reviews}{verdict}"))
    });
    let top_critics = json.as_ref().and_then(|json| {
        let score = &json["overlay"]["criticsTop"];
        let percent = field(score, "scorePercent")?;
        let reviews = score["reviewCount"].as_u64().map(|count| format!(" from {count} reviews")).unwrap_or_default();
        Some(format!("{percent}{reviews}"))
    });
    let audience = json.as_ref().and_then(|json| {
        let score = &json["audienceScore"];
        let percent = field(score, "scorePercent")?;
        let ratings = field(score, "bandedRatingCount").map(|count| format!(" from {count}")).unwrap_or_default();
        let verdict = match (score["certified"].as_bool(), score["sentiment"].as_str()) {
            (Some(true), _) => " (Verified Hot)",
            (_, Some("POSITIVE")) => " (Hot)",
            (_, Some("NEGATIVE")) => " (Stale)",
            _ => "",
        };
        Some(format!("{percent}{ratings}{verdict}"))
    });

    Scorecard {
        critic_reviews: json.as_ref().and_then(|json| json["criticsScore"]["reviewCount"].as_u64()),
        critics,
        top_critics,
        audience,
        consensus: consensus(html, &lower),
        description: json.as_ref().and_then(|json| field(json, "description")),
    }
}

/// The JSON in the `<script>` whose opening tag contains `marker`.
fn script_json(html: &str, lower: &str, marker: &str) -> Option<serde_json::Value> {
    let found = lower.find(marker)?;
    let start = found + lower[found..].find('>')? + 1;
    let end = start + lower[start..].find("</script>")?;
    serde_json::from_str(&html[start..end]).ok()
}

fn consensus(html: &str, lower: &str) -> Option<String> {
    let section = lower.find("id=\"critics-consensus\"")?;
    let start = section + lower[section..].find("<p>")? + "<p>".len();
    let end = start + lower[start..].find("</p>")?;
    Some(tidy(&html[start..end])).filter(|text| !text.is_empty())
}

/// Text with its tags removed and entities decoded.
fn tidy(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    collapse_whitespace(&decode_entities(&text))
}

fn format_context(query: &str, results: &[SearchResult], chosen: usize, exact: bool, scorecard: Option<&Scorecard>) -> String {
    let result = &results[chosen];
    let not_available = "(not available yet)";
    let years = result.years.as_deref().map(|years| format!("{years}, ")).unwrap_or_default();
    let mut context = format!(
        "Rotten Tomatoes result for \"{query}\":\nTitle: {} ({years}{})\nURL: {}\n",
        result.title,
        result.kind.label(),
        result.url
    );
    if !exact {
        context.push_str("Match: nothing has exactly this title, so this is the closest result\n");
    }

    match scorecard {
        Some(card) => {
            context.push_str(&format!("Tomatometer: {}\n", card.critics.as_deref().unwrap_or(not_available)));
            if let Some(top) = &card.top_critics {
                context.push_str(&format!("Top critics: {top}\n"));
            }
            context.push_str(&format!("Popcornmeter: {}\n", card.audience.as_deref().unwrap_or(not_available)));
            if let Some(consensus) = &card.consensus {
                context.push_str(&format!("Critics consensus: {consensus}\n"));
            }
            if let Some(description) = &card.description {
                context.push_str(&format!("Synopsis: {description}\n"));
            }
        }
        None => {
            // The search page's score is all there is when the title page didn't load.
            let score = result.tomatometer.map(|score| format!("{score}%"));
            context.push_str(&format!("Tomatometer: {}\n", score.as_deref().unwrap_or(not_available)));
            context.push_str("Popcornmeter: (the title page couldn't be read)\n");
        }
    }

    let others: Vec<String> = results
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != chosen)
        .take(MAX_OTHER_MATCHES)
        .map(|(_, other)| {
            let years = other.years.as_deref().map(|years| format!("{years} ")).unwrap_or_default();
            let score = other.tomatometer.map(|score| format!(", {score}%")).unwrap_or_default();
            format!("{} ({years}{}{score})", other.title, other.kind.label())
        })
        .collect();
    if !others.is_empty() {
        context.push_str(&format!("Other matches: {}\n", others.join("; ")));
    }

    context.push('\n');
    context
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from the real search page for "the bear": movie rows hyphenate their
    /// attributes, show rows don't, and each row has a thumbnail link before the title.
    const SEARCH: &str = r#"<search-page-results><search-page-result
        data-qa="search-result" skeleton="panel" type="movie">
        <ul slot="list">
        <search-page-media-row skeleton="panel" cast="Josh Brolin" data-qa="data-row" end-year=""
            release-year="2019" start-year="" tomatometer-is-certified="false" tomatometer-score="100">
            <a href="https://www.rottentomatoes.com/m/mickey_and_the_bear" class="unset" data-qa="thumbnail-link" slot="thumbnail"><img alt="Mickey and the Bear"></a>
            <a href="https://www.rottentomatoes.com/m/mickey_and_the_bear" class="unset" data-qa="info-name" slot="title">
                Mickey and the Bear
            </a>
        </search-page-media-row>
        <search-page-media-row skeleton="panel" release-year="1988" tomatometer-score="88">
            <a href="https://www.rottentomatoes.com/m/the_bear_1989" class="unset" data-qa="info-name" slot="title">The Bear</a>
        </search-page-media-row>
        <search-page-media-row skeleton="panel" release-year="2011" tomatometer-score="">
            <a href="https://www.rottentomatoes.com/m/wojtek" class="unset" data-qa="info-name" slot="title">Wojtek: The Bear That Went to War</a>
        </search-page-media-row>
        </ul></search-page-result>
        <search-page-result data-qa="search-result" skeleton="panel" type="tvSeries">
        <search-page-media-row skeleton="panel" endyear="2026" releaseyear="" startyear="2022"
            tomatometeriscertified="false" tomatometerscore="93" tomatometersentiment="POSITIVE">
            <a href="https://www.rottentomatoes.com/tv/the_bear" class="unset" data-qa="info-name" slot="title">The Bear</a>
        </search-page-media-row>
        <search-page-media-row skeleton="panel" startyear="1979" tomatometerscore="">
            <a href="https://evil.example/tv/x" class="unset" data-qa="info-name" slot="title">Elsewhere</a>
        </search-page-media-row>
        </search-page-result>
        <search-page-result data-qa="search-result" type="celebrity">
        <search-page-media-row><a href="https://www.rottentomatoes.com/celebrity/a" data-qa="info-name">A Bear</a></search-page-media-row>
        </search-page-result></search-page-results>"#;

    /// Trimmed from the real Dune: Part Two page.
    const TITLE_PAGE: &str = r#"<script id="media-hero-json" data-json="mediaHero" type="application/json">{"content":{}}</script>
        <div id="critics-consensus" class="consensus">
          <rt-text context="heading"><score-icon-critics certified="true"></score-icon-critics>Critics Consensus</rt-text>
          <p>Visually thrilling and narratively epic, <em>Dune: Part Two</em> continues Denis Villeneuve&#39;s adaptation.</p>
          <a href="/m/dune_part_two/reviews">Read Critics Reviews</a>
        </div>
        <script
            id="media-scorecard-json"
            data-json="mediaScorecard"
            type="application/json"
        >{"audienceScore":{"averageRating":"4.7","bandedRatingCount":"5,000+ Verified Ratings","certified":true,"scorePercent":"95%","sentiment":"POSITIVE","title":"Popcornmeter"},
          "criticsScore":{"averageRating":"8.40","certified":true,"likedCount":429,"notLikedCount":37,"reviewCount":466,"scorePercent":"92%","sentiment":"POSITIVE","title":"Tomatometer"},
          "description":"Paul Atreides unites with Chani and the Fremen.",
          "overlay":{"criticsTop":{"certified":true,"reviewCount":78,"scorePercent":"87%"}}}</script>"#;

    #[test]
    fn reads_movies_and_shows_from_the_search_page() {
        let results = search_results(SEARCH);
        let titles: Vec<&str> = results.iter().map(|result| result.title.as_str()).collect();
        assert_eq!(titles, ["Mickey and the Bear", "The Bear", "Wojtek: The Bear That Went to War", "The Bear"]);

        assert_eq!(
            results[3],
            SearchResult {
                kind: Kind::Tv,
                title: "The Bear".to_string(),
                url: "https://www.rottentomatoes.com/tv/the_bear".to_string(),
                year: Some(2022),
                years: Some("2022-2026".to_string()),
                tomatometer: Some(93),
            }
        );
        assert_eq!(results[1].years.as_deref(), Some("1988"));
        assert_eq!(results[2].tomatometer, None);
    }

    #[test]
    fn only_rotten_tomatoes_title_pages_are_fetched() {
        assert!(is_title_url("https://www.rottentomatoes.com/m/dune_part_two"));
        assert!(is_title_url("https://www.rottentomatoes.com/tv/the_bear"));
        assert!(!is_title_url("https://evil.example/m/x"));
        assert!(!is_title_url("http://www.rottentomatoes.com/m/x"));
        assert!(!is_title_url("https://www.rottentomatoes.com:8443/m/x"));
        assert!(!is_title_url("https://www.rottentomatoes.com/celebrity/a"));
        assert!(!is_title_url("/m/relative"));
    }

    #[test]
    fn matches_every_exact_title_or_else_the_closest() {
        let results = search_results(SEARCH);
        let exact = |candidates: Vec<usize>| Matches { candidates, exact: true };
        // The movie and the show, to be decided between by their review counts.
        assert_eq!(matches(&results, "the bear", None), exact(vec![1, 3]));
        assert_eq!(matches(&results, "The Bear", Some(1988)), exact(vec![1]));
        assert_eq!(matches(&results, "mickey & the bear", None), exact(vec![0]));
        assert_eq!(matches(&results, "bears", None), Matches { candidates: vec![0], exact: false });
        assert_eq!(matches(&results, "wojtek", Some(2011)), Matches { candidates: vec![2], exact: false });
        assert_eq!(matches(&results, "the bear", Some(1950)), Matches { candidates: vec![], exact: false });
        assert_eq!(matches(&[], "anything", None), Matches { candidates: vec![], exact: false });
    }

    #[test]
    fn splits_a_trailing_year() {
        assert_eq!(split_year("dune 1984"), Some(("dune", 1984)));
        assert_eq!(split_year("the bear (2022)"), Some(("the bear", 2022)));
        assert_eq!(split_year("dune, 2021"), Some(("dune", 2021)));
        assert_eq!(split_year("1917"), None);
        assert_eq!(split_year("ocean's 11"), None);
        assert_eq!(split_year("dune part two"), None);
    }

    #[test]
    fn reads_the_scorecard_and_consensus() {
        let card = scorecard(TITLE_PAGE);
        assert_eq!(card.critics.as_deref(), Some("92% from 466 critic reviews (Certified Fresh)"));
        assert_eq!(card.top_critics.as_deref(), Some("87% from 78 reviews"));
        assert_eq!(card.audience.as_deref(), Some("95% from 5,000+ Verified Ratings (Verified Hot)"));
        assert_eq!(
            card.consensus.as_deref(),
            Some("Visually thrilling and narratively epic, Dune: Part Two continues Denis Villeneuve's adaptation.")
        );
        assert_eq!(card.description.as_deref(), Some("Paul Atreides unites with Chani and the Fremen."));
    }

    #[test]
    fn a_page_without_scores_says_so() {
        let card = scorecard(r#"<script id="media-scorecard-json" type="application/json">{"criticsScore":{"scorePercent":""},"audienceScore":{}}</script>"#);
        assert_eq!(card, Scorecard::default());

        let results = search_results(SEARCH);
        let context = format_context("wojtek", &results, 2, false, Some(&card));
        assert!(context.contains("Tomatometer: (not available yet)"), "{context}");
        assert!(context.contains("Popcornmeter: (not available yet)"), "{context}");
        assert!(context.contains("so this is the closest result"), "{context}");
    }

    #[test]
    fn falls_back_to_the_search_score_and_lists_other_matches() {
        let results = search_results(SEARCH);
        let context = format_context("the bear", &results, 3, true, None);
        assert!(context.contains("Title: The Bear (2022-2026, TV series)"), "{context}");
        assert!(context.contains("Tomatometer: 93%\n"), "{context}");
        assert!(context.contains("Other matches: Mickey and the Bear (2019 movie, 100%); The Bear (1988 movie, 88%); Wojtek"), "{context}");
    }

    /// Reads the live site, so it's skipped unless asked for:
    /// `cargo test -- --ignored looks_up_a_live_title`
    #[tokio::test]
    #[ignore]
    async fn looks_up_a_live_title() {
        let context = lookup(&reqwest::Client::new(), "dune part two").await.unwrap();
        assert!(context.contains("https://www.rottentomatoes.com/m/dune_part_two"), "{context}");
        assert!(context.contains("Tomatometer: 9"), "{context}");
        assert!(context.contains("Critics consensus:"), "{context}");
    }
}
