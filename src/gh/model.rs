//! Модель данных GitHub в том виде, в каком их отдаёт `gh --json`.

use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Author {
    #[serde(default)]
    pub login: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    #[serde(default)]
    pub author: Author,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

impl Comment {
    /// Фикстуры тестов и удобство: комментарий за один вызов.
    pub fn new(login: &str, body: &str, created_at: DateTime<Utc>) -> Self {
        Self {
            author: Author {
                login: login.to_string(),
            },
            body: body.to_string(),
            created_at: Some(created_at),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Label {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<Label>>,
    #[serde(default)]
    pub comments: Option<Vec<Comment>>,
}

impl Issue {
    pub fn body_str(&self) -> &str {
        self.body.as_deref().unwrap_or("")
    }

    pub fn label_names(&self) -> Vec<String> {
        self.labels
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|l| l.name.clone())
            .collect()
    }
}

/// `mergeable` у PR. GitHub считает его лениво (**C7**).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    Unknown,
}

impl Mergeable {
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "MERGEABLE" => Mergeable::Mergeable,
            "CONFLICTING" => Mergeable::Conflicting,
            _ => Mergeable::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mergeable::Mergeable => "MERGEABLE",
            Mergeable::Conflicting => "CONFLICTING",
            Mergeable::Unknown => "UNKNOWN",
        }
    }
}

/// `mergeStateStatus`: конфликт здесь называется `DIRTY`.
pub fn merge_state_is_conflict(status: &str) -> bool {
    matches!(status.trim(), "DIRTY" | "CONFLICTING")
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pr {
    #[serde(default)]
    pub number: u64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub mergeable: Option<String>,
    #[serde(default)]
    pub merge_state_status: Option<String>,
    #[serde(default)]
    pub auto_merge_request: Option<serde_json::Value>,
    #[serde(default)]
    pub head_ref_name: Option<String>,
    #[serde(default)]
    pub head_ref_oid: Option<String>,
}

impl Pr {
    pub fn body_str(&self) -> &str {
        self.body.as_deref().unwrap_or("")
    }

    pub fn mergeable_state(&self) -> Mergeable {
        Mergeable::parse(self.mergeable.as_deref().unwrap_or("UNKNOWN"))
    }

    /// Авто-merge уже поставлен в очередь?
    pub fn auto_merge_queued(&self) -> bool {
        match &self.auto_merge_request {
            None => false,
            Some(serde_json::Value::Null) => false,
            Some(_) => true,
        }
    }
}
