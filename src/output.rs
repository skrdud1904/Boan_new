use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset, Utc};
use serde_json::json;

use crate::{briefing, discord, ranking::RankedArticle};

static REPORT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub(crate) const REPORT_NOTICE: &str = "기사의 보도 내용과 메타데이터를 기준으로 분류·선정했습니다. 점수는 중요도 순위이며, 사고 사실을 독립적으로 입증한 확률이 아닙니다.";

/// Writes an exclusive JSON/Markdown report pair and returns (JSON path, Markdown path).
/// Existing reports are never overwritten, including when timestamps are identical.
pub fn write_digest(
    directory: &Path,
    articles: &[RankedArticle],
    now: DateTime<Utc>,
) -> Result<(PathBuf, PathBuf)> {
    let report = json!({
        "schema_version": 2,
        "generated_at": now,
        "display_timezone": "Asia/Seoul",
        "selected_count": articles.len(),
        "verification_notice": REPORT_NOTICE,
        "articles": articles,
        "briefings": articles.iter().map(briefing::from_article).collect::<Vec<_>>(),
        "discord_payload": discord::webhook_payload(articles, now),
    });
    let json_bytes = serde_json::to_vec_pretty(&report).context("JSON 보고서 생성 실패")?;
    let markdown = render_markdown(articles, now);
    fs::create_dir_all(directory).context("보고서 디렉터리 생성 실패")?;

    let (json_path, markdown_path, mut json_file, mut markdown_file) =
        reserve_report_pair(directory, now)?;
    let write_result: Result<()> = (|| {
        json_file.write_all(&json_bytes)?;
        json_file.write_all(b"\n")?;
        json_file.sync_all()?;
        markdown_file.write_all(markdown.as_bytes())?;
        markdown_file.sync_all()?;
        Ok(())
    })();
    drop(json_file);
    drop(markdown_file);
    if let Err(error) = write_result {
        // These two paths were exclusively created by this call, never user files.
        let _ = fs::remove_file(&json_path);
        let _ = fs::remove_file(&markdown_path);
        return Err(error).context("보고서 저장 실패");
    }
    Ok((json_path, markdown_path))
}

fn reserve_report_pair(
    directory: &Path,
    now: DateTime<Utc>,
) -> Result<(PathBuf, PathBuf, File, File)> {
    for _ in 0..128 {
        let sequence = REPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let stem = format!(
            "incidents-{}-{}-{sequence}",
            now.format("%Y%m%dT%H%M%S%.9fZ"),
            std::process::id(),
        );
        let json_path = directory.join(format!("{stem}.json"));
        let markdown_path = directory.join(format!("{stem}.md"));
        let json_file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&json_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("JSON 보고서 파일 생성 실패"),
        };
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&markdown_path)
        {
            Ok(markdown_file) => return Ok((json_path, markdown_path, json_file, markdown_file)),
            Err(error) => {
                drop(json_file);
                fs::remove_file(&json_path).context("미완성 보고서 예약 파일 정리 실패")?;
                if error.kind() != ErrorKind::AlreadyExists {
                    return Err(error).context("Markdown 보고서 파일 생성 실패");
                }
            }
        }
    }
    bail!("다른 보고서와 충돌하지 않는 파일 이름을 확보하지 못했습니다")
}

pub fn render_markdown(articles: &[RankedArticle], now: DateTime<Utc>) -> String {
    let kst = FixedOffset::east_opt(9 * 60 * 60).expect("KST offset is valid");
    let mut markdown = format!(
        "# 보안뉴스 침해사고 브리핑\n\n생성: {} KST · 선정: {}건\n\n{}\n\n",
        now.with_timezone(&kst).format("%Y-%m-%d %H:%M:%S"),
        articles.len(),
        REPORT_NOTICE,
    );
    if articles.is_empty() {
        markdown.push_str("선정 조건에 맞는 침해사고 기사가 없습니다.\n");
        return markdown;
    }

    for (index, ranked) in articles.iter().enumerate() {
        let article = &ranked.article;
        markdown.push_str(&format!(
            "## {}. {}\n\n",
            index + 1,
            escape_markdown(&single_line(&article.title)),
        ));
        let classification = ranked.classification.label();
        markdown.push_str(&format!(
            "보도 시각: {} KST · 점수: {} · 분류: {}\n\n",
            article
                .published_at
                .with_timezone(&kst)
                .format("%Y-%m-%d %H:%M"),
            ranked.score,
            escape_markdown(classification),
        ));
        let brief = briefing::from_article(ranked);
        for (section, value) in briefing::SECTION_NAMES.iter().zip(brief.sections()) {
            markdown.push_str(&format!("### {section}\n\n"));
            if *section == briefing::SECTION_NAMES[0] {
                markdown.push_str(&escape_markdown(&truncate_chars(&single_line(&value), 250)));
            } else {
                for line in value.lines() {
                    markdown.push_str(&escape_markdown(line));
                    markdown.push('\n');
                }
            }
            markdown.push_str("\n\n");
        }
        markdown.push_str(briefing::FOOTER);
        markdown.push_str("\n\n");
        if let Some(url) = safe_source_url(&article.url) {
            markdown.push_str(&format!("[보안뉴스 원문](<{url}>)\n\n"));
        }
        if !ranked.reasons.is_empty() {
            markdown.push_str("선정 근거:\n\n");
            for reason in &ranked.reasons {
                markdown.push_str(&format!("- {}\n", escape_markdown(&single_line(reason))));
            }
            markdown.push('\n');
        }
    }
    markdown
}

