//! Fetches pages linked in chat, so the bot knows what was shared without being
//! asked and without a Claude call. Anyone in a room can post any URL, and the bot
//! may be running on a private network, so `LinkFetcher` only reaches public
//! addresses, and that's enforced where connections are made: in the DNS resolver
//! and on every redirect, so a hostname or redirect can't lead it somewhere private.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use tracing::debug;

const MAX_LINKS_PER_MESSAGE: usize = 3;
const MAX_REDIRECTS: usize = 3;
const MAX_BODY_BYTES: usize = 2_000_000;
const MAX_TITLE_CHARS: usize = 200;
const MAX_DESCRIPTION_CHARS: usize = 300;
const MAX_TEXT_CHARS: usize = 4000;
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
const USER_AGENT: &str = concat!("matrix-llm-bot/", env!("CARGO_PKG_VERSION"), " (link preview)");

/// Hosts whose links are Matrix plumbing rather than something people shared.
const SKIPPED_HOSTS: &[&str] = &["matrix.to"];

/// What was found at a posted link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkPreview {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The start of the page's readable text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The http(s) URLs in a message, in order and without repeats.
pub fn extract_urls(text: &str) -> Vec<Url> {
    let mut urls: Vec<Url> = Vec::new();
    for word in text.split_whitespace() {
        let start = word.find("https://").or_else(|| word.find("http://"));
        let Some(start) = start else {
            continue;
        };
        let candidate = word[start..].trim_end_matches(|c: char| ".,;:!?)]}>'\"*_`".contains(c));
        let Ok(url) = Url::parse(candidate) else {
            continue;
        };
        let skipped = url.host_str().is_some_and(|host| SKIPPED_HOSTS.contains(&host));
        if !skipped && !urls.contains(&url) {
            urls.push(url);
        }
    }
    urls
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19))) // benchmarking
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let first = ip.segments()[0];
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (first & 0xFE00) == 0xFC00 // unique local
        || (first & 0xFFC0) == 0xFE80 // link local
        || (first == 0x2001 && ip.segments()[1] == 0xDB8) // documentation
        || (first == 0x64 && ip.segments()[1] == 0xFF9B)) // NAT64, which can embed a private IPv4
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

/// Whether a URL is one the bot may try. Hostnames are checked again by the
/// resolver; an IP written directly into the URL never reaches it, so it's checked
/// here. Limiting ports keeps the bot off anything that isn't a website.
fn is_allowed_url(url: &Url, allow_private: bool) -> bool {
    if allow_private {
        return true;
    }
    let scheme_ok = matches!(url.scheme(), "http" | "https");
    let port_ok = matches!(url.port_or_known_default(), Some(80 | 443));
    let host_ok = match url.host() {
        Some(url::Host::Domain(_)) => true,
        Some(url::Host::Ipv4(ip)) => is_public_ipv4(ip),
        Some(url::Host::Ipv6(ip)) => is_public_ipv6(ip),
        None => false,
    };
    scheme_ok && port_ok && host_ok
}

/// Resolves hostnames, dropping every address that isn't public. This is the check
/// that matters most: it runs on the addresses actually connected to.
struct PublicOnlyResolver {
    allow_private: bool,
}

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private = self.allow_private;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed: Vec<SocketAddr> = found.into_iter().filter(|addr| allow_private || is_public(addr.ip())).collect();
            if allowed.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            let addrs: Addrs = Box::new(allowed.into_iter());
            Ok(addrs)
        })
    }
}

fn redirect_policy(allow_private: bool) -> Policy {
    Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if is_allowed_url(attempt.url(), allow_private) {
            attempt.follow()
        } else {
            attempt.error("redirect to a disallowed address")
        }
    })
}

/// Fetches previews of linked pages. Failures just mean no preview: the link is
/// still in the message text.
#[derive(Clone)]
pub struct LinkFetcher {
    client: reqwest::Client,
    enabled: bool,
    allow_private: bool,
}

impl LinkFetcher {
    pub fn new(enabled: bool) -> Result<Self> {
        Self::build(enabled, false)
    }

    fn build(enabled: bool, allow_private: bool) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent(USER_AGENT)
            .redirect(redirect_policy(allow_private))
            .dns_resolver(Arc::new(PublicOnlyResolver { allow_private }))
            // A proxy would make the connections the resolver can't see.
            .no_proxy()
            .build()
            .context("failed to build link preview client")?;

