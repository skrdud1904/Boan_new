//! Read-only collection from Boannews' official RSS feed.
//!
//! RSS is untrusted input: links are canonicalized, dates must be present, and
//! neither item links nor resources embedded in descriptions are requested.

use std::{collections::HashSet, io::Read, thread, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use quick_xml::{Reader, events::Event};
use reqwest::{StatusCode, blocking::Client, header::RETRY_AFTER, redirect::Policy};
use rss::Channel;
use scraper::Html;
use url::Url;

use crate::models::Article;

pub const FEED_URL: &str = "https://www.boannews.com/rss/allArticle.xml";
const ROBOTS_URL: &str = "https://www.boannews.com/robots.txt";
const BOT_NAME: &str = "BoanIncidentDigest";
const USER_AGENT: &str = "BoanIncidentDigest/0.1 (personal RSS incident digest)";
const MAX_BODY_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TITLE_CHARACTERS: usize = 500;
const MAX_SUMMARY_CHARACTERS: usize = 6_000;
const MAX_ATTEMPTS: usize = 3;
const MAX_WAIT_SECONDS: u64 = 30;

/// A collection cooldown that the scheduler must preserve across attempts.
/// This remains available through anyhow's context chain via `downcast_ref`.
#[derive(Debug)]
pub struct CollectionDeferred {
    pub retry_after: Duration,
}

impl std::fmt::Display for CollectionDeferred {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "collection deferred for at least {:?}",
            self.retry_after
        )
    }
}

impl std::error::Error for CollectionDeferred {}

