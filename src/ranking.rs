//! Explainable, local prioritization of article-reported security incidents.
//!
//! These rules identify what an article reports; they do not independently verify
//! the incident. Korean lexical rules can miss paraphrases and ambiguous negation.
//! Follow-up grouping uses a short, explicit organization alias list and matching
//! incident families within 24 hours; unrelated incidents can still look similar.

use std::collections::HashSet;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::models::Article;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentKind {
    /// The article reports an incident as having occurred, not an independent finding.
    ReportedIncident,
    /// The article reports an observed attack campaign or exploitation in progress.
    ActiveAttack,
    /// The article leaves the occurrence or actual exposure unconfirmed.
    SuspectedIncident,
}

impl IncidentKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::ReportedIncident => "사고 발생 보도",
            Self::ActiveAttack => "공격 활동 보도",
            Self::SuspectedIncident => "의혹·조사 단계 보도",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RankedArticle {
    pub article: Article,
    pub score: u32,
    pub classification: IncidentKind,
    pub reasons: Vec<String>,
    /// Short, verbatim source excerpts, not generated claims.
    pub evidence: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Ransomware,
    DataLeak,
    Theft,
    Attack,
}

struct Finding {
    kind: IncidentKind,
    family: Family,
    evidence: String,
}

/// Return at most `limit` relevant, previously unselected articles, newest first
/// when their scores tie. Empty or insufficient evidence does not force a full list.
pub fn rank_articles(
    articles: &[Article],
    now: DateTime<Utc>,
    window_hours: i64,
    limit: usize,
    excluded_ids: &HashSet<String>,
) -> Vec<RankedArticle> {
    if window_hours <= 0 || limit == 0 {
        return Vec::new();
    }
    let Some(window) = Duration::try_hours(window_hours) else {
        return Vec::new();
    };
    let Some(cutoff) = now.checked_sub_signed(window) else {
        return Vec::new();
    };
    let future_tolerance = now + Duration::minutes(5);
    let mut candidates: Vec<(RankedArticle, Family)> = articles
        .iter()
        .filter(|article| {
            article.published_at >= cutoff
                && article.published_at <= future_tolerance
                && !excluded_ids.contains(&article.id)
        })
        .filter_map(|article| {
            let findings = analyze(article);
            let primary = findings.first()?;
            let classification = primary.kind;
            let family = primary.family;
            let normalized = compact(&format!("{} {}", article.title, article.summary));
            let mut score = match classification {
                IncidentKind::ReportedIncident => 55,
                IncidentKind::ActiveAttack => 48,
                IncidentKind::SuspectedIncident => 28,
            };
            let mut reasons = vec![format!(
                "{}: 기사 문구에 따른 분류이며 독립 사실 확인은 아님",
                classification.label()
            )];
            let mut evidence = Vec::new();
            for finding in &findings {
                if !evidence.contains(&finding.evidence) {
                    evidence.push(finding.evidence.clone());
                }
                if evidence.len() == 3 {
                    break;
                }
            }
            for (terms, weight, reason) in [
                (
                    &["랜섬웨어", "ransomware"][..],
                    18,
                    "랜섬웨어 관련 표현 (+18)",
                ),
                (
                    &[
                        "병원",
                        "의료",
                        "전력",
                        "발전소",
                        "공공기관",
                        "정부기관",
                        "기간시설",
                        "수도시설",
                    ][..],
                    16,
                    "의료·공공·필수 서비스 관련 표현 (+16)",
                ),
                (
                    &["금융", "은행", "거래소", "가상자산", "암호화폐", "결제"][..],
                    10,
                    "금융·자산 관련 표현 (+10)",
                ),
                (
                    &["대규모", "대량", "수백만", "수천만", "수억", "전체고객"][..],
                    12,
                    "큰 피해 규모를 시사하는 표현 (+12)",
                ),
                (
                    &[
                        "국내",
                        "한국",
                        "대한민국",
                        "서울",
                        "부산",
                        "과기정통부",
                        "개인정보보호위원회",
                        "kisa",
                    ][..],
                    8,
                    "국내 관련 표현 (+8)",
                ),
                (
                    &[
                        "서비스중단",
                        "시스템마비",
                        "업무중단",
                        "운영중단",
                        "진료중단",
                    ][..],
                    16,
                    "서비스·업무 중단 관련 표현 (+16)",
                ),
                (
                    &["실제악용", "악용중", "악용되고", "공격확산", "공격진행중"][..],
                    10,
                    "진행 중인 공격 관련 표현 (+10)",
                ),
            ] {
                if contains_any(&normalized, terms) {
                    score += weight;
                    reasons.push(reason.to_owned());
                }
            }
            let age_hours = (now - article.published_at).num_hours().max(0);
            let recency = match age_hours {
                0..=5 => 12,
                6..=11 => 8,
                12..=23 => 4,
                _ => 0,
            };
            score += recency;
            reasons.push(format!("발행 후 약 {age_hours}시간 (+{recency})"));
            Some((
                RankedArticle {
                    article: article.clone(),
                    score,
                    classification,
                    reasons,
                    evidence,
                },
                family,
            ))
        })
        .collect();

    candidates.sort_by(|(a, _), (b, _)| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.article.published_at.cmp(&a.article.published_at))
            .then_with(|| a.article.id.cmp(&b.article.id))
            .then_with(|| a.article.url.cmp(&b.article.url))
    });
    let mut selected: Vec<(RankedArticle, Family)> = Vec::new();
    for candidate in candidates {
        if let Some((previous, _)) = selected
            .iter_mut()
            .find(|previous| same_incident(previous, &candidate))
        {
            let reason =
                "동일 기사 또는 같은 사건으로 추정되는 후속 기사 중 우선순위가 높은 기사 선정";
            if !previous.reasons.iter().any(|existing| existing == reason) {
                previous.reasons.push(reason.to_owned());
            }
            continue;
        }
        if selected.len() < limit {
            selected.push(candidate);
        }
    }
    selected.into_iter().map(|(article, _)| article).collect()
}

