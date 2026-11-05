//! Renders pages in a headless Chrome, so sites that build their content with
//! JavaScript, like Reddit, have something to read. Chrome runs in its own container
//! with no secrets and no route to the internet except `browser_proxy`, since it runs
//! whatever scripts a posted page carries.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chromiumoxide::Browser;
use chromiumoxide::cdp::browser_protocol::target::{CreateBrowserContextParams, CreateTargetParams};
use futures_util::StreamExt;
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, warn};
use url::Url;

/// Longest one page gets, from opening it to reading its HTML.
const RENDER_TIMEOUT: Duration = Duration::from_secs(20);
/// How long after the load event to wait for scripts to fill the page in.
const SETTLE_TIME: Duration = Duration::from_millis(1500);
/// Scrolls to the bottom of a page read in full, each followed by a wait. Sites like
/// Reddit only load comments as they're scrolled into view.
const SCROLL_PASSES: usize = 3;
const SCROLL_WAIT: Duration = Duration::from_millis(1000);
/// Pages rendered at once. Each is a browser tab, which costs real memory.
const MAX_CONCURRENT_RENDERS: usize = 2;

/// A connection to the headless browser, opened on first use and reopened after the
/// browser restarts.
pub struct PageBrowser {
    /// The browser's DevTools address, like `chrome:9222`.
    host: String,
    port: u16,
    /// The proxy as the browser reaches it, like `http://matrix-llm-bot:3128`.
    proxy_url: String,
    user_agent: &'static str,
    browser: Mutex<Option<Arc<Browser>>>,
    slots: Semaphore,
}

impl PageBrowser {
    pub fn new(browser_url: &str, proxy_url: &str, user_agent: &'static str) -> Result<Self> {
        let url = Url::parse(browser_url).context("browser_url isn't a valid URL")?;
        let host = url.host_str().context("browser_url has no host")?.to_string();
        let port = url.port_or_known_default().context("browser_url has no port")?;
        Ok(Self {
            host,
            port,
            proxy_url: proxy_url.to_string(),
            user_agent,
            browser: Mutex::new(None),
            slots: Semaphore::new(MAX_CONCURRENT_RENDERS),
        })
    }

    /// The browser's host, which the proxy uses to accept only its connections.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The page's HTML after its scripts have run. A `full` render scrolls to the bottom
    /// a few times first, so content that loads as it's scrolled to is there too. It
    /// takes a few seconds more, so previews don't.
    pub async fn render(&self, url: &Url, full: bool) -> Result<String> {
        let _slot = self.slots.acquire().await?;
        let browser = self.connected().await?;

        // A context of its own for each page, so pages share no cookies, and so the
        // proxy is set for everything the page loads.
        let context = browser
            .create_browser_context(CreateBrowserContextParams {
                proxy_server: Some(self.proxy_url.clone()),
                // Loopback is otherwise sent direct. This sends it to the proxy too,
                // which refuses it.
                proxy_bypass_list: Some("<-loopback>".to_string()),
                dispose_on_detach: Some(true),
                ..Default::default()
            })
            .await;
        let context = match context {
            Ok(context) => context,
            Err(err) => {
                // Most likely the browser restarted, so connect again next time.
                *self.browser.lock().await = None;
                return Err(anyhow!(err).context("failed to open a browser context"));
            }
        };

        let rendered = tokio::time::timeout(RENDER_TIMEOUT, async {
            let mut target = CreateTargetParams::new("about:blank");
            target.browser_context_id = Some(context.clone());
            let page = browser.new_page(target).await?;
            page.set_user_agent(self.user_agent).await?;
            let html = async {
                page.goto(url.as_str()).await?;
                tokio::time::sleep(SETTLE_TIME).await;
                if full {
                    for _ in 0..SCROLL_PASSES {
                        page.evaluate("window.scrollTo(0, document.body.scrollHeight)").await?;
                        tokio::time::sleep(SCROLL_WAIT).await;
                    }
                }
                page.content().await
            }
            .await;
            let _ = page.close().await;
            html
        })
        .await;

        if let Err(err) = browser.dispose_browser_context(context).await {
            debug!(?err, "failed to close a browser context");
        }

        match rendered {
            Ok(Ok(html)) => Ok(html),
            Ok(Err(err)) => Err(anyhow!(err).context("browser failed to render the page")),
            Err(_) => Err(anyhow!("browser timed out rendering the page")),
        }
    }

