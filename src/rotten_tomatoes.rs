//! The `rotten_tomatoes` skill tool: a movie's or show's Tomatometer and Popcornmeter.
//! Rotten Tomatoes has no public API, so this reads its search page, then the chosen
//! title's page, whose scores and credits are embedded as JSON. Both are its own
//! server-rendered pages, so no browser is needed. Only Rotten Tomatoes pages and its
//! image server are ever fetched: the title page's and poster's addresses come from
//! the pages, and are checked before they're used.

use std::io::Cursor;

use anyhow::{Context, Result, bail};
use image::codecs::jpeg::JpegEncoder;
use tracing::warn;

use crate::links::{attribute, collapse_whitespace, decode_entities};
use crate::message_log::ReplyImage;

const SITE: &str = "https://www.rottentomatoes.com";
/// Other matches listed after the chosen one, so the reply can point out a remake or a
/// show of the same name.
const MAX_OTHER_MATCHES: usize = 4;
/// Title pages read to choose between titles with the same name.
const MAX_COMPARED: usize = 3;
/// People listed from the cast and crew, and producers from the credits.
const MAX_CAST: usize = 8;
const MAX_PRODUCERS: usize = 4;
/// Posters are served as large as 2000x3000, far more than a chat needs.
const POSTER_MAX_WIDTH: u32 = 400;
const POSTER_MAX_HEIGHT: u32 = 600;
const POSTER_MAX_BYTES: usize = 8 * 1024 * 1024;
const POSTER_QUALITY: u8 = 85;
/// People on the cast sheet, which is two rows of three.
const MAX_PEOPLE: usize = 6;
/// Headshots are served at 100x120, a few kilobytes each.
const PHOTO_MAX_BYTES: usize = 1024 * 1024;

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
struct TitlePage {
    critic_reviews: Option<u64>,
    critics: Option<String>,
    top_critics: Option<String>,
    audience: Option<String>,
    consensus: Option<String>,
    description: Option<String>,
    /// Like `PG-13`, `In Theaters Nov 13`, and `2h 46m`, or a show's seasons.
    facts: Vec<String>,
    genres: Vec<String>,
    directors: Vec<String>,
    producers: Vec<String>,
    /// `Name as Role`, from the page's cast and crew section.
    cast: Vec<String>,
    /// The cast and crew section's people, with their photos.
    people: Vec<CastMember>,
    /// The trailer's name, length, and Rotten Tomatoes page.
    trailer: Option<String>,
    poster: Option<String>,
}

/// What a lookup found: the context for the skill's prompt, and the poster's address
/// if there's one to post.
pub struct Lookup {
    pub context: String,
    pub poster: Option<String>,
    pub people: Vec<CastMember>,
}

/// Someone from a title page's cast and crew section.
#[derive(Debug, Clone, PartialEq)]
pub struct CastMember {
    pub name: String,
    /// The character they play, or their job.
    pub role: Option<String>,
    /// Their headshot's address, already checked to be on the image server.
    pub photo: Option<String>,
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
pub async fn lookup(http: &reqwest::Client, query: &str) -> Result<Lookup> {
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
        return Ok(Lookup {
            context: format!("No Rotten Tomatoes match was found for \"{query}\".\n\n"),
            poster: None,
            people: Vec::new(),
        });
    }