fn analyze(article: &Article) -> Vec<Finding> {
    let title = compact(&article.title);
    if has_nonincident_genre(&title) {
        return Vec::new();
    }
    let completed_event = contains_any(
        &title,
        &[
            "유출됐",
            "유출됐다",
            "유출확인",
            "해킹당",
            "공격받",
            "공격을받",
            "마비됐",
            "피해입",
            "피해를입",
        ],
    );
    // A simulation or a product announcement can describe serious incidents as
    // examples. Its headline context applies to the accompanying RSS summary.
    let simulation = contains_any(
        &title,
        &[
            "모의훈련",
            "모의공격",
            "모의해킹",
            "가상훈련",
            "가상시나리오",
        ],
    );
    let product_announcement = contains_any(
        &title,
        &["솔루션", "신제품", "보안제품", "보안서비스", "보안플랫폼"],
    ) && contains_any(
        &title,
        &["공개", "발표", "출시", "판매", "선보", "소개", "시연"],
    );
    if !completed_event && (simulation || product_announcement) {
        return Vec::new();
    }
    if !completed_event
        && contains_any(
            &title,
            &[
                "출시",
                "선보",
                "출간",
                "세미나",
                "웨비나",
                "예방교육",
                "교육실시",
                "모집",
                "예방캠페인",
                "인식제고캠페인",
                "예방수칙",
                "대응법",
                "방지법",
                "대응가이드",
                "예방가이드",
                "방어가이드",
                "대응전략",
                "예방법",
                "솔루션공급",
                "협약",
                "인증획득",
            ],
        )
    {
        return Vec::new();
    }

    let headline_findings = sentence_findings(&article.title);
    // A denial in the headline must not turn into a positive finding merely
    // because the body repeats the original allegation without its attribution.
    if has_denial(&title) && headline_findings.is_empty() {
        return Vec::new();
    }
    let uncertain_headline = has_uncertainty(&title);
    let mut findings = headline_findings;
    for sentence in article.summary.split(['.', '!', '?', '\n', '。']) {
        findings.extend(sentence_findings(sentence));
    }
    if uncertain_headline {
        for finding in &mut findings {
            finding.kind = IncidentKind::SuspectedIncident;
        }
    }
    findings.sort_by_key(|finding| match finding.kind {
        IncidentKind::ReportedIncident => 0,
        IncidentKind::ActiveAttack => 1,
        IncidentKind::SuspectedIncident => 2,
    });
    findings
}

