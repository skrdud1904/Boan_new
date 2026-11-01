use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, FixedOffset, Utc};
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde_json::{Value, json};

use crate::briefing;
use crate::output::safe_source_url;
use crate::ranking::RankedArticle;

const CONTENT_LIMIT: usize = 2_000;
const ARTICLE_LIMIT: usize = 5;
const EMBED_TOTAL_LIMIT: usize = 6_000;
const EMBED_ARTICLE_LIMIT: usize = 2_000;

/// Sends one explicitly requested digest. All errors omit the secret webhook URL.
/// No retries are made: a timeout may have occurred after Discord accepted the post.
pub fn send_digest(
    webhook_url: &str,
    articles: &[RankedArticle],
    now: DateTime<Utc>,
) -> Result<()> {
    if articles.is_empty() || articles.len() > ARTICLE_LIMIT {
        bail!("Discord briefing must contain between 1 and 5 articles");
    }
    let mut url = validate_webhook_url(webhook_url)?;
    url.query_pairs_mut().append_pair("wait", "true");
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .redirect(Policy::none())
        .build()
        .map_err(|_| anyhow!("Discord HTTP 클라이언트를 만들지 못했습니다"))?;
    let response = client
        .post(url)
        .json(&webhook_payload(articles, now))
        .send()
        .map_err(|error| {
            if error.is_timeout() {
                anyhow!(
                    "Discord 응답 시간 초과: 전달 여부가 불확실하므로 채널 확인 후 재시도하세요"
                )
            } else if error.is_connect() {
                anyhow!("Discord 연결 실패: 네트워크 연결을 확인하세요")
            } else {
                anyhow!("Discord 요청 실패: 전달 여부를 확인한 후 재시도하세요")
            }
        })?;
    let status = response.status();
    if !status.is_success() {
        // Do not include request URLs, Location headers or response bodies: they can
        // contain the webhook token, including in server-generated errors.
        bail!("Discord 전송 실패 (HTTP {})", status.as_u16());
    }
    Ok(())
}

pub fn validate_webhook_url(raw: &str) -> Result<Url> {
    const INVALID: &str =
        "Discord 웹훅 URL이 잘못되었습니다. HTTPS Discord 웹훅 주소만 사용할 수 있습니다";
    if raw.trim() != raw || raw.chars().any(|c| c.is_control() || c == '\\') {
        bail!(INVALID);
    }
    let url = Url::parse(raw).map_err(|_| anyhow!(INVALID))?;
    if url.scheme() != "https"
        || !matches!(
            url.host_str(),
            Some("discord.com" | "canary.discord.com" | "ptb.discord.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!(INVALID);
    }
    let segments: Vec<_> = url
        .path_segments()
        .ok_or_else(|| anyhow!(INVALID))?
        .collect();
    if segments.len() != 4
        || segments[0] != "api"
        || segments[1] != "webhooks"
        || segments[2].is_empty()
        || segments[2].len() > 20
        || !segments[2].bytes().all(|c| c.is_ascii_digit())
        || segments[3].is_empty()
        || segments[3].len() > 256
        || !segments[3]
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        bail!(INVALID);
    }
    Ok(url)
}

/// Serializable preview of the exact outbound payload. Contains no credentials.
pub fn webhook_payload(articles: &[RankedArticle], now: DateTime<Utc>) -> Value {
    let count = articles.len().min(ARTICLE_LIMIT);
    let budget = (EMBED_TOTAL_LIMIT / count.max(1)).min(EMBED_ARTICLE_LIMIT);
    let embeds: Vec<_> = articles
        .iter()
        .take(ARTICLE_LIMIT)
        .map(|article| render_embed(article, budget))
        .collect();
    json!({
        "content": render_content(articles, now),
        "embeds": embeds,
        "allowed_mentions": {
            "parse": [],
            "users": [],
            "roles": [],
            "replied_user": false,
        },
    })
}

pub fn render_content(articles: &[RankedArticle], now: DateTime<Utc>) -> String {
    let kst = FixedOffset::east_opt(9 * 60 * 60).expect("KST offset is valid");
    let count = articles.len().min(ARTICLE_LIMIT);
    let header = format!(
        "보안뉴스 침해사고 브리핑 · {} KST\n선정 {}건\n\n",
        now.with_timezone(&kst).format("%m/%d %H:%M"),
        count,
    );
    let mut footer =
        "\n보도 내용에 근거한 분류·순위입니다. 사고 사실을 독립적으로 입증한 결과는 아닙니다."
            .to_owned();
    if articles.len() > count {
        footer.push_str(&format!(
            " 추가 {}건은 로컬 보고서에 있습니다.",
            articles.len() - count
        ));
    }
    if count == 0 {
        return format!("{header}선정 조건에 맞는 기사가 없습니다.\n{footer}");
    }

    let content =
        format!("{header}기사별 상세 내용과 원문 링크는 아래 카드에서 확인하세요.\n{footer}");
    debug_assert!(utf16_length(&content) <= CONTENT_LIMIT);
    content
}

