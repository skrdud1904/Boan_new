//! Independent, synthetic adversarial checks of observable ranking behavior.
//! Organization names used to exercise alias handling do not assert real events.

use std::collections::HashSet;

use boannews_incident_brief::models::Article;
use boannews_incident_brief::ranking::{IncidentKind, RankedArticle, rank_articles};
use chrono::{DateTime, Duration, Utc};

fn clock() -> DateTime<Utc> {
    "2026-11-01T09:00:00Z".parse().unwrap()
}

fn news(id: &str, title: &str, summary: &str) -> Article {
    Article {
        id: id.into(),
        title: title.into(),
        summary: summary.into(),
        url: format!("https://www.boannews.com/news/articleView.html?idxno={id}"),
        published_at: clock() - Duration::hours(2),
        discovered_at: clock(),
    }
}

fn select(articles: &[Article]) -> Vec<RankedArticle> {
    rank_articles(articles, clock(), 24, 5, &HashSet::new())
}

#[test]
fn definite_patient_data_exposure_is_a_reported_incident() {
    let articles = [news(
        "101",
        "청솔의료원, 환자정보 7만 건 유출 확인",
        "조사기관은 진료기록이 외부 서버에 게시된 사실을 확인했다.",
    )];
    let selected = select(&articles);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].classification, IncidentKind::ReportedIncident);
}

#[test]
fn observed_attack_and_simulated_exercise_are_distinguished() {
    let articles = [
        news(
            "102",
            "택배 안내를 사칭한 새 공격 캠페인 포착",
            "관측팀은 공격자가 악성코드를 유포하고 있다고 발표했다.",
        ),
        news(
            "103",
            "하늘교육원, 랜섬웨어 공격 대응 모의훈련 실시",
            "훈련에서는 가상 시스템의 개인정보 유출 사고를 재현했다.",
        ),
    ];
    let selected = select(&articles);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].article.id, "102");
    assert_eq!(selected[0].classification, IncidentKind::ActiveAttack);
}

#[test]
fn hypothetical_harm_of_a_new_flaw_is_not_an_observed_incident() {
    let articles = [news(
        "104",
        "연구팀, 가정용 라우터 인증 결함 공개",
        "이 결함을 악용하면 랜섬웨어 공격과 개인정보 유출 사고로 이어질 수 있다. 실제 공격 사례는 보고되지 않았다.",
    )];
    assert!(
        select(&articles).is_empty(),
        "가능한 피해를 설명하는 취약점 발표를 실제 사고로 선정하면 안 된다",
    );
}

#[test]
fn negative_investigation_result_is_not_a_suspected_incident() {
    let articles = [news(
        "105",
        "다온협회 조사 종료, 개인정보 유출 정황은 발견되지 않았다",
        "조사기관은 외부 반출을 확인할 자료가 없다고 설명했다.",
    )];
    assert!(
        select(&articles).is_empty(),
        "유출 정황이 없다는 조사 결과의 부정을 해석해야 한다",
    );
}

#[test]
fn uncertain_actor_does_not_make_confirmed_occurrence_uncertain() {
    let articles = [news(
        "106",
        "새봄병원 해킹 피해 확인, 공격 배후는 해외 조직으로 추정",
        "병원과 조사기관이 침해사고 발생을 확인했다. 공격자의 신원은 아직 밝혀지지 않았다.",
    )];
    let selected = select(&articles);
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected[0].classification,
        IncidentKind::ReportedIncident,
        "사고 발생 확인과 공격자 신원에 대한 추정을 구분해야 한다",
    );
}

#[test]
fn unverified_database_seller_claim_remains_suspected() {
    let articles = [news(
        "107",
        "누리상점 고객정보 유출 주장, 자료 진위 확인 중",
        "판매자가 게시한 데이터의 실제 소유자와 유출 여부는 확인되지 않았다.",
    )];
    let selected = select(&articles);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].classification, IncidentKind::SuspectedIncident);
}

#[test]
fn named_expert_column_is_excluded_even_when_it_recounts_real_harm() {
    let articles = [news(
        "108",
        "[이연구 박사 칼럼] 지역 의료기관의 보안 투자 방향",
        "지난해 병원 랜섬웨어 공격으로 진료 중단이 발생했다. 개인정보 유출 사고를 교훈 삼아 예산을 늘려야 한다.",
    )];
    assert!(select(&articles).is_empty());
}

#[test]
fn investigation_and_theft_followups_for_same_exchange_are_one_selection() {
    let articles = [
        news("109", "바이낸스 자산 탈취 사고, 이용자 피해 접수", ""),
        news("110", "바이낸스 해킹 사건 합동 조사 개시", ""),
    ];
    let selected = select(&articles);
    assert_eq!(selected.len(), 1);
    assert!(
        selected[0]
            .reasons
            .iter()
            .any(|reason| reason.contains("후속"))
    );
}

#[test]
fn different_victims_are_not_grouped_by_shared_financial_attack_vocabulary() {
    let articles = [
        news("111", "바이낸스 자산 탈취 사고, 이용자 피해 접수", ""),
        news("112", "업비트 해킹 사건, 이용자 피해 접수", ""),
    ];
    assert_eq!(select(&articles).len(), 2);
}

#[test]
fn organization_named_as_analyst_is_not_assumed_to_be_the_victim() {
    let articles = [
        news("113", "바이낸스 자산 탈취 사고, 이용자 피해 접수", ""),
        news(
            "114",
            "바이낸스 분석팀, 새롬거래소 해킹 사건 보고",
            "새롬거래소는 고객 지갑이 침해됐다고 확인했다.",
        ),
    ];
    assert_eq!(
        select(&articles).len(),
        2,
        "분석 기관과 피해 기관이 다른 사건을 합치면 안 된다",
    );
}

#[test]
fn product_announcement_with_harm_examples_is_not_an_incident_report() {
    let articles = [news(
        "115",
        "새빛소프트, 신규 랜섬웨어 대응 솔루션 공개",
        "랜섬웨어 공격과 개인정보 유출 사고를 사전에 막는 제품이다. 다음 달부터 판매한다.",
    )];
    assert!(
        select(&articles).is_empty(),
        "제품 설명에 포함된 일반적인 사고 용어가 실제 발생 근거는 아니다",
    );
}