fn sentence_findings(sentence: &str) -> Vec<Finding> {
    let text = compact(sentence);
    if text.is_empty()
        || contains_any(
            &text,
            &[
                "사실무근",
                "오보",
                "모의훈련",
                "모의공격",
                "모의해킹",
                "가상시나리오",
            ],
        )
    {
        return Vec::new();
    }
    // Nouns such as "피해" and "사고" alone do not establish occurrence:
    // vulnerability advisories routinely use them to describe possible harm.
    let observed_context = contains_any(
        &text,
        &[
            "유출됐",
            "유출된사실",
            "유출확인",
            "유출을확인",
            "사고확인",
            "사고발생확인",
            "사고발생을확인",
            "발생했다",
            "발생했",
            "발생한",
            "발생사실",
            "발생확인",
            "발생을확인",
            "피해확인",
            "피해를확인",
            "당했",
            "받았",
            "포착",
            "실제악용",
            "악용중",
            "악용되고",
        ],
    );
    let alleged_context = contains_any(
        &text,
        &[
            "정황",
            "의혹",
            "의심",
            "조사중",
            "확인중",
            "유출주장",
            "해킹주장",
            "공격주장",
            "피해주장",
            "판매",
            "게시",
        ],
    );
    let hypothetical_result = contains_any(
        &text,
        &[
            "악용하면",
            "악용할경우",
            "발생할경우",
            "공격할경우",
            "이어질수있",
            "초래할수있",
            "유출될수있",
            "탈취할수있",
            "유출할수있",
            "발생할수있",
        ],
    );
    if hypothetical_result && !observed_context {
        return Vec::new();
    }
    if contains_any(
        &text,
        &[
            "가능",
            "위험",
            "우려",
            "할수있",
            "될수있",
            "발생할",
            "유출될",
            "악용될",
        ],
    ) && !observed_context
        && !alleged_context
    {
        return Vec::new();
    }

    let uncertain = has_uncertainty(&text);
    let mut events = Vec::new();
    if contains_any(
        &text,
        &[
            "랜섬웨어공격",
            "랜섬웨어피해",
            "랜섬웨어감염",
            "랜섬웨어사고",
        ],
    ) && !contains_any(
        &text,
        &[
            "랜섬웨어공격은없",
            "랜섬웨어공격없",
            "랜섬웨어피해없",
            "랜섬웨어피해는없",
            "랜섬웨어공격이아니",
            "랜섬웨어감염은없",
        ],
    ) && !negative_event_assertion(&text, &["랜섬웨어공격", "랜섬웨어피해", "랜섬웨어감염"])
    {
        events.push((Family::Ransomware, IncidentKind::ReportedIncident));
    }
    let leak_signal = contains_any(
        &text,
        &[
            "정보유출",
            "데이터유출",
            "유출사고",
            "유출됐",
            "유출된것",
            "유출확인",
            "정보노출",
            "외부노출",
            "db노출",
            "유출정황",
            "유출의혹",
            "유출설",
        ],
    ) || (text.contains("유출")
        && contains_any(
            &text,
            &[
                "개인정보",
                "고객정보",
                "회원정보",
                "계정정보",
                "환자정보",
                "데이터",
            ],
        ));
    if leak_signal
        && !negative_event_assertion(&text, &["유출", "정보노출", "외부노출", "db노출"])
        && !contains_any(
            &text,
            &[
                "유출은없",
                "유출없",
                "유출이없",
                "유출되지않",
                "유출사실없",
                "유출사실이없",
                "유출의혹을부인",
                "유출의혹부인",
                "유출설부인",
                "노출은없",
                "노출되지않",
                "유출은아니",
                "유출이아니",
                "유출된것은아니",
            ],
        )
        && !contains_any(&text, &["유출예방", "유출방지", "유출대응훈련", "유출대비"])
    {
        events.push((Family::DataLeak, IncidentKind::ReportedIncident));
    }
    if contains_any(
        &text,
        &[
            "자금탈취",
            "자산탈취",
            "코인탈취",
            "탈취당",
            "탈취됐",
            "탈취사고",
            "불법이체",
            "무단결제",
        ],
    ) && !contains_any(
        &text,
        &[
            "탈취없",
            "탈취는없",
            "탈취되지않",
            "불법이체없",
            "무단결제없",
        ],
    ) && !negative_event_assertion(&text, &["탈취", "불법이체", "무단결제"])
    {
        events.push((Family::Theft, IncidentKind::ReportedIncident));
    }
    if contains_any(
        &text,
        &[
            "해킹피해",
            "해킹당",
            "해킹사고",
            "해킹사건",
            "침해사고발생",
            "침해사고확인",
            "공격을받",
            "공격받",
            "해킹의혹",
            "해킹정황",
            "해킹설",
        ],
    ) && !contains_any(
        &text,
        &[
            "해킹피해없",
            "해킹피해는없",
            "해킹은아니",
            "해킹이아니",
            "해킹사실없",
            "해킹사실이없",
            "해킹의혹부인",
            "해킹의혹을부인",
            "해킹설부인",
            "공격받지않",
            "공격을받지않",
            "침해사고없",
            "침해사고는없",
        ],
    ) && !negative_event_assertion(&text, &["해킹", "해킹피해", "공격", "침해사고"])
    {
        events.push((Family::Attack, IncidentKind::ReportedIncident));
    }
    if contains_any(
        &text,
        &[
            "실제악용",
            "악용중",
            "악용되고",
            "공격포착",
            "공격을포착",
            "공격확산",
            "공격진행중",
            "악성코드유포중",
            "악성코드를유포",
            "악성코드유포확인",
            "피싱공격확인",
            "공격캠페인발견",
            "공격캠페인포착",
        ],
    ) && !contains_any(
        &text,
        &[
            "악용되지않",
            "악용정황없",
            "악용정황은없",
            "공격은없",
            "공격이없",
        ],
    ) && !negative_event_assertion(&text, &["악용", "공격"])
    {
        events.push((Family::Attack, IncidentKind::ActiveAttack));
    }
    events
        .into_iter()
        .map(|(family, kind)| Finding {
            kind: if uncertain {
                IncidentKind::SuspectedIncident
            } else {
                kind
            },
            family,
            evidence: excerpt(sentence.trim()),
        })
        .collect()
}