fn render_embed(ranked: &RankedArticle, budget: usize) -> Value {
    let brief = briefing::from_article(ranked);
    let title = truncate_utf16(&plain_text(&brief.title), 150);
    let overhead = utf16_length(&title)
        + utf16_length(brief.source)
        + utf16_length(briefing::FOOTER)
        + briefing::SECTION_NAMES
            .iter()
            .map(|name| utf16_length(name))
            .sum::<usize>();
    let available = budget.saturating_sub(overhead);
    // Preserve every section; spend most space on the actual source excerpt.
    let weights = [40, 14, 18, 16, 8, 4];
    let fields: Vec<_> = briefing::SECTION_NAMES
        .iter()
        .zip(brief.sections())
        .zip(weights)
        .map(|((name, value), weight)| {
            let limit = (available * weight / 100).min(1024);
            let value = if *name == briefing::SECTION_NAMES[3] {
                // Keep all three proposals and their labels, even with five cards.
                let line_budget = limit.saturating_sub(2) / 3;
                value
                    .lines()
                    .map(|line| truncate_utf16(&plain_text(line), line_budget))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                truncate_utf16(&plain_text(&value), limit)
            };
            json!({"name": name, "value": value, "inline": false})
        })
        .collect();
    let mut embed = json!({
        "author": {"name": brief.source},
        "title": title,
        "color": 0x2563EB,
        "fields": fields,
        "footer": {"text": briefing::FOOTER},
        "timestamp": ranked.article.published_at.to_rfc3339(),
    });
    if let Some(url) = safe_source_url(&ranked.article.url) {
        embed["url"] = json!(url);
    }
    embed
}

fn utf16_length(value: &str) -> usize {
    value.encode_utf16().count()
}

fn truncate_utf16(value: &str, limit: usize) -> String {
    if utf16_length(value) <= limit {
        return value.to_owned();
    }
    if limit == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for character in value.chars() {
        if used + character.len_utf16() > limit - 1 {
            break;
        }
        result.push(character);
        used += character.len_utf16();
    }
    result.push('…');
    result
}