        Ok(Self { client, enabled, allow_private })
    }

    /// Previews of the links in `text`, fetched at once, in the order they appear.
    pub async fn fetch_all(&self, text: &str) -> Vec<LinkPreview> {
        if !self.enabled {
            return Vec::new();
        }

        let tasks: Vec<_> = extract_urls(text)
            .into_iter()
            .take(MAX_LINKS_PER_MESSAGE)
            .map(|url| {
                let fetcher = self.clone();
                tokio::spawn(async move { fetcher.fetch(url).await })
            })
            .collect();

        let mut previews = Vec::new();
        for task in tasks {
            if let Ok(Some(preview)) = task.await {
                previews.push(preview);
            }
        }
        previews
    }

    async fn fetch(&self, url: Url) -> Option<LinkPreview> {
        if !is_allowed_url(&url, self.allow_private) {
            debug!(%url, "not fetching disallowed link");
            return None;
        }

        match self.try_fetch(&url).await {
            Ok(preview) => preview,
            Err(err) => {
                debug!(?err, %url, "failed to fetch link");
                None
            }
        }
    }

    async fn try_fetch(&self, url: &Url) -> Result<Option<LinkPreview>> {
        let mut response = self.client.get(url.clone()).send().await?.error_for_status()?;

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();

        if content_type.starts_with("image/") || content_type.starts_with("video/") || content_type.starts_with("audio/") {
            let kind = content_type.split(';').next().unwrap_or_default().to_string();
            let description = Some(format!("A {kind} file."));
            return Ok(Some(LinkPreview { url: url.to_string(), title: None, description, text: None }));
        }

        let is_html = content_type.contains("html");
        if !is_html && !content_type.starts_with("text/plain") {
            return Ok(None);
        }

        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            body.extend_from_slice(&chunk);
            if body.len() >= MAX_BODY_BYTES {
                body.truncate(MAX_BODY_BYTES);
                break;
            }
        }
        let body = String::from_utf8_lossy(&body);

        let preview = if is_html {
            parse_html(url.as_str(), &body)
        } else {
            LinkPreview {
                url: url.to_string(),
                title: None,
                description: None,
                text: Some(clip(&collapse_whitespace(&body), MAX_TEXT_CHARS)).filter(|text| !text.is_empty()),
            }
        };
        Ok(Some(preview).filter(|preview| preview.title.is_some() || preview.description.is_some() || preview.text.is_some()))
    }
}

fn parse_html(url: &str, html: &str) -> LinkPreview {
    // Same byte offsets as `html`, so positions found here can slice the original.
    let lower = html.to_ascii_lowercase();

    let meta = |keys: &[&str]| keys.iter().find_map(|key| meta_content(html, &lower, key));
    let tidy = |text: String, limit: usize| Some(clip(&collapse_whitespace(&decode_entities(&text)), limit)).filter(|text| !text.is_empty());

    LinkPreview {
        url: url.to_string(),
        title: meta(&["og:title", "twitter:title"]).or_else(|| title_tag(html, &lower)).and_then(|title| tidy(title, MAX_TITLE_CHARS)),
        description: meta(&["og:description", "description", "twitter:description"])
            .and_then(|description| tidy(description, MAX_DESCRIPTION_CHARS)),
        text: tidy(visible_text(html, &lower), MAX_TEXT_CHARS),
    }
}

fn title_tag(html: &str, lower: &str) -> Option<String> {
    let open = lower.find("<title")?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    Some(html.get(start..end)?.to_string())
}

/// The `content` of the `<meta>` tag whose `property` or `name` is `key`.
fn meta_content(html: &str, lower: &str, key: &str) -> Option<String> {
    let mut position = 0;
    while let Some(found) = lower[position..].find("<meta") {
        let start = position + found;
        let end = start + lower[start..].find('>')?;
        let tag = html.get(start..end)?;
        let tag_lower = &lower[start..end];
        position = end;

        let matches = |name: &str| attribute(tag, tag_lower, name).is_some_and(|value| value.eq_ignore_ascii_case(key));
        if matches("property") || matches("name") {
            return attribute(tag, tag_lower, "content").map(String::from);
        }
    }
    None
}

fn attribute<'a>(tag: &'a str, tag_lower: &str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=");
    let mut position = 0;
    while let Some(found) = tag_lower[position..].find(&needle) {
        let start = position + found;
        position = start + needle.len();

        let at_boundary = tag_lower[..start].ends_with(|c: char| c.is_whitespace());
        if !at_boundary {
            continue;
        }

        let value = tag.get(position..)?;
        let quote = value.chars().next()?;
        return if quote == '"' || quote == '\'' {
            let value = &value[1..];
            Some(&value[..value.find(quote)?])
        } else {
            Some(value.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or_default())
        };
    }
    None
}