fn has_uncertainty(text: &str) -> bool {
    // Resolve each uncertainty marker against the closest explicit subject.
    // Unknown attackers or damage counts do not undo a confirmed occurrence.
    // A later occurrence subject (e.g. "공격자가 정보 유출 주장") does override
    // an earlier actor subject, so claims about the event remain suspected.
    const SECONDARY_SCOPES: &[&str] = &[
        "피해규모",
        "유출규모",
        "정확한규모",
        "피해액",
        "피해인원",
        "배후",
        "공격자",
        "공격주체",
        "해킹주체",
        "공격세력",
        "해커신원",
        "범인",
        "소행",
    ];
    const OCCURRENCE_SCOPES: &[&str] = &[
        "유출",
        "노출",
        "침해사고",
        "해킹",
        "공격발생",
        "사고발생",
        "피해발생",
        "탈취",
    ];
    for marker in [
        "의혹",
        "정황",
        "추정",
        "가능성",
        "조사중",
        "확인중",
        "유출설",
        "해킹설",
        "주장",
        "의심",
        "추측",
        "미확인",
        "진위",
    ] {
        for (position, _) in text.match_indices(marker) {
            let prefix = &text[..position];
            let secondary = SECONDARY_SCOPES
                .iter()
                .filter_map(|scope| prefix.rfind(scope))
                .max();
            let occurrence = OCCURRENCE_SCOPES
                .iter()
                .filter_map(|scope| prefix.rfind(scope))
                .max();
            if let Some(subject) = secondary
                && occurrence.is_none_or(|event| subject >= event)
                && prefix[subject..].chars().count() <= 40
            {
                continue;
            }
            return true;
        }
    }
    false
}

fn has_denial(text: &str) -> bool {
    negative_event_assertion(
        text,
        &[
            "유출",
            "정보노출",
            "외부노출",
            "해킹",
            "해킹피해",
            "공격",
            "침해사고",
            "탈취",
            "무단결제",
        ],
    ) || contains_any(
        text,
        &[
            "사실무근",
            "오보",
            "부인",
            "유출은없",
            "유출없",
            "유출이없",
            "유출되지않",
            "유출사실없",
            "유출사실이없",
            "해킹은아니",
            "해킹이아니",
            "해킹사실없",
            "해킹사실이없",
            "침해사고없",
            "공격은없",
            "공격이없",
        ],
    )
}

fn contains_any(text: &str, terms: &[&str]) -> bool {
    terms.iter().any(|term| text.contains(term))
}

