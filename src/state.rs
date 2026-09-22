//! Машиночитаемое состояние итерации: `.ai-logs/state-<issue>.json`.
//!
//! Отсюда `ai-dev status` и отказ от повторного вызова `@claude` — bash
//! про свои итерации не помнил ничего.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Стадия итерации — в терминах §2 спецификации.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    Taken,
    Implementing,
    WaitingApp,
    WaitingCi,
    Reviewing,
    Finished,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Taken => "taken",
            Stage::Implementing => "implementing",
            Stage::WaitingApp => "waiting-app",
            Stage::WaitingCi => "waiting-ci",
            Stage::Reviewing => "reviewing",
            Stage::Finished => "finished",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterationState {
    pub issue: u64,
    pub stage: Stage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl IterationState {
    pub fn new(issue: u64, now: DateTime<Utc>) -> Self {
        Self {
            issue,
            stage: Stage::Taken,
            assigned_at: None,
            asked_at: None,
            pr: None,
            outcome: None,
            updated_at: now,
        }
    }

    pub fn path(log_dir: &Path, issue: u64) -> PathBuf {
        log_dir.join(format!("state-{issue}.json"))
    }

    pub fn load(log_dir: &Path, issue: u64) -> Option<Self> {
        let raw = std::fs::read_to_string(Self::path(log_dir, issue)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn save(&self, log_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(log_dir)
            .with_context(|| format!("создание {}", log_dir.display()))?;
        let path = Self::path(log_dir, self.issue);
        let body = serde_json::to_string_pretty(self).context("сериализация состояния")?;
        std::fs::write(&path, body + "\n").with_context(|| format!("запись {}", path.display()))
    }

    /// Все состояния из каталога логов — для `ai-dev status`.
    pub fn all(log_dir: &Path) -> Vec<Self> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(log_dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("state-") && name.ends_with(".json") {
                if let Ok(raw) = std::fs::read_to_string(entry.path()) {
                    if let Ok(state) = serde_json::from_str::<Self>(&raw) {
                        out.push(state);
                    }
                }
            }
        }
        out.sort_by_key(|s| s.issue);
        out
    }
}
