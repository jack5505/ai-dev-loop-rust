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
    /// `gh issue view --json body,comments` не отдаёт номер вовсе, поэтому
    /// поле обязано иметь значение по умолчанию: иначе разбор падает и
    /// проход разблокировки (C6) молча не видит ни одной задачи.
    #[serde(default)]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Регрессия: `gh issue view N --json body,comments` не возвращает номер.
    /// Пока поле было обязательным, разбор падал на каждой задаче, и проход
    /// разблокировки (C6) не видел ни одной — ошибка нашлась сухим прогоном
    /// на боевых данных.
    #[test]
    fn issue_view_without_number_parses() {
        let raw = r#"{"body":"тело задачи","comments":[
            {"author":{"login":"claude"},"body":"BLOCKED-BY: jack5505/mahalla#247",
             "createdAt":"2026-09-19T10:00:00Z"}]}"#;
        let issue: Issue = serde_json::from_str(raw).expect("разбор ответа gh");
        assert_eq!(issue.number, 0);
        assert_eq!(issue.body_str(), "тело задачи");
        let comments = issue.comments.expect("комментарии");
        assert_eq!(comments[0].author.login, "claude");
        assert!(comments[0].created_at.is_some());
    }

    #[test]
    fn pr_row_from_list_parses() {
        let raw = r#"[{"number":327,"url":"https://github.com/a/b/pull/327",
            "body":"AI-TASK: #300\nCloses #300","updatedAt":"2026-09-18T12:00:00Z",
            "mergeable":"CONFLICTING","mergeStateStatus":"DIRTY",
            "autoMergeRequest":null,"headRefName":"ai/issue-300"}]"#;
        let prs: Vec<Pr> = serde_json::from_str(raw).expect("разбор списка PR");
        let pr = &prs[0];
        assert_eq!(pr.mergeable_state(), Mergeable::Conflicting);
        assert!(!pr.auto_merge_queued());
        assert!(merge_state_is_conflict(
            pr.merge_state_status.as_deref().unwrap()
        ));
        assert_eq!(pr.head_ref_name.as_deref(), Some("ai/issue-300"));
    }

    #[test]
    fn queued_auto_merge_is_detected() {
        let raw = r#"{"autoMergeRequest":{"enabledAt":"2026-09-20T10:00:00Z"}}"#;
        let pr: Pr = serde_json::from_str(raw).expect("разбор PR");
        assert!(pr.auto_merge_queued());
    }
}