/// Recognize a negated predicate attached to the event, including investigation
/// language such as "유출 정황은 발견되지 않았다". This is scoped by subject;
/// a leak denial must not suppress an independently reported ransomware attack.
fn negative_event_assertion(text: &str, subjects: &[&str]) -> bool {
    subjects.iter().any(|subject| {
        text.match_indices(subject).any(|(position, _)| {
            let mut predicate = &text[position + subject.len()..];
            for qualifier in [
                "정황", "사실", "사례", "증거", "흔적", "의혹", "여부", "피해", "발생",
            ] {
                if let Some(rest) = predicate.strip_prefix(qualifier) {
                    predicate = rest;
                    break;
                }
            }
            for particle in ["은", "는", "이", "가", "을", "를"] {
                if let Some(rest) = predicate.strip_prefix(particle) {
                    predicate = rest;
                    break;
                }
            }
            for adverb in ["아직", "전혀", "끝내"] {
                if let Some(rest) = predicate.strip_prefix(adverb) {
                    predicate = rest;
                    break;
                }
            }
            [
                "없",
                "아니",
                "부인",
                "확인되지않",
                "발견되지않",
                "관찰되지않",
                "보고되지않",
                "나타나지않",
                "입증되지않",
                "근거가없",
                "되지않",
                "받지않",
            ]
            .iter()
            .any(|denial| predicate.starts_with(denial))
        })
    })
}

fn has_nonincident_genre(title: &str) -> bool {
    let mut remaining = title;
    while let Some((opening, closing)) = [('[', ']'), ('［', '］'), ('【', '】')]
        .into_iter()
        .find(|(opening, _)| remaining.starts_with(*opening))
    {
        let Some(end) = remaining.find(closing) else {
            break;
        };
        let genre = &remaining[opening.len_utf8()..end];
        if contains_any(
            genre,
            &[
                "기고",
                "칼럼",
                "사설",
                "광고",
                "협찬",
                "보안상식",
                "보안빅데이터",
            ],
        ) {
            return true;
        }
        remaining = &remaining[end + closing.len_utf8()..];
    }
    false
}