/// The page's text with tags removed and non-content sections skipped entirely.
fn visible_text(html: &str, lower: &str) -> String {
    const SKIPPED: &[&str] = &["script", "style", "noscript", "svg", "head", "title", "nav", "footer", "template"];

    let mut text = String::new();
    let mut position = 0;

    while let Some(found) = lower[position..].find('<') {
        let tag_start = position + found;
        text.push_str(html.get(position..tag_start).unwrap_or_default());
        text.push(' ');

        let after = &lower[tag_start + 1..];
        let skipped = SKIPPED.iter().find(|name| {
            after.strip_prefix(**name).is_some_and(|rest| rest.starts_with(|c: char| c == '>' || c == '/' || c.is_whitespace()))
        });

        let next = match skipped {
            Some(name) => lower[tag_start..]
                .find(&format!("</{name}"))
                .and_then(|close| lower[tag_start + close..].find('>').map(|end| tag_start + close + end + 1)),
            None if after.starts_with("!--") => lower[tag_start..].find("-->").map(|end| tag_start + end + 3),
            None => lower[tag_start..].find('>').map(|end| tag_start + end + 1),
        };

        match next {
            Some(next) => position = next,
            None => return text,
        }
    }

    text.push_str(html.get(position..).unwrap_or_default());
    text
}