/// Fetch robots.txt first, then the official RSS feed if access is permitted.
/// Failure to fetch or interpret robots.txt stops collection.
pub fn fetch_articles() -> Result<Vec<Article>> {
    let client = Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(25))
        .redirect(Policy::custom(|attempt| {
            let target = attempt.url();
            let same_path = attempt
                .previous()
                .first()
                .is_some_and(|original| original.path() == target.path());
            if attempt.previous().len() > 3
                || target.scheme() != "https"
                || !allowed_host(target)
                || !target.username().is_empty()
                || target.password().is_some()
                || target.port().is_some()
                || target.query().is_some()
                || !same_path
            {
                attempt.error("redirect leaves the approved Boannews resource")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .context("could not build the RSS HTTP client")?;

    let robots =
        get_text(&client, ROBOTS_URL, Duration::ZERO).context("could not verify robots.txt")?;
    let crawl_delay = robots_policy(&robots)?;
    if !crawl_delay.is_zero() {
        thread::sleep(crawl_delay);
    }
    let xml = get_text(&client, FEED_URL, crawl_delay)
        .context("could not fetch the Boannews RSS feed")?;
    parse_feed(&xml, Utc::now())
}

/// Parse an RSS snapshot without network access. Invalid individual items are
/// skipped; malformed XML or an oversized document is a collection error.
pub fn parse_feed(xml: &str, now: DateTime<Utc>) -> Result<Vec<Article>> {
    ensure!(
        xml.len() as u64 <= MAX_BODY_BYTES,
        "RSS document exceeds the 2 MiB limit"
    );
    validate_xml_document(xml)?;
    let channel = Channel::read_from(xml.as_bytes()).context("invalid RSS document")?;
    let mut seen = HashSet::new();
    let mut articles = Vec::new();

    for item in channel.items() {
        let Some((id, url)) = item.link().and_then(canonical_article_url) else {
            continue;
        };
        let Some(published_at) = item.pub_date().and_then(parse_publication_date) else {
            continue;
        };
        let title = plain_text(item.title().unwrap_or_default(), MAX_TITLE_CHARACTERS);
        if title.is_empty() || !seen.insert(id.clone()) {
            continue;
        }
        articles.push(Article {
            id,
            title,
            url,
            summary: plain_text(
                item.description().unwrap_or_default(),
                MAX_SUMMARY_CHARACTERS,
            ),
            published_at,
            discovered_at: now,
        });
    }
    ensure!(
        channel.items().is_empty() || !articles.is_empty(),
        "RSS contained articles, but all were rejected due to invalid links, titles, or dates"
    );
    articles.sort_by(|left, right| {
        right
            .published_at
            .cmp(&left.published_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(articles)
}

fn validate_xml_document(xml: &str) -> Result<()> {
    // The RSS parser may stop at the channel/root boundary. Validate the whole
    // document first so truncated feeds and trailing roots are not accepted.
    let mut reader = Reader::from_str(xml);
    let mut depth = 0_u32;
    let mut roots = 0_u32;
    loop {
        match reader
            .read_event()
            .context("malformed XML in RSS document")?
        {
            Event::Start(element) => {
                if depth == 0 {
                    roots += 1;
                    ensure!(roots == 1, "RSS document has multiple XML roots");
                }
                for attribute in element.attributes() {
                    attribute.context("malformed XML attribute in RSS document")?;
                }
                depth += 1;
                ensure!(depth <= 128, "RSS document exceeds XML nesting limit");
            }
            Event::Empty(element) => {
                if depth == 0 {
                    roots += 1;
                    ensure!(roots == 1, "RSS document has multiple XML roots");
                }
                for attribute in element.attributes() {
                    attribute.context("malformed XML attribute in RSS document")?;
                }
            }
            Event::End(_) => {
                depth = depth.checked_sub(1).context("unexpected XML closing tag")?;
            }
            Event::DocType(_) => bail!("DTD declarations are not allowed in RSS"),
            Event::Text(text) if depth == 0 => {
                ensure!(
                    text.as_ref().iter().all(u8::is_ascii_whitespace),
                    "unexpected text outside the RSS root"
                );
            }
            Event::CData(_) | Event::GeneralRef(_) if depth == 0 => {
                bail!("unexpected XML content outside the RSS root");
            }
            Event::Eof => break,
            _ => {}
        }
    }
    ensure!(depth == 0 && roots == 1, "incomplete RSS XML document");
    Ok(())
}

fn allowed_host(url: &Url) -> bool {
    matches!(url.host_str(), Some("boannews.com" | "www.boannews.com"))
}

fn canonical_article_url(raw: &str) -> Option<(String, String)> {
    // Reject control characters before URL parsing can silently normalize them.
    if raw.chars().any(char::is_control) {
        return None;
    }
    let url = Url::parse(FEED_URL).ok()?.join(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !allowed_host(&url)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let key = match url.path() {
        "/news/articleView.html" => "idxno",
        "/media/view.asp" => "idx",
        _ => return None,
    };
    let mut ids = url.query_pairs().filter(|(name, _)| name == key);
    let (_, id) = ids.next()?;
    if ids.next().is_some()
        || id.is_empty()
        || id.len() > 20
        || !id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let id = id.trim_start_matches('0');
    if id.is_empty() {
        return None;
    }
    Some((
        id.to_owned(),
        format!("https://www.boannews.com/news/articleView.html?idxno={id}"),
    ))
}

fn parse_publication_date(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(date) = DateTime::parse_from_rfc2822(value) {
        return Some(date.with_timezone(&Utc));
    }
    if let Ok(date) = DateTime::parse_from_rfc3339(value) {
        return Some(date.with_timezone(&Utc));
    }
    // The publisher currently emits an offset-free timestamp in Korean time.
    let local = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").ok()?;
    let korea = FixedOffset::east_opt(9 * 60 * 60)?;
    Some(
        korea
            .from_local_datetime(&local)
            .single()?
            .with_timezone(&Utc),
    )
}

fn plain_text(value: &str, max_characters: usize) -> String {
    let text = Html::parse_fragment(value)
        .root_element()
        .text()
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    text.chars().take(max_characters).collect()
}

fn robots_policy(body: &str) -> Result<Duration> {
    let body = body.trim_start_matches('\u{feff}');
    // A successful HTML error page or arbitrary text is not a robots policy.
    let directives: Vec<_> = body
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|line| !line.is_empty())
        .collect();
    ensure!(
        !directives.iter().any(|line| line.starts_with('<')),
        "robots.txt contains HTML instead of a policy"
    );
    ensure!(
        !directives.iter().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.trim().eq_ignore_ascii_case("user-agent") && value.trim().is_empty()
            })
        }),
        "robots.txt contains an empty User-agent directive"
    );
    ensure!(
        directives.is_empty()
            || directives.iter().any(|line| {
                line.split_once(':').is_some_and(|(name, _)| {
                    matches!(
                        name.trim().to_ascii_lowercase().as_str(),
                        "user-agent" | "sitemap"
                    )
                })
            }),
        "robots.txt has no recognizable policy directives"
    );
    let mut matcher = robotstxt::DefaultMatcher::default();
    ensure!(
        matcher.one_agent_allowed_by_robots(body, BOT_NAME, FEED_URL),
        "robots.txt disallows collection of the RSS feed"
    );
    applicable_crawl_delay(&directives)
}

/// Crawl-delay is an optional extension, which Google's rule matcher ignores.
/// Respect it for the most specific matching agent group(s).
fn applicable_crawl_delay(lines: &[&str]) -> Result<Duration> {
    #[derive(Default)]
    struct Group<'a> {
        agents: Vec<String>,
        delays: Vec<&'a str>,
        has_rules: bool,
    }
    let mut groups = Vec::new();
    let mut group = Group::default();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "user-agent" {
            if group.has_rules {
                groups.push(group);
                group = Group::default();
            }
            group.agents.push(value.to_ascii_lowercase());
        } else if !group.agents.is_empty() {
            group.has_rules = true;
            if name == "crawl-delay" {
                group.delays.push(value);
            }
        }
    }
    groups.push(group);

    let bot = BOT_NAME.to_ascii_lowercase();
    let matching: Vec<_> = groups
        .iter()
        .filter_map(|group| {
            let specificity = group
                .agents
                .iter()
                .filter_map(|agent| {
                    if agent == "*" {
                        Some(0)
                    } else if !agent.is_empty() && bot.contains(agent.as_str()) {
                        Some(agent.len())
                    } else {
                        None
                    }
                })
                .max()?;
            Some((specificity, group))
        })
        .collect();
    let best = matching.iter().map(|(specificity, _)| *specificity).max();
    let mut seconds = 0.0_f64;
    for (_, group) in matching
        .iter()
        .filter(|(specificity, _)| Some(*specificity) == best)
    {
        for value in &group.delays {
            let delay: f64 = value.parse().context("invalid applicable Crawl-delay")?;
            ensure!(
                delay.is_finite() && (0.0..=MAX_WAIT_SECONDS as f64).contains(&delay),
                "applicable Crawl-delay exceeds the supported 30-second wait"
            );
            seconds = seconds.max(delay);
        }
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn get_text(client: &Client, url: &str, minimum_retry_delay: Duration) -> Result<String> {
    for attempt in 0..MAX_ATTEMPTS {
        let response = match client.get(url).send() {
            Ok(response) => response,
            Err(error)
                if attempt + 1 < MAX_ATTEMPTS && (error.is_timeout() || error.is_connect()) =>
            {
                let delay = retry_delay(
                    None,
                    Utc::now(),
                    Duration::from_secs(1 << attempt),
                    false,
                    minimum_retry_delay,
                )?;
                thread::sleep(delay);
                continue;
            }
            Err(error) => return Err(error).context("HTTP request failed"),
        };
        if is_transient(response.status()) {
            let final_attempt = attempt + 1 == MAX_ATTEMPTS;
            let fallback = if final_attempt {
                Duration::from_secs(300)
            } else {
                Duration::from_secs(1 << attempt)
            };
            // Duplicate or non-text values are malformed, not absent. The
            // empty sentinel follows the conservative malformed-header path.
            let retry_after = if response.headers().get_all(RETRY_AFTER).iter().count() > 1 {
                Some("")
            } else {
                response
                    .headers()
                    .get(RETRY_AFTER)
                    .map(|value| value.to_str().unwrap_or(""))
            };
            let delay = retry_delay(
                retry_after,
                Utc::now(),
                fallback,
                final_attempt,
                minimum_retry_delay,
            )?;
            // Release the response before waiting or making another request.
            drop(response);
            thread::sleep(delay);
            continue;
        }
        let response = response
            .error_for_status()
            .context("HTTP server rejected request")?;
        ensure!(
            response.status().is_success(),
            "HTTP response was not successful"
        );
        if let Some(length) = response.content_length() {
            ensure!(
                length <= MAX_BODY_BYTES,
                "HTTP response exceeds the 2 MiB limit"
            );
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_BODY_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("could not read HTTP response")?;
        ensure!(
            bytes.len() as u64 <= MAX_BODY_BYTES,
            "HTTP response exceeds the 2 MiB limit"
        );
        return String::from_utf8(bytes).context("HTTP response is not valid UTF-8");
    }
    bail!("HTTP retry limit reached")
}

fn is_transient(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn retry_delay(
    value: Option<&str>,
    now: DateTime<Utc>,
    fallback: Duration,
    final_attempt: bool,
    minimum_delay: Duration,
) -> Result<Duration> {
    let delay = match value {
        None => fallback,
        Some(value) => {
            let value = value.trim();
            if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
                match value.parse::<u64>() {
                    Ok(seconds) => Duration::from_secs(seconds),
                    // An oversized unsigned duration must never shorten the
                    // publisher's requested cooldown to the local fallback.
                    Err(_) => Duration::MAX,
                }
            } else if let Ok(date) = DateTime::parse_from_rfc2822(value) {
                (date.with_timezone(&Utc) - now)
                    .to_std()
                    .unwrap_or(Duration::ZERO)
            } else {
                return Err(CollectionDeferred {
                    retry_after: Duration::from_secs(3600),
                }
                .into());
            }
        }
    }
    .max(minimum_delay);
    if final_attempt || delay > Duration::from_secs(MAX_WAIT_SECONDS) {
        return Err(CollectionDeferred { retry_after: delay }.into());
    }
    Ok(delay)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-11-01T08:00:00Z".parse().unwrap()
    }

    #[test]
    fn mixed_dates_cdata_and_utf8_are_normalized() {
        let articles =
            parse_feed(include_str!("../tests/fixtures/mixed-dates.xml"), now()).unwrap();
        assert_eq!(articles.len(), 3);
        assert_eq!(articles[0].id, "103");
        assert_eq!(articles[1].id, "102");
        assert_eq!(articles[2].id, "101");
        assert_eq!(
            articles[2].published_at.to_rfc3339(),
            "2026-11-01T05:00:57+00:00"
        );
        assert_eq!(articles[2].summary, "가상 기업 침해사고 & 복구 공지");
        assert_eq!(articles[2].title, "가상 기업, 침해사고 확인");
        assert!(
            articles
                .iter()
                .all(|article| article.discovered_at == now())
        );
    }

    #[test]
    fn invalid_items_are_skipped_and_legacy_ids_are_deduplicated() {
        let articles =
            parse_feed(include_str!("../tests/fixtures/untrusted-items.xml"), now()).unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].id, "123");
        assert_eq!(
            articles[0].url,
            "https://www.boannews.com/news/articleView.html?idxno=123"
        );
    }

    #[test]
    fn date_errors_are_not_replaced_with_collection_time() {
        for value in [
            "",
            "yesterday",
            "2026-02-30 12:00:00",
            "2026-13-01 12:00:00",
        ] {
            assert!(parse_publication_date(value).is_none(), "{value}");
        }
        assert_eq!(
            parse_publication_date("2026-11-01 14:00:57"),
            parse_publication_date("2026-11-01T14:00:57+09:00")
        );
    }

    #[test]
    fn unsafe_urls_are_rejected() {
        for value in [
            "https://attacker.example/news/articleView.html?idxno=1",
            "https://www.boannews.com.attacker.example/news/articleView.html?idxno=1",
            "https://attacker@www.boannews.com/news/articleView.html?idxno=1",
            "https://www.boannews.com:8443/news/articleView.html?idxno=1",
            "ftp://www.boannews.com/news/articleView.html?idxno=1",
            "https://www.boannews.com/admin/?idxno=1",
            "https://www.boannews.com/news/articleView.html?idxno=1&idxno=2",
            "https://www.boannews.com/news/articleView.html?idxno=abc",
            "https://www.boannews.com/news/articleView.html?idxno=0",
            "https://www.boannews.com/news/articleView.html?idxno=1\n",
            "javascript:alert(1)",
            "file:///news/articleView.html?idxno=1",
        ] {
            assert!(canonical_article_url(value).is_none(), "accepted {value}");
        }
        assert_eq!(
            canonical_article_url("http://boannews.com/media/view.asp?idx=00123&kind=1#top"),
            Some((
                "123".to_owned(),
                "https://www.boannews.com/news/articleView.html?idxno=123".to_owned()
            ))
        );
    }

    #[test]
    fn bad_xml_and_oversized_input_fail() {
        assert!(parse_feed("this is not RSS", now()).is_err());
        assert!(parse_feed("<rss><channel><item></channel></rss>", now()).is_err());
        assert!(parse_feed("<rss><channel></channel>", now()).is_err());
        assert!(parse_feed("<rss><channel></channel></rss><extra/>", now()).is_err());
        assert!(parse_feed("<rss><channel></channel></rss>trailing", now()).is_err());
        assert!(parse_feed(&"x".repeat(MAX_BODY_BYTES as usize + 1), now()).is_err());
    }

    #[test]
    fn nonempty_feed_with_only_invalid_items_is_an_error() {
        let xml = "<rss version=\"2.0\"><channel><title>Test</title><link>https://www.boannews.com/</link><description>Test</description><item><title>Invalid</title><link>https://attacker.example/</link></item></channel></rss>";
        assert!(parse_feed(xml, now()).is_err());
        let empty = "<rss version=\"2.0\"><channel><title>Test</title><link>https://www.boannews.com/</link><description>Test</description></channel></rss>";
        assert!(parse_feed(empty, now()).unwrap().is_empty());
    }

    #[test]
    fn field_limits_preserve_utf8_boundaries() {
        let title = "한".repeat(MAX_TITLE_CHARACTERS + 1);
        let summary = "글".repeat(MAX_SUMMARY_CHARACTERS + 1);
        let xml = format!(
            "<rss version=\"2.0\"><channel><title>Test</title><link>https://www.boannews.com/</link><description>Test</description><item><title>{title}</title><description>{summary}</description><link>https://www.boannews.com/news/articleView.html?idxno=1</link><pubDate>2026-11-01 14:00:00</pubDate></item></channel></rss>"
        );
        let articles = parse_feed(&xml, now()).unwrap();
        assert_eq!(articles[0].title.chars().count(), MAX_TITLE_CHARACTERS);
        assert_eq!(articles[0].summary.chars().count(), MAX_SUMMARY_CHARACTERS);
    }

    #[test]
    fn robots_rules_and_bot_specific_delay_are_respected() {
        let current = "User-agent: *\nDisallow: /admin/\n\nUser-agent: bingbot\nCrawl-delay: 30\n";
        assert_eq!(robots_policy(current).unwrap(), Duration::ZERO);
        assert!(robots_policy("User-agent: *\nDisallow: /rss/").is_err());
        assert!(robots_policy("<html>upstream error</html>").is_err());
        assert!(robots_policy("temporary error").is_err());
        assert_eq!(
            robots_policy("# intentionally empty\n").unwrap(),
            Duration::ZERO
        );
        let specific = "User-agent: *\nCrawl-delay: 20\nUser-agent: BoanIncidentDigest\nAllow: /rss/\nCrawl-delay: 1.5\n";
        assert_eq!(
            robots_policy(specific).unwrap(),
            Duration::from_millis(1500)
        );
        assert!(robots_policy("User-agent: *\nCrawl-delay: 60\n").is_err());
    }

    #[test]
    fn retry_after_supports_seconds_dates_and_deferral() {
        let fallback = Duration::from_secs(1);
        assert_eq!(
            retry_delay(Some("12"), now(), fallback, false, Duration::ZERO).unwrap(),
            Duration::from_secs(12)
        );
        assert_eq!(
            retry_delay(
                Some("Sun, 01 Nov 2026 08:00:20 GMT"),
                now(),
                fallback,
                false,
                Duration::ZERO,
            )
            .unwrap(),
            Duration::from_secs(20)
        );
        assert_eq!(
            retry_delay(None, now(), fallback, false, Duration::ZERO).unwrap(),
            fallback
        );
        assert!(is_transient(StatusCode::TOO_MANY_REQUESTS));
        assert!(!is_transient(StatusCode::FORBIDDEN));
    }

    #[test]
    fn long_cooldowns_survive_error_context_without_truncation() {
        for (header, expected) in [
            ("3600", Duration::from_secs(3600)),
            ("Sun, 01 Nov 2026 09:00:00 GMT", Duration::from_secs(3600)),
            ("18446744073709551615", Duration::from_secs(u64::MAX)),
            ("18446744073709551616", Duration::MAX),
        ] {
            let error = retry_delay(
                Some(header),
                now(),
                Duration::from_secs(1),
                false,
                Duration::ZERO,
            )
            .context("could not verify robots.txt")
            .unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<CollectionDeferred>()
                    .unwrap()
                    .retry_after,
                expected,
                "{header}"
            );
        }
    }

    #[test]
    fn final_transient_response_preserves_even_short_retry_after() {
        let error = retry_delay(
            Some("12"),
            now(),
            Duration::from_secs(300),
            true,
            Duration::ZERO,
        )
        .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<CollectionDeferred>()
                .unwrap()
                .retry_after,
            Duration::from_secs(12)
        );
        let error =
            retry_delay(None, now(), Duration::from_secs(300), true, Duration::ZERO).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<CollectionDeferred>()
                .unwrap()
                .retry_after,
            Duration::from_secs(300)
        );
    }

    #[test]
    fn malformed_retry_after_defers_conservatively() {
        for value in ["bad", "", "-1", "+1", "1.5", "12, 3600"] {
            let error = retry_delay(
                Some(value),
                now(),
                Duration::from_secs(1),
                false,
                Duration::ZERO,
            )
            .unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<CollectionDeferred>()
                    .unwrap()
                    .retry_after,
                Duration::from_secs(3600),
                "{value}"
            );
        }
    }

    #[test]
    fn retry_after_date_keeps_fractional_remaining_seconds() {
        let fractional_now = now() + chrono::Duration::milliseconds(250);
        assert_eq!(
            retry_delay(
                Some("Sun, 01 Nov 2026 08:00:20 GMT"),
                fractional_now,
                Duration::from_secs(1),
                false,
                Duration::ZERO,
            )
            .unwrap(),
            Duration::from_millis(19_750)
        );
    }

    #[test]
    fn retries_preserve_crawl_delay_and_longer_server_backoff() {
        let minimum = robots_policy("User-agent: *\nCrawl-delay: 20\n").unwrap();
        for (header, fallback, expected) in [
            // Reconnect and status retries without a server header.
            (None, 1, 20),
            (None, 2, 20),
            // A shorter Retry-After cannot override the robots minimum.
            (Some("12"), 1, 20),
            (Some("25"), 1, 25),
        ] {
            assert_eq!(
                retry_delay(header, now(), Duration::from_secs(fallback), false, minimum).unwrap(),
                Duration::from_secs(expected)
            );
        }
        let error =
            retry_delay(Some("12"), now(), Duration::from_secs(300), true, minimum).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<CollectionDeferred>()
                .unwrap()
                .retry_after,
            minimum
        );
        let error =
            retry_delay(Some("3600"), now(), Duration::from_secs(1), false, minimum).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<CollectionDeferred>()
                .unwrap()
                .retry_after,
            Duration::from_secs(3600)
        );
    }

    #[test]
    fn empty_robots_user_agent_is_rejected_even_with_other_valid_groups() {
        for policy in [
            "User-agent:\nDisallow: /rss/\n",
            "USER-AGENT:   # missing token\nDisallow: /rss/\n",
            "User-agent: *\nAllow: /\nUser-agent: \nDisallow: /rss/\n",
        ] {
            assert!(robots_policy(policy).is_err(), "accepted {policy}");
        }
    }
}