fn compact(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn excerpt(text: &str) -> String {
    // Character iteration preserves UTF-8 boundaries for Korean text.
    let mut result: String = text.chars().take(220).collect();
    if text.chars().count() > 220 {
        result.push('…');
    }
    result
}

fn same_incident(left: &(RankedArticle, Family), right: &(RankedArticle, Family)) -> bool {
    let a = &left.0.article;
    let b = &right.0.article;
    if a.id == b.id || a.url == b.url {
        return true;
    }
    let a_title = compact(&a.title);
    let b_title = compact(&b.title);
    if a_title == b_title {
        return true;
    }
    if (a.published_at - b.published_at).abs() > Duration::hours(24) {
        return false;
    }
    // A theft follow-up may describe the same event only as a hacking case.
    // Permit this one cross-family pairing only for an explicit shared target;
    // generic title similarity is insufficient, and leak/ransomware stay separate.
    let generic_attack_and_theft = matches!(
        (left.1, right.1),
        (Family::Attack, Family::Theft) | (Family::Theft, Family::Attack)
    );
    if let (Some(a_target), Some(b_target)) = (headline_target(&a_title), headline_target(&b_title))
        && a_target == b_target
        && (left.1 == right.1 || generic_attack_and_theft)
    {
        return true;
    }
    if left.1 != right.1 {
        return false;
    }
    let a_words = title_words(&a.title);
    let b_words = title_words(&b.title);
    let common = a_words.intersection(&b_words).count();
    let combined = a_words.union(&b_words).count();
    // Four shared content words reduce accidental grouping of generic headlines.
    common >= 4 && common * 100 >= combined * 75
}

fn title_words(title: &str) -> HashSet<String> {
    const STOP_WORDS: &[&str] = &[
        "속보",
        "단독",
        "긴급",
        "보안뉴스",
        "확인",
        "발생",
        "관련",
        "통해",
        "대한",
    ];
    title
        .split(|character: char| !character.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|word| word.chars().count() >= 2 && !STOP_WORDS.contains(&word.as_str()))
        .collect()
}

fn headline_target(title: &str) -> Option<usize> {
    const TARGETS: &[&[&str]] = &[
        &["바이낸스", "binance"],
        &["쿠팡", "coupang"],
        &["sk텔레콤", "skt"],
        &["업비트", "upbit"],
        &["빗썸", "bithumb"],
        &["코인베이스", "coinbase"],
        &["lg유플러스"],
    ];
    // Do not group headlines mentioning multiple organizations. An explicit
    // victim signal must follow the organization within a short title span.
    let mut matches = TARGETS.iter().enumerate().filter_map(|(index, aliases)| {
        let has_target = aliases.iter().any(|alias| {
            title.find(alias).is_some_and(|position| {
                let after = &title[position + alias.len()..];
                let role = after.trim_start_matches(['의', ',', ':', '·', '"', '\'', '’', '”']);
                // Named organizations can be analysts or investigators of someone
                // else's incident. Ambiguous roles disable target-based grouping.
                if [
                    "분석팀",
                    "연구팀",
                    "조사팀",
                    "보안팀",
                    "분석가",
                    "연구원",
                    "보안연구원",
                    "전문가",
                    "연구소",
                    "조사단",
                    "ceo",
                ]
                .iter()
                .any(|label| role.starts_with(label))
                {
                    return false;
                }
                let near: String = after.chars().take(35).collect();
                contains_any(
                    &near,
                    &[
                        "유출",
                        "해킹",
                        "랜섬웨어",
                        "탈취",
                        "공격받",
                        "공격을받",
                        "침해사고",
                    ],
                )
            })
        });
        has_target.then_some(index)
    });
    let first = matches.next()?;
    if matches.next().is_some() {
        None
    } else {
        Some(first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-11-01T09:00:00Z".parse().unwrap()
    }

    fn article(id: &str, title: &str, summary: &str) -> Article {
        Article {
            id: id.to_owned(),
            title: title.to_owned(),
            url: format!("https://www.boannews.com/media/view.asp?idx={id}"),
            summary: summary.to_owned(),
            published_at: now() - Duration::hours(1),
            discovered_at: now(),
        }
    }

    fn rank(articles: &[Article]) -> Vec<RankedArticle> {
        rank_articles(articles, now(), 24, 5, &HashSet::new())
    }

    #[test]
    fn incident_has_verbatim_evidence_and_transparent_score() {
        let input = article(
            "1",
            "국내 병원, 랜섬웨어 공격으로 진료 중단",
            "랜섬웨어 공격을 받아 병원 시스템이 멈췄다.",
        );
        let ranked = rank(std::slice::from_ref(&input));
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].classification, IncidentKind::ReportedIncident);
        assert!(ranked[0].score >= 100);
        assert!(ranked[0].evidence.contains(&input.title));
        assert!(
            ranked[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("독립 사실 확인은 아님"))
        );
    }

    #[test]
    fn vulnerability_without_observed_attack_is_not_an_incident() {
        let articles = [
            article(
                "1",
                "새 취약점 발견, 개인정보 유출 위험",
                "공격자가 취약점을 악용할 수 있다. 보안 패치가 공개됐다.",
            ),
            article(
                "2",
                "제품 취약점으로 원격 공격 가능",
                "개인정보가 유출될 가능성이 있다.",
            ),
        ];
        assert!(rank(&articles).is_empty());
    }

    #[test]
    fn observed_exploitation_is_active_attack() {
        let ranked = rank(&[article(
            "1",
            "제로데이 취약점 실제 악용 중",
            "공격 캠페인 포착, 긴급 업데이트 권고",
        )]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].classification, IncidentKind::ActiveAttack);
    }

    #[test]
    fn products_columns_and_training_are_excluded() {
        let articles = [
            article(
                "1",
                "랜섬웨어 공격 막는 신제품 출시",
                "랜섬웨어 공격과 개인정보 유출 사고에 대응한다.",
            ),
            article(
                "2",
                "[기고] 개인정보 유출 사고의 교훈",
                "국내 해킹 피해가 늘어난다.",
            ),
            article(
                "3",
                "기관, 개인정보 유출 예방 교육 실시",
                "해킹 사고 대응 모의 훈련을 실시했다.",
            ),
            article(
                "4",
                "랜섬웨어 공격 대응 가이드",
                "랜섬웨어 공격과 개인정보 유출에 대비해야 한다.",
            ),
            article(
                "5",
                "[배종찬의 보안 빅데이터] 해킹 피해가 주는 교훈",
                "개인정보 유출 사고에 대한 인식을 높여야 한다.",
            ),
        ];
        assert!(rank(&articles).is_empty());
    }

    #[test]
    fn suspected_real_exposure_remains_suspected() {
        let ranked = rank(&[article(
            "1",
            "A사 고객정보 유출 가능성 조사 중",
            "데이터베이스 외부 노출이 확인됐다. 유출 규모는 조사 중이다.",
        )]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].classification, IncidentKind::SuspectedIncident);
    }

    #[test]
    fn organization_prefixed_genres_exclude_historical_incident_discussion() {
        for title in [
            "[가상 정보보호학회 칼럼] 미래 금융보안의 방향",
            "[가상 연구원 기고] 침해사고가 주는 교훈",
            "[기획][가상 신문 사설] 개인정보를 보호하자",
            "【가상 기업 광고】 새 보안 서비스",
        ] {
            let articles = [article(
                "1",
                title,
                "과거 랜섬웨어 공격으로 국내 병원의 진료가 중단됐다. 개인정보 유출 사고도 있었다.",
            )];
            assert!(rank(&articles).is_empty(), "{title}");
        }
        assert_eq!(
            rank(&[article("1", "[단독] 가상 기업 개인정보 유출 확인", "")]).len(),
            1,
        );
    }

    #[test]
    fn hacking_case_wording_is_an_incident_report() {
        let ranked = rank(&[article(
            "1",
            "가상 거래소 해킹 사건 공식 수사 돌입",
            "거래소가 보유한 디지털 자산이 외부 지갑으로 이동했다.",
        )]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].classification, IncidentKind::ReportedIncident);
    }

    #[test]
    fn malicious_campaign_is_not_mistaken_for_awareness_campaign() {
        let ranked = rank(&[article(
            "1",
            "새 피싱 공격 캠페인 포착",
            "공격자가 악성코드를 유포하고 있다.",
        )]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].classification, IncidentKind::ActiveAttack);
    }

    #[test]
    fn damage_scope_investigation_does_not_undo_confirmed_occurrence() {
        for title in [
            "개인정보 유출 확인, 피해 규모 조사 중",
            "개인정보 유출 확인, 유출 규모는 조사 중",
            "개인정보 유출 확인, 피해 인원 확인 중",
        ] {
            let ranked = rank(&[article("1", title, "개인정보 유출 사고가 확인됐다.")]);
            assert_eq!(ranked.len(), 1, "{title}");
            assert_eq!(
                ranked[0].classification,
                IncidentKind::ReportedIncident,
                "{title}"
            );
        }
    }

    #[test]
    fn denials_do_not_resurrect_body_allegations() {
        let articles = [
            article(
                "1",
                "A사, 개인정보 유출 의혹 부인",
                "개인정보 유출 의혹이 제기됐다. 회사는 사실이 아니라고 밝혔다.",
            ),
            article(
                "2",
                "B사 조사 결과 개인정보 유출은 없었다",
                "B사 고객정보 유출 주장을 조사했다.",
            ),
            article(
                "3",
                "해킹 사실이 없다, 서비스 오류로 확인",
                "해킹 피해 의혹은 사실무근이다.",
            ),
        ];
        assert!(rank(&articles).is_empty());
    }

    #[test]
    fn leak_denial_does_not_hide_a_reported_ransomware_attack() {
        let ranked = rank(&[article(
            "1",
            "병원 랜섬웨어 공격 받았지만 개인정보 유출은 없다",
            "병원은 진료를 중단했다.",
        )]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].classification, IncidentKind::ReportedIncident);
    }

    #[test]
    fn duplicate_ids_and_followups_do_not_fill_top_five() {
        let articles = [
            article(
                "1",
                "바이낸스 고객정보 유출 사고 확인",
                "고객정보 유출 사고가 발생했다.",
            ),
            article(
                "1",
                "바이낸스 고객정보 유출 사고 확인",
                "고객정보 유출 사고가 발생했다.",
            ),
            article(
                "2",
                "바이낸스 해킹 후 고객정보 유출, 피해 규모 조사",
                "고객정보 유출 사고 후속 보도다.",
            ),
            article(
                "3",
                "국내 병원 랜섬웨어 공격, 진료 중단",
                "랜섬웨어 공격을 받았다.",
            ),
        ];
        let ranked = rank(&articles);
        assert_eq!(ranked.len(), 2);
        assert!(ranked.iter().any(|item| {
            item.reasons
                .iter()
                .any(|reason| reason.contains("후속 기사"))
        }));
    }

    #[test]
    fn same_company_different_incident_families_are_preserved() {
        let articles = [
            article("1", "바이낸스 고객정보 유출 확인", "고객정보 유출 사고"),
            article(
                "2",
                "바이낸스 자금 탈취 사고 확인",
                "암호화폐 자금 탈취 사고",
            ),
            article("3", "바이낸스 랜섬웨어 공격 피해 확인", ""),
        ];
        assert_eq!(rank(&articles).len(), 3);
    }

    #[test]
    fn same_exchange_hacking_investigation_and_theft_followups_are_grouped() {
        let articles = [
            article("1", "바이낸스 해킹 사건, 합동 조사 착수", ""),
            article("2", "바이낸스 가상자산 탈취 사고, 피해 신고 접수", ""),
            article("3", "바이낸스 해킹 사건 피해 추산 변경", ""),
        ];
        let ranked = rank(&articles);
        assert_eq!(ranked.len(), 1);
        assert!(
            ranked[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("후속 기사"))
        );
    }

    #[test]
    fn cross_family_grouping_requires_same_explicit_target_within_24_hours() {
        let mut investigation = article("1", "바이낸스 해킹 사건, 합동 조사 착수", "");
        let mut theft = article("2", "바이낸스 가상자산 탈취 사고 확인", "");
        investigation.published_at = now();
        theft.published_at = now() - Duration::hours(24);
        assert_eq!(rank(&[investigation.clone(), theft.clone()]).len(), 1);

        theft.published_at -= Duration::seconds(1);
        assert_eq!(
            rank_articles(
                &[investigation.clone(), theft],
                now(),
                48,
                5,
                &HashSet::new()
            )
            .len(),
            2,
        );
        let other_target = article("3", "업비트 가상자산 탈취 사고 확인", "");
        assert_eq!(rank(&[investigation, other_target]).len(), 2);

        let anonymous = [
            article("4", "가상 거래소 해킹 사건, 합동 조사 착수", ""),
            article("5", "가상 거래소 가상자산 탈취 사고 확인", ""),
        ];
        assert_eq!(rank(&anonymous).len(), 2);
    }

    #[test]
    fn actual_publication_time_controls_window_and_future_tolerance() {
        let mut old = article("old", "A사 개인정보 유출 사고", "");
        old.published_at = now() - Duration::hours(25);
        let mut future = article("future", "B사 개인정보 유출 사고", "");
        future.published_at = now() + Duration::minutes(6);
        let mut boundary = article("boundary", "C사 개인정보 유출 사고", "");
        boundary.published_at = now() - Duration::hours(24);
        let ranked = rank(&[old, future, boundary]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].article.id, "boundary");
    }

    #[test]
    fn excluded_ids_limits_and_invalid_windows_are_honored() {
        let articles = [
            article("1", "A사 개인정보 유출 사고", ""),
            article("2", "B사 랜섬웨어 공격", ""),
        ];
        let excluded = HashSet::from(["2".to_owned()]);
        let ranked = rank_articles(&articles, now(), 24, 1, &excluded);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].article.id, "1");
        assert!(rank_articles(&articles, now(), 0, 5, &excluded).is_empty());
        assert!(rank_articles(&articles, now(), i64::MAX, 5, &excluded).is_empty());
        assert!(rank_articles(&articles, now(), 24, 0, &excluded).is_empty());
    }

    #[test]
    fn equal_scores_have_stable_order_independent_of_input_order() {
        let a = article("a", "A사 개인정보 유출 사고", "");
        let b = article("b", "B사 개인정보 유출 사고", "");
        let forward: Vec<_> = rank(&[a.clone(), b.clone()])
            .into_iter()
            .map(|item| item.article.id)
            .collect();
        let reverse: Vec<_> = rank(&[b, a])
            .into_iter()
            .map(|item| item.article.id)
            .collect();
        assert_eq!(forward, vec!["a", "b"]);
        assert_eq!(forward, reverse);
    }

    #[test]
    fn excerpts_preserve_korean_utf8_boundaries() {
        let long = format!("개인정보 유출 사고 확인 {}", "가".repeat(300));
        let ranked = rank(&[article("1", &long, "")]);
        assert_eq!(ranked[0].evidence[0].chars().count(), 221);
        assert!(ranked[0].evidence[0].ends_with('…'));
    }
}