    async fn connected(&self) -> Result<Arc<Browser>> {
        let mut slot = self.browser.lock().await;
        if let Some(browser) = slot.as_ref() {
            return Ok(Arc::clone(browser));
        }

        // Chrome only accepts DevTools requests addressed to an IP or localhost, so a
        // container name is resolved here first.
        let addr: SocketAddr = tokio::net::lookup_host((self.host.as_str(), self.port))
            .await
            .context("failed to resolve the browser's address")?
            .next()
            .context("the browser's name has no address")?;
        let (browser, mut handler) = Browser::connect(websocket_url(addr).await?).await.context("failed to connect to the browser")?;
        tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if let Err(err) = event {
                    debug!(?err, "browser connection event failed");
                }
            }
            warn!("browser connection closed");
        });

        let browser = Arc::new(browser);
        *slot = Some(Arc::clone(&browser));
        Ok(browser)
    }
}

/// The browser's DevTools WebSocket, at the address it was reached on. The URL Chrome
/// gives names its own view of itself, like `127.0.0.1:9223` behind the image's relay
/// on 9222, which isn't reachable from here.
async fn websocket_url(addr: SocketAddr) -> Result<String> {
    let version: serde_json::Value = reqwest::Client::new()
        .get(format!("http://{addr}/json/version"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("failed to reach the browser")?
        .json()
        .await
        .context("the browser's version reply wasn't JSON")?;
    let given = version["webSocketDebuggerUrl"].as_str().context("the browser gave no WebSocket URL")?;
    reachable_websocket_url(given, addr)
}

fn reachable_websocket_url(given: &str, addr: SocketAddr) -> Result<String> {
    let mut url = Url::parse(given).context("the browser's WebSocket URL is invalid")?;
    let host = match addr.ip() {
        std::net::IpAddr::V4(ip) => ip.to_string(),
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
    };
    url.set_host(Some(&host))?;
    url.set_port(Some(addr.port())).map_err(|()| anyhow!("can't set the WebSocket port"))?;
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_websocket_url_uses_the_address_the_browser_was_reached_on() {
        let addr: SocketAddr = "172.20.0.3:9222".parse().unwrap();
        let url = reachable_websocket_url("ws://127.0.0.1:9223/devtools/browser/abc", addr).unwrap();
        assert_eq!(url, "ws://172.20.0.3:9222/devtools/browser/abc");
    }

    use super::*;

    /// Needs a running Chrome, so it's skipped unless asked for:
    /// `BROWSER_TEST_URL=http://127.0.0.1:9222 cargo test -- --ignored renders_through_the_proxy`
    #[tokio::test]
    #[ignore]
    async fn renders_through_the_proxy() {
        let browser_url = std::env::var("BROWSER_TEST_URL").expect("BROWSER_TEST_URL");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_url = format!("http://{}", listener.local_addr().unwrap());
        let browser = PageBrowser::new(&browser_url, &proxy_url, crate::links::USER_AGENT).unwrap();
        tokio::spawn(crate::browser_proxy::serve(listener, browser.host().to_string()));

        let html = browser.render(&Url::parse("https://example.com/").unwrap(), true).await.unwrap();
        assert!(html.contains("Example Domain"), "{html}");

        // A page can't reach the bot's own network through the browser.
        let local = browser.render(&Url::parse("https://127.0.0.1:9/").unwrap(), false).await;
        assert!(local.is_err() || !local.unwrap().contains("Example"), "loopback went through");

        // With the proxy gone, nothing loads: the page really did come through it.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let unproxied = PageBrowser::new(&browser_url, &format!("http://{closed}"), crate::links::USER_AGENT).unwrap();
        let html = unproxied.render(&Url::parse("https://example.com/").unwrap(), false).await;
        assert!(html.is_err() || !html.unwrap().contains("Example Domain"), "loaded without the proxy");
    }
}