pub(crate) fn safe_source_url(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw).ok()?;
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("boannews.com" | "www.boannews.com"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    Some(url.to_string().replace('<', "%3C").replace('>', "%3E"))
}

fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn escape_markdown(value: &str) -> String {
    let mut result = String::new();
    for character in value.chars() {
        match character {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '\\' | '[' | ']' | '(' | ')' | '*' | '_' | '`' | '#' | '!' | '|' | '~' => {
                result.push('\\');
                result.push(character);
            }
            _ => result.push(character),
        }
    }
    result
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    if limit == 0 {
        return String::new();
    }
    let mut result: String = value.chars().take(limit - 1).collect();
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_with_the_same_timestamp_do_not_overwrite_each_other() {
        let directory = tempfile::tempdir().unwrap();
        let now = "2026-11-01T09:00:00Z".parse().unwrap();
        let first = write_digest(directory.path(), &[], now).unwrap();
        let first_json = fs::read(&first.0).unwrap();
        let second = write_digest(directory.path(), &[], now).unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(&first.0).unwrap(), first_json);
        assert!(first.1.exists() && second.0.exists() && second.1.exists());
        let report: serde_json::Value = serde_json::from_slice(&first_json).unwrap();
        assert_eq!(report["selected_count"], 0);
        assert_eq!(report["display_timezone"], "Asia/Seoul");
        assert_eq!(report["schema_version"], 2);
        assert_eq!(report["discord_payload"]["embeds"], json!([]));
    }

    #[test]
    fn report_uses_kst_and_handles_an_empty_selection() {
        let now = "2026-11-01T18:30:00Z".parse().unwrap();
        let markdown = render_markdown(&[], now);
        assert!(markdown.contains("2026-11-02 03:30:00 KST"));
        assert!(markdown.contains("선정 조건에 맞는"));
        assert!(markdown.contains("독립적으로 입증한 확률이 아닙니다"));
    }

    #[test]
    fn source_links_cannot_use_active_content_or_impostor_hosts() {
        assert!(safe_source_url("javascript:alert(1)").is_none());
        assert!(safe_source_url("https://www.boannews.com.attacker.test/").is_none());
        assert!(safe_source_url("https://name:password@www.boannews.com/").is_none());
        assert!(safe_source_url("https://www.boannews.com:8443/").is_none());
        assert!(safe_source_url("https://www.boannews.com/media/view.asp?idx=123").is_some());
    }

    #[test]
    fn korean_and_emoji_summary_is_shortened_without_splitting_unicode() {
        let summary = "침해🔐".repeat(100);
        let shortened = truncate_chars(&summary, 250);
        assert_eq!(shortened.chars().count(), 250);
        assert!(shortened.ends_with('…'));
        assert_eq!(truncate_chars("abc", 0), "");
    }

    #[test]
    fn article_text_is_not_treated_as_markdown_or_html() {
        assert_eq!(
            escape_markdown("[click](javascript:x)<script>"),
            "\\[click\\]\\(javascript:x\\)&lt;script&gt;"
        );
        assert_eq!(single_line("one\n\n two\tthree"), "one two three");
    }

    #[test]
    fn report_keeps_source_and_classification_with_a_short_summary() {
        let now = "2026-11-01T09:00:00Z".parse().unwrap();
        let article = RankedArticle {
            article: crate::models::Article {
                id: "123".into(),
                title: "침해사고 <script> 경고".into(),
                url: "https://www.boannews.com/news/articleView.html?idxno=123".into(),
                summary: "유출사고".repeat(100),
                published_at: now,
                discovered_at: now,
            },
            score: 77,
            classification: crate::ranking::IncidentKind::SuspectedIncident,
            reasons: vec!["조사 단계로 보도됨".into()],
            evidence: vec!["원문 근거".into()],
        };
        let markdown = render_markdown(&[article], now);
        assert!(markdown.contains("의혹·조사 단계 보도"));
        assert!(markdown.contains("점수: 77"));
        assert!(markdown.contains(
            "[보안뉴스 원문](<https://www.boannews.com/news/articleView.html?idxno=123>)"
        ));
        assert!(markdown.contains("조사 단계로 보도됨"));
        for section in briefing::SECTION_NAMES {
            assert!(markdown.contains(section));
        }
        assert!(markdown.contains("규칙 기반 해석/제안 포함"));
        assert!(!markdown.contains("<script>"));
        let summary = markdown
            .lines()
            .find(|line| line.starts_with("유출사고"))
            .unwrap();
        assert_eq!(summary.chars().count(), 250);
    }
}
