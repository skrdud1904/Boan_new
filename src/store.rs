use std::{collections::HashSet, path::Path};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::models::Article;

pub struct Store {
    connection: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path).context("Cannot open news database")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS articles (
                 id TEXT PRIMARY KEY NOT NULL,
                 payload TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS deliveries (
                 article_id TEXT PRIMARY KEY NOT NULL REFERENCES articles(id),
                 sent_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS metadata (
                 key TEXT PRIMARY KEY NOT NULL,
                 value TEXT NOT NULL
             );",
        )?;
        Ok(Self { connection })
    }

    /// Update an existing report while preserving its first discovery timestamp.
    pub fn upsert(&mut self, articles: &[Article]) -> Result<usize> {
        let transaction = self.connection.transaction()?;
        let mut new_count = 0;
        for article in articles {
            let previous: Option<String> = transaction
                .query_row(
                    "SELECT payload FROM articles WHERE id = ?1",
                    [&article.id],
                    |row| row.get(0),
                )
                .optional()?;
            let mut updated = article.clone();
            if let Some(payload) = previous {
                let original: Article = serde_json::from_str(&payload)?;
                updated.discovered_at = original.discovered_at;
            } else {
                new_count += 1;
            }
            transaction.execute(
                "INSERT INTO articles(id, payload) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET payload = excluded.payload",
                params![updated.id, serde_json::to_string(&updated)?],
            )?;
        }
        transaction.commit()?;
        Ok(new_count)
    }

    pub fn articles(&self) -> Result<Vec<Article>> {
        let mut statement = self
            .connection
            .prepare("SELECT payload FROM articles ORDER BY id")?;
        let payloads = statement.query_map([], |row| row.get::<_, String>(0))?;
        payloads
            .map(|payload| Ok(serde_json::from_str(&payload?)?))
            .collect()
    }

    pub fn sent_ids(&self) -> Result<HashSet<String>> {
        let mut statement = self
            .connection
            .prepare("SELECT article_id FROM deliveries")?;
        Ok(statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?)
    }

    pub fn mark_sent(&mut self, ids: &[String], now: DateTime<Utc>) -> Result<()> {
        self.complete_delivery(ids, now, None)
    }

    /// Commit the receipt and scheduled-day marker together so restart/recovery
    /// cannot send the next batch for an already completed daily briefing.
    pub fn complete_delivery(
        &mut self,
        ids: &[String],
        now: DateTime<Utc>,
        scheduled_date: Option<&str>,
    ) -> Result<()> {
        let transaction = self.connection.transaction()?;
        for id in ids {
            transaction.execute(
                "INSERT INTO deliveries(article_id, sent_at) VALUES (?1, ?2)
                 ON CONFLICT(article_id) DO NOTHING",
                params![id, now.to_rfc3339()],
            )?;
        }
        if let Some(date) = scheduled_date {
            transaction.execute(
                "INSERT INTO metadata(key, value) VALUES ('last_sent_daily_date', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [date],
            )?;
        }
        transaction.execute("DELETE FROM metadata WHERE key = 'pending_delivery'", [])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn clear_pending_delivery(&self) -> Result<()> {
        self.connection
            .execute("DELETE FROM metadata WHERE key = 'pending_delivery'", [])?;
        Ok(())
    }

    pub fn get_metadata(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row("SELECT value FROM metadata WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn set_metadata(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO metadata(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn example() -> Article {
        Article {
            id: "42".into(),
            title: "예시 기업 개인정보 유출 확인".into(),
            url: "https://www.boannews.com/news/articleView.html?idxno=42".into(),
            summary: "테스트용 가상 사건".into(),
            published_at: Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap(),
            discovered_at: Utc.with_ymd_and_hms(2026, 11, 1, 1, 0, 0).unwrap(),
        }
    }

    #[test]
    fn duplicate_collection_updates_without_resetting_first_seen() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let original = example();
        assert_eq!(store.upsert(std::slice::from_ref(&original)).unwrap(), 1);
        let mut correction = original.clone();
        correction.title = "정정: 피해 범위 재확인".into();
        correction.discovered_at += chrono::Duration::hours(1);
        assert_eq!(store.upsert(&[correction.clone()]).unwrap(), 0);
        let stored = store.articles().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].title, correction.title);
        assert_eq!(stored[0].discovered_at, original.discovered_at);
    }

    #[test]
    fn delivery_tracking_is_atomic_and_survives_reopening() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.sqlite3");
        {
            let mut store = Store::open(&path).unwrap();
            let article = example();
            store.upsert(std::slice::from_ref(&article)).unwrap();
            assert!(store.sent_ids().unwrap().is_empty());
            store.set_metadata("pending_delivery", "pending").unwrap();
            assert!(
                store
                    .mark_sent(&["42".into(), "missing".into()], Utc::now())
                    .is_err()
            );
            assert!(store.sent_ids().unwrap().is_empty());
            assert!(store.get_metadata("pending_delivery").unwrap().is_some());
            store.mark_sent(&["42".into()], Utc::now()).unwrap();
            assert!(store.get_metadata("pending_delivery").unwrap().is_none());
            store.set_metadata("daily", "2026-11-01").unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert!(store.sent_ids().unwrap().contains("42"));
        assert_eq!(
            store.get_metadata("daily").unwrap().as_deref(),
            Some("2026-11-01")
        );
    }

    #[test]
    fn scheduled_day_and_receipt_commit_or_roll_back_together() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store.upsert(&[example()]).unwrap();
        store.set_metadata("pending_delivery", "pending").unwrap();
        assert!(
            store
                .complete_delivery(
                    &["42".into(), "missing".into()],
                    Utc::now(),
                    Some("2026-11-01")
                )
                .is_err()
        );
        assert!(
            store
                .get_metadata("last_sent_daily_date")
                .unwrap()
                .is_none()
        );
        assert!(store.get_metadata("pending_delivery").unwrap().is_some());
        assert!(store.sent_ids().unwrap().is_empty());
        store
            .complete_delivery(&["42".into()], Utc::now(), Some("2026-11-01"))
            .unwrap();
        assert_eq!(
            store
                .get_metadata("last_sent_daily_date")
                .unwrap()
                .as_deref(),
            Some("2026-11-01")
        );
        assert!(store.get_metadata("pending_delivery").unwrap().is_none());
        assert!(store.sent_ids().unwrap().contains("42"));
    }
}