    // Several titles share the name, like a show and its remakes. The one with the most
    // critic reviews is almost always the one people mean.
    let compared: Vec<usize> = matched.candidates.iter().copied().take(MAX_COMPARED).collect();
    let pages = futures_util::future::join_all(compared.iter().map(|&index| title_page(http, &results[index].url))).await;
    let (chosen, scorecard) = compared
        .into_iter()
        .zip(pages)
        .map(|(index, page)| match page {
            Ok(html) => (index, Some(parse_title_page(&html))),
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

    let poster = scorecard.as_ref().and_then(|page| page.poster.clone());
    let people = scorecard.as_ref().map(|page| page.people.clone()).unwrap_or_default();
    Ok(Lookup { context: format_context(query, &results, chosen, matched.exact, scorecard.as_ref()), poster, people })
}

/// The images posted with a lookup's reply: its poster, then a sheet of its cast and
/// crew photos. Either is left out if it can't be made.
pub async fn pictures(http: &reqwest::Client, found: &Lookup) -> Vec<ReplyImage> {
    let mut pictures = Vec::new();
    if let Some(url) = &found.poster {
        match poster(http, url).await {
            Ok(poster) => pictures.push(poster),
            Err(err) => warn!(?err, "failed to fetch a poster"),
        }
    }
    if found.people.iter().any(|person| person.photo.is_some()) {
        match cast_sheet(http, &found.people).await {
            Ok(sheet) => pictures.push(sheet),
            Err(err) => warn!(?err, "failed to make a cast sheet"),
        }
    }
    pictures
}

async fn cast_sheet(http: &reqwest::Client, people: &[CastMember]) -> Result<ReplyImage> {
    let photos = futures_util::future::join_all(people.iter().map(|person| async move {
        let url = person.photo.as_deref()?;
        fetch_image(http, url, PHOTO_MAX_BYTES).await.inspect_err(|err| warn!(?err, %url, "failed to fetch a cast photo")).ok()
    }))
    .await;
    let people: Vec<crate::cast_sheet::Person> = people
        .iter()
        .zip(photos)
        .map(|(person, photo)| crate::cast_sheet::Person { name: person.name.clone(), role: person.role.clone(), photo })
        .collect();
    // Drawing and encoding are CPU work, so they're kept off the async threads.
    tokio::task::spawn_blocking(move || crate::cast_sheet::render(&people)).await.context("cast sheet task failed")?
}

/// An image from Rotten Tomatoes' image server, refused if it's larger than `max_bytes`.
async fn fetch_image(http: &reqwest::Client, url: &str, max_bytes: usize) -> Result<Vec<u8>> {
    if !is_poster_url(url) {
        bail!("not a Rotten Tomatoes image address");
    }
    let response = http.get(url).send().await.context("image request failed")?.error_for_status().context("image request returned an error status")?;
    if response.content_length().is_some_and(|length| length > max_bytes as u64) {
        bail!("image is too large");
    }
    let data = response.bytes().await.context("failed to read the image")?;
    if data.len() > max_bytes {
        bail!("image is too large");
    }
    Ok(data.to_vec())
}

/// Downloads a poster and shrinks it to chat size.
async fn poster(http: &reqwest::Client, url: &str) -> Result<ReplyImage> {
    let data = fetch_image(http, url, POSTER_MAX_BYTES).await?;
    // Decoding and encoding are CPU work, so they're kept off the async threads.
    tokio::task::spawn_blocking(move || shrink_poster(&data)).await.context("poster task failed")?
}

fn shrink_poster(data: &[u8]) -> Result<ReplyImage> {
    let image = image::load_from_memory(data).context("the poster isn't an image")?;
    let image = image.thumbnail(POSTER_MAX_WIDTH, POSTER_MAX_HEIGHT).to_rgb8();
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(Cursor::new(&mut jpeg), POSTER_QUALITY).encode_image(&image).context("failed to encode the poster")?;
    Ok(ReplyImage {
        filename: "poster.jpg".to_string(),
        mime: mime::IMAGE_JPEG,
        data: jpeg,
        width: image.width(),
        height: image.height(),
        animated: false,
    })
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

/// A movie or show page on Rotten Tomatoes, the only kind of page this fetches.
fn is_title_url(href: &str) -> bool {
    is_https_on(href, "www.rottentomatoes.com").is_some_and(|url| url.path().starts_with("/m/") || url.path().starts_with("/tv/"))
}

/// An image on Rotten Tomatoes' image server, the only images this fetches.
fn is_poster_url(href: &str) -> bool {
    is_https_on(href, "resizing.flixster.com").is_some()
}

fn is_https_on(href: &str, host: &str) -> Option<url::Url> {
    let url = url::Url::parse(href).ok()?;
    (url.scheme() == "https" && url.host_str() == Some(host) && url.port().is_none()).then_some(url)
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

/// The scores, consensus, and credits from a title page.
fn parse_title_page(html: &str) -> TitlePage {
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

    // The page's schema.org description, which carries the credits, poster, and trailer.
    let schema = script_json(html, &lower, "type=\"application/ld+json\"").unwrap_or_default();
    let hero = script_json(html, &lower, "id=\"media-hero-json\"").unwrap_or_default();
    let strings = |value: &serde_json::Value| -> Vec<String> {
        value.as_array().into_iter().flatten().filter_map(|item| item.as_str()).map(str::trim).filter(|text| !text.is_empty()).map(String::from).collect()
    };
    let names = |value: &serde_json::Value| -> Vec<String> {
        let people = match value {
            serde_json::Value::Array(people) => people.as_slice(),
            person => std::slice::from_ref(person),
        };
        people.iter().filter_map(|person| field(person, "name")).collect()
    };

    let directors = names(&schema["director"]);
    let producers: Vec<String> = names(&schema["producer"]).into_iter().take(MAX_PRODUCERS).collect();
    let people: Vec<CastMember> = cast_and_crew(html, &lower).into_iter().take(MAX_PEOPLE).collect();
    let mut cast: Vec<String> = people
        .iter()
        // Directors are listed on their own line.
        .filter(|person| !directors.contains(&person.name))
        .map(|person| match &person.role {
            Some(role) => format!("{} as {role}", person.name),
            None => person.name.clone(),
        })
        .take(MAX_CAST)
        .collect();
    if cast.is_empty() {
        cast = names(&schema["actor"]).into_iter().take(MAX_CAST).collect();
    }
    let trailer = field(&schema["video"], "contentUrl").filter(|url| is_https_on(url, "www.rottentomatoes.com").is_some()).map(|url| {
        let name = field(&schema["video"], "name").unwrap_or_else(|| "Trailer".to_string());
        match field(&schema["video"], "duration") {
            Some(length) => format!("{name} ({length}): {url}"),
            None => format!("{name}: {url}"),
        }
    });
    let mut genres = strings(&hero["content"]["metadataGenres"]);
    if genres.is_empty() {
        genres = strings(&schema["genre"]);
    }

    TitlePage {
        critic_reviews: json.as_ref().and_then(|json| json["criticsScore"]["reviewCount"].as_u64()),
        critics,
        top_critics,
        audience,
        consensus: consensus(html, &lower),
        description: json.as_ref().and_then(|json| field(json, "description")),
        facts: strings(&hero["content"]["metadataProps"]),
        genres,
        directors,
        producers,
        cast,
        people,
        trailer,
        poster: field(&schema, "image").filter(|url| is_poster_url(url)),
    }
}

/// Each person in the cast and crew section, with their character or job and their
/// photo. Someone listed twice, like a show's creator who also directs, is listed once
/// with both jobs.
fn cast_and_crew(html: &str, lower: &str) -> Vec<CastMember> {
    const ITEM: &str = "data-qa=\"person-item\"";
    let Some(section) = lower.find("data-qa=\"section:cast-and-crew\"") else { return Vec::new() };
    let end = lower[section..].find("</section>").map_or(lower.len(), |offset| section + offset);
    let text_of = |marker: &str, from: usize, to: usize| -> Option<String> {
        let found = from + lower[from..to].find(marker)?;
        let start = found + lower[found..to].find('>')? + 1;
        let close = start + lower[start..to].find("</p>")?;
        Some(tidy(&html[start..close])).filter(|text| !text.is_empty())
    };

    let mut items: Vec<usize> = lower[section..end].match_indices(ITEM).map(|(offset, _)| section + offset).collect();
    items.push(end);
    let mut people: Vec<CastMember> = Vec::new();
    for bounds in items.windows(2) {
        let (from, to) = (bounds[0], bounds[1]);
        let Some(name) = text_of("data-qa=\"person-name\"", from, to) else { continue };
        let role = text_of("data-qa=\"person-role\"", from, to);
        let photo = lower[from..to].find("<rt-img").and_then(|offset| {
            let tag_start = from + offset;
            let tag_end = tag_start + lower[tag_start..to].find('>')?;
            attribute(&html[tag_start..tag_end], &lower[tag_start..tag_end], "src").map(decode_entities).filter(|url| is_poster_url(url))
        });

        match people.iter_mut().find(|person| person.name == name) {
            Some(listed) => {
                if let Some(role) = role {
                    listed.role = Some(match listed.role.take() {
                        Some(roles) => format!("{roles}, {role}"),
                        None => role,
                    });
                }
                listed.photo = listed.photo.take().or(photo);
            }
            None => people.push(CastMember { name, role, photo }),
        }
    }
    people
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

fn format_context(query: &str, results: &[SearchResult], chosen: usize, exact: bool, scorecard: Option<&TitlePage>) -> String {
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
            let lists = [
                ("Details", &card.facts, " · "),
                ("Genres", &card.genres, ", "),
                ("Directed by", &card.directors, ", "),
                ("Produced by", &card.producers, ", "),
                ("Cast and crew", &card.cast, "; "),
            ];
            for (label, items, separator) in lists {
                if !items.is_empty() {
                    context.push_str(&format!("{label}: {}\n", items.join(separator)));
                }
            }
            if let Some(trailer) = &card.trailer {
                context.push_str(&format!("Trailer: {trailer}\n"));
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
    const TITLE_PAGE: &str = r#"<script type="application/ld+json">{"@context":"http://schema.org","@type":"Movie",
          "actor":[{"@type":"Person","name":"Timothée Chalamet"},{"@type":"Person","name":"Zendaya"}],
          "contentRating":"PG-13","director":[{"@type":"Person","name":"Denis Villeneuve"}],
          "genre":["Sci-Fi","Adventure"],
          "image":"https://resizing.flixster.com/PgHhmCKS3hR6GUsVNlC-vZ9d90I=/ems.cHJkLWVtcy1hc3NldHMvbW92aWVz",
          "producer":[{"name":"Joshua Grode"},{"name":"Jon Spaihts"},{"name":"Thomas Tull"},{"name":"Herb Gains"},{"name":"Brian Herbert"}],
          "video":{"@type":"VideoObject","name":"Dune: Part Two: Trailer 3","duration":"2:40","contentUrl":"https://www.rottentomatoes.com/m/dune_part_two/videos/3uVjDzzml2kM"}}</script>
        <script id="media-hero-json" data-json="mediaHero" type="application/json">{"content":{"metadataGenres":["Sci-Fi","Adventure","Action"],"metadataProps":["PG-13","In Theaters Nov 13","2h 46m"]}}</script>
        <section aria-labelledby="cast-and-crew-label" class="cast-and-crew" data-qa="section:cast-and-crew">
          <a href="/celebrity/denis-villeneuve" data-qa="person-item"><tile-dynamic skeleton="panel">
            <div slot="inset-text" aria-label="Denis Villeneuve, Director">
            <p class="name" data-qa="person-name">Denis Villeneuve</p> <p class="role" data-qa="person-role">Director</p></div></tile-dynamic></a>
          <a href="/celebrity/timothee_chalamet" data-qa="person-item"><tile-dynamic skeleton="panel">
            <rt-img alt="Timothée Chalamet thumbnail image" loading="lazy" slot="image"
              src="https://resizing.flixster.com/4IBwYVIQ=/100x120/v2/https://resizing.flixster.com/x/v3/t/assets/535891_v9_bc.jpg"></rt-img>
            <p class="name" data-qa="person-name">Timothée Chalamet</p> <p class="role" data-qa="person-role">Paul Atreides</p></tile-dynamic></a>
          <a href="/celebrity/someone" data-qa="person-item"><rt-img src="https://evil.example/face.jpg"></rt-img>
            <p class="name" data-qa="person-name">No Role</p></a>
          <a href="/celebrity/denis-villeneuve" data-qa="person-item"><p class="name" data-qa="person-name">Denis Villeneuve</p>
            <p class="role" data-qa="person-role">Screenwriter</p></a>
          <a href="/celebrity/zendaya" data-qa="person-item"><p class="name" data-qa="person-name">Zendaya</p>
            <p class="role" data-qa="person-role">Chani</p></a>
        </section>
        <p data-qa="person-name">Outside The Section</p>
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
        assert!(is_poster_url("https://resizing.flixster.com/abc=/ems.xyz"));
        assert!(!is_poster_url("https://resizing.flixster.com.evil.example/abc"));
        assert!(!is_poster_url("http://resizing.flixster.com/abc"));
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
        let card = parse_title_page(TITLE_PAGE);
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
    fn reads_the_credits_trailer_and_poster() {
        let card = parse_title_page(TITLE_PAGE);
        assert_eq!(card.facts, ["PG-13", "In Theaters Nov 13", "2h 46m"]);
        assert_eq!(card.genres, ["Sci-Fi", "Adventure", "Action"]);
        assert_eq!(card.directors, ["Denis Villeneuve"]);
        assert_eq!(card.producers, ["Joshua Grode", "Jon Spaihts", "Thomas Tull", "Herb Gains"]);
        // The director has their own line, and a role belongs only to the person before it.
        assert_eq!(card.cast, ["Timothée Chalamet as Paul Atreides", "No Role", "Zendaya as Chani"]);
        assert_eq!(
            card.trailer.as_deref(),
            Some("Dune: Part Two: Trailer 3 (2:40): https://www.rottentomatoes.com/m/dune_part_two/videos/3uVjDzzml2kM")
        );
        assert!(card.poster.as_deref().is_some_and(|url| url.starts_with("https://resizing.flixster.com/")));

        // Everyone, crew included, for the photo sheet: someone listed twice is listed
        // once with both jobs, and only the image server's photos are kept.
        let people: Vec<(&str, Option<&str>, bool)> =
            card.people.iter().map(|person| (person.name.as_str(), person.role.as_deref(), person.photo.is_some())).collect();
        assert_eq!(
            people,
            [
                ("Denis Villeneuve", Some("Director, Screenwriter"), false),
                ("Timothée Chalamet", Some("Paul Atreides"), true),
                ("No Role", None, false),
                ("Zendaya", Some("Chani"), false),
            ]
        );

        let context = format_context("dune part two", &search_results(SEARCH), 0, true, Some(&card));
        assert!(context.contains("Details: PG-13 · In Theaters Nov 13 · 2h 46m\n"), "{context}");
        assert!(context.contains("Directed by: Denis Villeneuve\n"), "{context}");
        assert!(context.contains("Cast and crew: Timothée Chalamet as Paul Atreides; No Role; Zendaya as Chani\n"), "{context}");
        assert!(context.contains("Trailer: Dune: Part Two: Trailer 3 (2:40): https://"), "{context}");
    }

    #[test]
    fn falls_back_to_the_schema_for_cast_and_genres() {
        let page = r#"<script type="application/ld+json">{"actor":{"name":"Solo Actor"},"genre":["Drama"],
            "image":"https://evil.example/poster.jpg","video":{"contentUrl":"https://evil.example/trailer"}}</script>"#;
        let card = parse_title_page(page);
        assert_eq!(card.cast, ["Solo Actor"]);
        assert_eq!(card.genres, ["Drama"]);
        assert_eq!(card.poster, None);
        assert_eq!(card.trailer, None);
    }

    #[test]
    fn shrinks_a_poster_to_chat_size() {
        let mut jpeg = Vec::new();
        let big = image::RgbImage::from_pixel(1296, 1920, image::Rgb([200, 30, 30]));
        JpegEncoder::new(Cursor::new(&mut jpeg)).encode_image(&big).unwrap();

        let poster = shrink_poster(&jpeg).unwrap();
        assert!(poster.width <= POSTER_MAX_WIDTH && poster.height <= POSTER_MAX_HEIGHT);
        assert_eq!((poster.width, poster.height), (400, 593));
        assert_eq!(poster.mime, mime::IMAGE_JPEG);
        assert!(image::load_from_memory(&poster.data).is_ok());
        assert!(shrink_poster(b"not an image").is_err());
    }

    #[test]
    fn a_page_without_scores_says_so() {
        let card = parse_title_page(r#"<script id="media-scorecard-json" type="application/json">{"criticsScore":{"scorePercent":""},"audienceScore":{}}</script>"#);
        assert_eq!(card, TitlePage::default());

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
        let http = reqwest::Client::new();
        let found = lookup(&http, "dune part two").await.unwrap();
        let context = found.context.clone();
        assert!(context.contains("https://www.rottentomatoes.com/m/dune_part_two"), "{context}");
        assert!(context.contains("Tomatometer: 9"), "{context}");
        assert!(context.contains("Critics consensus:"), "{context}");
        assert!(context.contains("Cast and crew: Timothée Chalamet as Paul Atreides"), "{context}");
        assert!(context.contains("Trailer: "), "{context}");
        let pictures = pictures(&http, &found).await;
        let names: Vec<&str> = pictures.iter().map(|picture| picture.filename.as_str()).collect();
        assert_eq!(names, ["poster.jpg", "cast.jpg"]);
        assert!(pictures[0].width <= POSTER_MAX_WIDTH && pictures[0].height <= POSTER_MAX_HEIGHT && pictures[0].data.len() > 1000);
    }
}