fn plain_text(value: &str) -> String {
    value
        .chars()
        .map(|c| if "\\`*_~|<>[]()".contains(c) { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_discord_webhook_hosts_and_paths_are_accepted() {
        for host in ["discord.com", "canary.discord.com", "ptb.discord.com"] {
            assert!(
                validate_webhook_url(&format!(
                    "https://{host}/api/webhooks/123456789012345678/fake-token_A.B"
                ))
                .is_ok()
            );
        }
        for url in [
            "http://discord.com/api/webhooks/123/fake-secret",
            "https://discord.com.attacker.test/api/webhooks/123/fake-secret",
            "https://attacker.test/api/webhooks/123/fake-secret",
            "https://user:fake-secret@discord.com/api/webhooks/123/token",
            "https://discord.com:8443/api/webhooks/123/fake-secret",
            "https://discord.com/api/webhooks/123/fake-secret?thread_id=123",
            "https://discord.com/api/webhooks/123/fake-secret#fragment",
            "https://discord.com/api/webhooks/123/fake-secret/messages/1",
            "https://discord.com/api/webhooks/not-a-number/fake-secret",
            "https://discord.com/api/webhooks/123/",
            "https://discord.com/api/webhooks/123/fake%2Fsecret",
            "https://discord.com/api/webhooks/123/fake-secret\n",
            "https://discord.com\\api\\webhooks\\123\\fake-secret",
        ] {
            let error = validate_webhook_url(url).unwrap_err();
            assert!(!error.to_string().contains("fake-secret"));
            assert!(!format!("{error:?}").contains(url));
        }
    }

    #[test]
    fn unicode_truncation_respects_discord_limit_without_splitting_surrogates() {
        assert_eq!(truncate_utf16("🔐🔐🔐", 4), "🔐…");
        assert_eq!(truncate_utf16("침해사고", 3), "침해…");
        assert_eq!(truncate_utf16("abc", 0), "");
        for limit in 0..20 {
            assert!(utf16_length(&truncate_utf16(&"🔐사고".repeat(20), limit)) <= limit);
        }
    }

    #[test]
    fn webhook_payload_disables_mentions_but_does_not_suppress_requested_cards() {
        let now = "2026-11-01T09:00:00Z".parse().unwrap();
        let payload = webhook_payload(&[], now);
        assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
        assert_eq!(payload["allowed_mentions"]["users"], json!([]));
        assert_eq!(payload["allowed_mentions"]["roles"], json!([]));
        assert_eq!(payload["allowed_mentions"]["replied_user"], false);
        assert!(payload.get("flags").is_none());
        assert_eq!(payload["embeds"], json!([]));
        assert!(utf16_length(payload["content"].as_str().unwrap()) <= CONTENT_LIMIT);
    }

    #[test]
    fn five_oversized_article_cards_fit_with_intact_links_and_all_sections() {
        let now = "2026-11-01T09:00:00Z".parse().unwrap();
        let articles: Vec<_> = (1..=6)
            .map(|index| RankedArticle {
                article: crate::models::Article {
                    id: index.to_string(),
                    title: format!("침해사고 {index} @everyone {}", "🔐".repeat(300)),
                    url: format!("https://www.boannews.com/news/articleView.html?idxno={index}"),
                    summary: "한글유출🔐".repeat(300),
                    published_at: now,
                    discovered_at: now,
                },
                score: u32::MAX,
                classification: crate::ranking::IncidentKind::ReportedIncident,
                reasons: vec![],
                evidence: vec![],
            })
            .collect();
        let payload = webhook_payload(&articles, now);
        let content = payload["content"].as_str().unwrap();
        assert!(utf16_length(content) <= CONTENT_LIMIT);
        let embeds = payload["embeds"].as_array().unwrap();
        assert_eq!(embeds.len(), 5);
        let total: usize = embeds
            .iter()
            .enumerate()
            .map(|(index, embed)| {
                assert!(
                    embed["title"]
                        .as_str()
                        .unwrap()
                        .starts_with(&format!("침해사고 {}", index + 1))
                );
                assert_eq!(
                    embed["url"],
                    format!(
                        "https://www.boannews.com/news/articleView.html?idxno={}",
                        index + 1
                    )
                );
                assert_eq!(embed["timestamp"], "2026-11-01T09:00:00+00:00");
                let size = assert_embed_limits(embed);
                assert!(size <= 1200);
                size
            })
            .sum();
        assert!(total <= EMBED_TOTAL_LIMIT);
        assert!(content.contains("추가 1건은 로컬 보고서"));
        assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
    }

    fn assert_embed_limits(embed: &Value) -> usize {
        let title = utf16_length(embed["title"].as_str().unwrap());
        let author = utf16_length(embed["author"]["name"].as_str().unwrap());
        let footer = utf16_length(embed["footer"]["text"].as_str().unwrap());
        assert!(title <= 256 && author <= 256 && footer <= 2048);
        let fields = embed["fields"].as_array().unwrap();
        assert_eq!(fields.len(), briefing::SECTION_NAMES.len());
        let mut size = title + author + footer;
        for (field, name) in fields.iter().zip(briefing::SECTION_NAMES) {
            assert_eq!(field["name"], name);
            let value = field["value"].as_str().unwrap();
            assert!(!value.is_empty());
            assert!(utf16_length(value) <= 1024);
            if name == briefing::SECTION_NAMES[3] {
                assert_eq!(value.lines().count(), 3);
                assert!(value.lines().all(|line| line.starts_with("• 제안:")));
            }
            size += utf16_length(name) + utf16_length(value);
        }
        size
    }

    #[test]
    fn every_batch_size_obeys_card_and_message_limits() {
        let now = "2026-11-01T09:00:00Z".parse().unwrap();
        for count in 1..=5 {
            let articles: Vec<_> = (1..=count)
                .map(|index| RankedArticle {
                    article: crate::models::Article {
                        id: index.to_string(),
                        title: "유출 조사🔐".repeat(200),
                        url: format!(
                            "https://www.boannews.com/news/articleView.html?idxno={index}"
                        ),
                        summary: "원문 발췌🔐".repeat(2000),
                        published_at: now,
                        discovered_at: now,
                    },
                    score: 28,
                    classification: crate::ranking::IncidentKind::SuspectedIncident,
                    reasons: vec![],
                    evidence: vec![],
                })
                .collect();
            let payload = webhook_payload(&articles, now);
            let sizes: Vec<_> = payload["embeds"]
                .as_array()
                .unwrap()
                .iter()
                .map(assert_embed_limits)
                .collect();
            assert!(sizes.iter().all(|size| *size <= EMBED_ARTICLE_LIMIT));
            assert!(sizes.iter().sum::<usize>() <= EMBED_TOTAL_LIMIT);
            assert!(utf16_length(payload["content"].as_str().unwrap()) <= CONTENT_LIMIT);
        }
    }

    #[test]
    fn untrusted_article_text_cannot_create_links_or_hide_limitations() {
        let now = "2026-11-01T09:00:00Z".parse().unwrap();
        let article = RankedArticle {
            article: crate::models::Article {
                id: "1".into(),
                title: "[거짓링크](https://attacker.test) @everyone".into(),
                url: "https://attacker.test/".into(),
                summary: String::new(),
                published_at: now,
                discovered_at: now,
            },
            score: 1,
            classification: crate::ranking::IncidentKind::SuspectedIncident,
            reasons: vec![],
            evidence: vec![],
        };
        let payload = webhook_payload(&[article], now);
        let embed = &payload["embeds"][0];
        assert!(embed.get("url").is_none());
        assert!(!embed["title"].as_str().unwrap().contains('['));
        assert!(
            embed["fields"][0]["value"]
                .as_str()
                .unwrap()
                .contains("제목만 표시")
        );
        assert!(
            embed["footer"]["text"]
                .as_str()
                .unwrap()
                .contains("전문 미확인")
        );
        assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
        assert_embed_limits(embed);
    }
}
