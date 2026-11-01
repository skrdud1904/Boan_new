use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Article {
    pub id: String,
    pub title: String,
    pub url: String,
    pub summary: String,
    pub published_at: DateTime<Utc>,
    pub discovered_at: DateTime<Utc>,
}
