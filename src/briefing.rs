//! Source excerpts and explicitly labelled, rule-based interpretation.
//! This module does not translate, call an LLM, or infer unreported impact.
use serde::Serialize;

use crate::ranking::{IncidentKind, RankedArticle};

pub const SECTION_NAMES: [&str; 6] = [
    "핵심 변화 · 원문(RSS)",
    "왜 중요한가 · 해석",
    "적용 조건·한계 · 수집 범위",
    "확인할 사항 · 제안",
    "원문 제목",
    "태그 · 규칙 기반",
];
pub const FOOTER: &str = "RSS 제목·요약 기준 · 규칙 기반 해석/제안 포함 · 기사 전문 미확인";

#[derive(Debug, Serialize)]
pub struct ArticleBriefing {
    pub source: &'static str,
    pub title: String,
    pub key_change: String,
    pub interpretation: &'static str,
    pub scope: &'static str,
    pub checks: [&'static str; 3],
    pub tags: Vec<&'static str>,
}

impl ArticleBriefing {
    pub fn sections(&self) -> [String; 6] {
        [
            self.key_change.clone(),
            self.interpretation.to_owned(),
            self.scope.to_owned(),
            self.checks
                .iter()
                .map(|check| format!("• 제안: {check}"))
                .collect::<Vec<_>>()
                .join("\n"),
            self.title.clone(),
            self.tags.join(" / "),
        ]
    }
}

pub fn from_article(ranked: &RankedArticle) -> ArticleBriefing {
    let (interpretation, first_check) = match ranked.classification {
        IncidentKind::ReportedIncident => (
            "해석: 사고 발생 보도로 분류했습니다. 자사와 관련된 자산·업무인지 확인할 가치가 있지만, 이 기사만으로 자사의 침해 여부를 판단할 수는 없습니다.",
            "피해 기관·제품과 자사 자산의 관련성을 확인한다.",
        ),
        IncidentKind::ActiveAttack => (
            "해석: 실제 공격 활동 보도로 분류했습니다. 관측된 공격 조건과 자사 환경을 비교할 필요가 있으며, 모든 조직이 같은 영향을 받는다는 뜻은 아닙니다.",
            "공격 대상·조건이 자사 환경과 일치하는지 확인한다.",
        ),
        IncidentKind::SuspectedIncident => (
            "해석: 의혹·조사 단계로 분류했습니다. 대응 판단에 앞서 공식 확인과 후속 보도를 확인해야 하며, 유출이나 침해가 확정됐다고 단정하지 않습니다.",
            "피해 주장에 대한 공식 확인과 후속 정정 여부를 확인한다.",
        ),
    };
    let text = format!("{} {}", ranked.article.title, ranked.article.summary).to_lowercase();
    let mut tags = vec![ranked.classification.label()];
    for (keywords, tag) in [
        (&["랜섬웨어", "ransomware"][..], "랜섬웨어"),
        (&["유출", "노출"][..], "정보 유출·노출"),
        (&["피싱", "사회공학", "사칭"][..], "사회공학"),
        (&["공급망"][..], "공급망"),
        (&["악성코드", "멀웨어"][..], "악성코드"),
        (&["암호화폐", "가상자산", "거래소"][..], "가상자산"),
    ] {
        if keywords.iter().any(|keyword| text.contains(keyword)) {
            tags.push(tag);
        }
        if tags.len() == 4 {
            break;
        }
    }
    ArticleBriefing {
        source: "보안뉴스",
        title: ranked.article.title.clone(),
        key_change: if ranked.article.summary.trim().is_empty() {
            format!(
                "RSS 요약이 없어 제목만 표시합니다: {}",
                ranked.article.title
            )
        } else {
            ranked.article.summary.clone()
        },
        interpretation,
        scope: "RSS의 제목·요약만 수집했습니다. 전체 본문과 독자의 환경별 영향은 검증하지 않았습니다. 대상·제품 버전·피해 범위는 원문 및 공식 공지에서 확인해야 합니다.",
        checks: [
            first_check,
            "원문·공식 권고의 적용 범위와 탐지 근거를 확인한다.",
            "기사만으로 노출을 단정하지 말고 내부 기록과 대조한다.",
        ],
        tags,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpts_are_not_rewritten_as_verified_facts() {
        let now = "2026-11-01T09:00:00Z".parse().unwrap();
        let mut ranked = RankedArticle {
            article: crate::models::Article {
                id: "1".into(),
                title: "예시기관 유출 의혹 조사 중".into(),
                url: "https://www.boannews.com/news/articleView.html?idxno=1".into(),
                summary: "유출 주장은 아직 확인되지 않았다.".into(),
                published_at: now,
                discovered_at: now,
            },
            score: 28,
            classification: IncidentKind::SuspectedIncident,
            reasons: vec![],
            evidence: vec![],
        };
        let briefing = from_article(&ranked);
        assert_eq!(briefing.key_change, ranked.article.summary);
        assert!(briefing.interpretation.contains("단정하지 않습니다"));
        assert!(briefing.scope.contains("검증하지 않았습니다"));
        assert_eq!(briefing.checks.len(), 3);
        assert!(!FOOTER.contains("자동 번역"));
        ranked.article.summary.clear();
        assert!(from_article(&ranked).key_change.contains("제목만 표시"));
    }
}