fn decode_entities(text: &str) -> String {
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        rest = &rest[start..];

        let entity = rest[1..].find(';').filter(|end| *end <= 8).map(|end| &rest[1..=end]);
        match entity.and_then(decode_entity) {
            Some(character) => {
                decoded.push(character);
                rest = &rest[entity.map_or(0, str::len) + 2..];
            }
            None => {
                decoded.push('&');
                rest = &rest[1..];
            }
        }
    }

    decoded.push_str(rest);
    decoded
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let number = entity.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", text[..end].trim_end()),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use axum::{Router, http::header, response::Redirect, routing::get};

    use super::*;

    fn urls(text: &str) -> Vec<String> {
        extract_urls(text).iter().map(ToString::to_string).collect()
    }

    #[test]
    fn extract_urls_finds_links_and_drops_surrounding_punctuation() {
        assert_eq!(urls("see https://example.com/a, ok?"), vec!["https://example.com/a"]);
        assert_eq!(urls("(http://example.com/b)"), vec!["http://example.com/b"]);
        assert_eq!(urls("<https://example.com/c>"), vec!["https://example.com/c"]);
    }

    #[test]
    fn extract_urls_skips_repeats_other_schemes_and_matrix_links() {
        assert_eq!(urls("https://a.example/ https://a.example/"), vec!["https://a.example/"]);
        assert!(urls("ftp://a.example/ file:///etc/passwd example.com").is_empty());
        assert!(urls("https://matrix.to/#/@gordy:p.ound.town").is_empty());
    }

    #[test]
    fn private_and_special_addresses_are_not_public() {
        for ip in [
            "127.0.0.1", "10.0.0.5", "172.16.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0",
            "255.255.255.255", "224.0.0.1", "::1", "::", "fc00::1", "fd12::1", "fe80::1", "ff02::1",
            "::ffff:192.168.1.1", "::ffff:127.0.0.1", "64:ff9b::a00:1", "2001:db8::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn ordinary_addresses_are_public() {
        for ip in ["1.1.1.1", "8.8.8.8", "172.32.0.1", "2606:4700:4700::1111", "::ffff:1.1.1.1"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn urls_are_limited_to_public_web_addresses() {
        let allowed = |url: &str| is_allowed_url(&Url::parse(url).unwrap(), false);

        assert!(allowed("https://example.com/page"));
        assert!(allowed("http://1.1.1.1/"));
        assert!(!allowed("http://127.0.0.1/"));
        assert!(!allowed("http://192.168.1.1/admin"));
        assert!(!allowed("http://[::1]/"));
        assert!(!allowed("http://[::ffff:10.0.0.1]/"));
        assert!(!allowed("http://2130706433/"), "decimal form of 127.0.0.1");
        assert!(!allowed("http://0x7f.0.0.1/"), "hex form of 127.0.0.1");
        assert!(!allowed("https://example.com:8080/"), "non-web port");
        assert!(!allowed("ftp://example.com/"));
    }

    #[tokio::test]
    async fn resolver_refuses_names_that_only_reach_private_addresses() {
        let resolver = PublicOnlyResolver { allow_private: false };
        let name: Name = "localhost".parse().unwrap();
        assert!(resolver.resolve(name).await.is_err());
    }

    #[test]
    fn parse_html_prefers_social_tags_and_decodes_entities() {
        let html = r#"<html><head><title>Plain title</title>
            <meta property="og:title" content="Tom &amp; Jerry">
            <META NAME='description' CONTENT='It&#39;s a cartoon'>
            <style>.a { color: red }</style><script>var hidden = 1;</script></head>
            <body><nav>Menu</nav><h1>Heading</h1><p>Body  text</p><!-- secret --></body></html>"#;

        let preview = parse_html("https://example.com/", html);
        assert_eq!(preview.title.as_deref(), Some("Tom & Jerry"));
        assert_eq!(preview.description.as_deref(), Some("It's a cartoon"));
        assert_eq!(preview.text.as_deref(), Some("Heading Body text"));
    }

    #[test]
    fn parse_html_falls_back_to_the_title_tag() {
        assert_eq!(parse_html("u", "<title>Just this</title>").title.as_deref(), Some("Just this"));
    }

    #[test]
    fn parse_html_survives_broken_markup_and_multibyte_text() {
        let preview = parse_html("u", "<title>Ünïcödé</title><p>café <b unclosed");
        assert_eq!(preview.title.as_deref(), Some("Ünïcödé"));
        assert_eq!(preview.text.as_deref(), Some("café"));
    }

    #[test]
    fn attribute_does_not_match_inside_longer_names() {
        let tag = r#"<meta data-name="wrong" name="right">"#;
        assert_eq!(attribute(tag, tag, "name"), Some("right"));
    }

    #[test]
    fn decode_entities_leaves_unknown_ones_alone() {
        assert_eq!(decode_entities("a &bogus; b &amp; c & d &#x27;"), "a &bogus; b & c & d '");
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}")
    }

    fn html_page(body: &'static str) -> ([(header::HeaderName, &'static str); 1], &'static str) {
        ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body)
    }

    #[tokio::test]
    async fn fetches_a_page_following_redirects() {
        let app = Router::new()
            .route("/page", get(|| async { html_page("<title>Hello</title><p>World</p>") }))
            .route("/moved", get(|| async { Redirect::temporary("/page") }));
        let base = serve(app).await;

        let fetcher = LinkFetcher::build(true, true).unwrap();
        let previews = fetcher.fetch_all(&format!("look {base}/moved")).await;

        assert_eq!(previews.len(), 1);
        assert_eq!(previews[0].title.as_deref(), Some("Hello"));
        assert_eq!(previews[0].text.as_deref(), Some("World"));
    }

    #[tokio::test]
    async fn the_real_fetcher_refuses_a_local_server() {
        let base = serve(Router::new().route("/", get(|| async { html_page("<title>Internal</title>") }))).await;
        assert!(LinkFetcher::new(true).unwrap().fetch_all(&format!("{base}/")).await.is_empty());
    }

    #[tokio::test]
    async fn redirects_to_disallowed_addresses_are_refused() {
        let app = Router::new().route("/", get(|| async { Redirect::temporary("http://192.168.1.1/") }));
        let base = serve(app).await;

        // A plain client carrying the real policy, since the strict fetcher would
        // refuse this local test server before it ever saw the redirect.
        let client = reqwest::Client::builder().redirect(redirect_policy(false)).build().unwrap();
        assert!(client.get(format!("{base}/")).send().await.is_err());
    }

    #[tokio::test]
    async fn describes_media_without_downloading_it() {
        let app = Router::new().route("/cat.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], "bytes") }));
        let base = serve(app).await;

        let previews = LinkFetcher::build(true, true).unwrap().fetch_all(&format!("{base}/cat.png")).await;
        assert_eq!(previews[0].description.as_deref(), Some("A image/png file."));
    }

    #[tokio::test]
    async fn ignores_other_file_types_and_failures() {
        let app = Router::new().route("/a.zip", get(|| async { ([(header::CONTENT_TYPE, "application/zip")], "zip") }));
        let base = serve(app).await;

        let fetcher = LinkFetcher::build(true, true).unwrap();
        assert!(fetcher.fetch_all(&format!("{base}/a.zip {base}/missing")).await.is_empty());
    }

    #[tokio::test]
    async fn does_nothing_when_disabled() {
        assert!(LinkFetcher::new(false).unwrap().fetch_all("https://example.com/").await.is_empty());
    }
}
