use std::{collections::HashSet, fs, process::Command};

use boannews_incident_brief::{crawler, output, ranking, store::Store};
use chrono::{Duration, Utc};

fn sample_feed() -> String {
    let date = (Utc::now() - Duration::hours(1)).to_rfc2822();
    let items = [
        (
            "1",
            "예시병원, 랜섬웨어 감염으로 진료 중단",
            "국내 예시병원이 랜섬웨어 공격을 받았다. 환자정보 유출 확인 후 시스템 복구 중이다.",
        ),
        (
            "2",
            "예시쇼핑몰, 고객정보 유출 의혹 조사 중",
            "예시쇼핑몰 고객정보 유출 정황이 발견됐다. 실제 노출 여부는 미확인이다.",
        ),
        (
            "3",
            "테스트 도구 취약점 공개",
            "결함을 악용하면 정보를 탈취할 수 있다. 실제 악용 정황은 없다.",
        ),
        (
            "4",
            "보안기업, 랜섬웨어 대응 솔루션 출시",
            "새로운 보안 제품을 출시하고 예방교육을 실시한다.",
        ),
    ];
    let body: String = items.iter().map(|(id, title, summary)| format!(
        "<item><title>{title}</title><link>https://www.boannews.com/news/articleView.html?idxno={id}</link><description>{summary}</description><pubDate>{date}</pubDate></item>"
    )).collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><rss version=\"2.0\"><channel><title>합성 검증용 기사</title><link>https://www.boannews.com</link><description>실제 사건이 아닌 테스트</description>{body}</channel></rss>"
    )
}

#[test]
fn rss_storage_selection_reports_and_sent_history_work_together() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("news.sqlite3");
    let now = Utc::now();
    let parsed = crawler::parse_feed(&sample_feed(), now).unwrap();
    assert_eq!(parsed.len(), 4);
    {
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.upsert(&parsed).unwrap(), 4);
        assert_eq!(store.upsert(&parsed).unwrap(), 0);
    }
    let mut store = Store::open(&path).unwrap();
    let selected = ranking::rank_articles(&store.articles().unwrap(), now, 24, 5, &HashSet::new());
    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0].article.id, "1");
    assert_eq!(selected[1].article.id, "2");
    assert_eq!(
        selected[1].classification,
        ranking::IncidentKind::SuspectedIncident
    );
    let (json, markdown) = output::write_digest(directory.path(), &selected, now).unwrap();
    let report: serde_json::Value = serde_json::from_slice(&fs::read(json).unwrap()).unwrap();
    assert_eq!(report["selected_count"], 2);
    assert!(
        !report["articles"][0]["evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(fs::read_to_string(markdown).unwrap().contains("예시병원"));
    store.mark_sent(&["1".into()], now).unwrap();
    let remaining = ranking::rank_articles(
        &store.articles().unwrap(),
        now,
        24,
        5,
        &store.sent_ids().unwrap(),
    );
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].article.id, "2");
}

#[test]
fn offline_cli_and_uncertain_delivery_recovery_never_need_network() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = directory.path().join("feed.xml");
    fs::write(&fixture, sample_feed()).unwrap();
    let binary = env!("CARGO_BIN_EXE_boannews-incident-brief");
    let run = |args: &[&str]| {
        Command::new(binary)
            .arg("--data-dir")
            .arg(directory.path())
            .args(args)
            .env_remove("DISCORD_WEBHOOK_URL_FILE")
            .env(
                "DISCORD_WEBHOOK_URL",
                "https://discord.com/api/webhooks/123/FAKE_TEST_ONLY",
            )
            .output()
            .unwrap()
    };
    let collected = run(&["collect", "--fixture", fixture.to_str().unwrap()]);
    assert!(
        collected.status.success(),
        "{}",
        String::from_utf8_lossy(&collected.stderr)
    );
    assert!(run(&["digest"]).status.success());
    let mut store = Store::open(&directory.path().join("news.sqlite3")).unwrap();
    assert!(store.sent_ids().unwrap().is_empty());
    store
        .set_metadata(
            "pending_delivery",
            &serde_json::json!({
                "article_ids": ["1"], "attempted_at": Utc::now(),
                "scheduled_date": "2026-11-01",
            })
            .to_string(),
        )
        .unwrap();
    let blocked = run(&["digest", "--send"]);
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("pending/uncertain"));
    assert!(run(&["resolve-delivery", "--delivered"]).status.success());
    assert!(store.sent_ids().unwrap().contains("1"));
    assert_eq!(
        store
            .get_metadata("last_sent_daily_date")
            .unwrap()
            .as_deref(),
        Some("2026-11-01")
    );
    assert!(store.get_metadata("pending_delivery").unwrap().is_none());
    store
        .set_metadata(
            "pending_delivery",
            &serde_json::json!({
                "article_ids": ["2"], "attempted_at": Utc::now(),
            })
            .to_string(),
        )
        .unwrap();
    assert!(
        run(&["resolve-delivery", "--not-delivered"])
            .status
            .success()
    );
    assert!(!store.sent_ids().unwrap().contains("2"));
    assert!(store.get_metadata("pending_delivery").unwrap().is_none());
    // Ensure a failed transaction cannot partially mark a batch as delivered.
    assert!(
        store
            .mark_sent(&["2".into(), "missing".into()], Utc::now())
            .is_err()
    );
    assert!(!store.sent_ids().unwrap().contains("2"));
}
