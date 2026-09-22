//! Единственное определение «очереди» (**C1**).
//!
//! `gh issue list --label ai-task` даёт совсем другое число: на 2026-09-14
//! это было 27 открытых задач против 7 фактически в очереди. Диагностика
//! «очередь пуста» без author-фильтра неверна, поэтому запрос живёт в одном
//! месте и им пользуются и оркестратор, и смотритель бэклога.

use anyhow::Result;

use crate::config::Config;
use crate::gh::model::Issue;
use crate::gh::GitHub;

/// Метки: задача в очереди, не у человека, не заблокирована.
pub fn label_filter(cfg: &Config) -> String {
    format!(
        "label:{} -label:{} -label:{}",
        cfg.task_label, cfg.human_label, cfg.blocked_label
    )
}

/// Доверенные авторы. Пусто в конфиге = владелец gh-токена: защита от
/// prompt injection. Межрепозиторные задачи агент создаёт от того же
/// аккаунта, поэтому они тоже проходят.
pub fn authors(cfg: &Config, self_login: &str) -> Vec<String> {
    if cfg.allowed_authors.is_empty() {
        vec![self_login.to_string()]
    } else {
        cfg.allowed_authors.clone()
    }
}

/// Запрос очереди целиком — тот, что уходит в `--search`.
pub fn search_query(cfg: &Config, author: &str, sort_asc: bool) -> String {
    let mut q = format!("{} author:{}", label_filter(cfg), author);
    if sort_asc {
        q.push_str(" sort:created-asc");
    }
    q
}

/// Очередь: объединение по доверенным авторам, отсортированное от старых
/// к новым.
///
/// GitHub трактует несколько `author:` в одном запросе как И, а не ИЛИ,
/// поэтому при двух и более доверенных авторах общий поиск всегда
/// возвращает пусто — это глушило очередь целиком. Спрашиваем по автору
/// отдельно и объединяем.
pub async fn fetch(
    gh: &dyn GitHub,
    cfg: &Config,
    authors: &[String],
    per_author_limit: u32,
) -> Result<Vec<Issue>> {
    let mut all: Vec<Issue> = Vec::new();
    for author in authors {
        let part = gh
            .search_issues(&search_query(cfg, author, true), per_author_limit)
            .await?;
        for issue in part {
            if !all.iter().any(|i| i.number == issue.number) {
                all.push(issue);
            }
        }
    }
    all.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.number.cmp(&b.number))
    });
    Ok(all)
}

/// Голова очереди — задача, которую берём в работу.
pub async fn head(gh: &dyn GitHub, cfg: &Config, authors: &[String]) -> Result<Option<Issue>> {
    Ok(fetch(gh, cfg, authors, 1).await?.into_iter().next())
}

/// Размер очереди. Нужен смотрителю бэклога: он решает по нему, заводить
/// ли новые задачи.
pub async fn size(gh: &dyn GitHub, cfg: &Config, authors: &[String]) -> Result<usize> {
    Ok(fetch(gh, cfg, authors, 200).await?.len())
}
