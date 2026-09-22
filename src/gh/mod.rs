//! Всё, что делает `gh`, спрятано за трейтом `GitHub`.
//!
//! Решение в пользу `gh`, а не octocrab: `gh` уже аутентифицирован на
//! сервере тем же токеном, слово в слово повторяет семантику `--search`,
//! а `gh pr checks --watch` даёт готовое ожидание CI. Трейт оставляет
//! дверь для REST/GraphQL, но переписывать поиск внутри переноса нельзя —
//! это отдельный риск.

pub mod cli;
pub mod dry;
pub mod model;

use anyhow::Result;
use async_trait::async_trait;

use model::{Comment, Issue, Mergeable, Pr};

#[async_trait]
pub trait GitHub: Send + Sync {
    /// Логин владельца токена: им оркестратор пишет комментарии, и по нему
    /// же отличается инструктаж оркестратора от сигнала агента (**C2**).
    async fn self_login(&self) -> Result<String>;
    async fn repo_name_with_owner(&self) -> Result<String>;

    async fn issues_with_label(&self, label: &str) -> Result<Vec<Issue>>;
    /// `gh issue list --state open --search <query> --limit <n>`
    async fn search_issues(&self, query: &str, limit: u32) -> Result<Vec<Issue>>;
    /// То же, но в другом репозитории и по всем состояниям (**C5**).
    async fn search_issues_in_repo(
        &self,
        repo: &str,
        query: &str,
        limit: u32,
    ) -> Result<Vec<Issue>>;
    async fn issue_labels(&self, num: u64) -> Result<Vec<String>>;
    /// Задача по номеру: `--issue N` для ручной диагностики.
    async fn issue_view(&self, num: u64) -> Result<Issue>;
    async fn issue_body_and_comments(&self, num: u64) -> Result<Issue>;
    async fn issue_comments(&self, num: u64) -> Result<Vec<Comment>>;
    async fn issue_state(&self, repo: Option<&str>, num: u64) -> Result<String>;

    async fn issue_edit_labels(&self, num: u64, add: &[&str], remove: &[&str]) -> Result<()>;
    async fn issue_comment(&self, num: u64, body: &str) -> Result<()>;
    async fn issue_create(
        &self,
        repo: &str,
        labels: &[&str],
        title: &str,
        body: &str,
    ) -> Result<String>;

    async fn pr_list_open(&self, limit: u32) -> Result<Vec<Pr>>;
    /// `gh pr list --state open --search <query> --json url` → первый URL.
    async fn pr_search_open_url(&self, query: &str) -> Result<Option<String>>;
    async fn pr_head_sha(&self, url: &str) -> Result<String>;
    /// Имя ветки PR: в режиме `github-app` её выбирает не оркестратор,
    /// а сам `@claude`, а по ней ищется упавший CI-run.
    async fn pr_head_branch(&self, url: &str) -> Result<String>;
    /// Один опрос; три попытки делает [`pr_mergeable`] (**C7**).
    async fn pr_mergeable_once(&self, url: &str) -> Result<Mergeable>;
    async fn pr_merge_state(&self, url: &str) -> Result<String>;
    async fn pr_comments(&self, url: &str) -> Result<Vec<Comment>>;
    async fn pr_create_draft(
        &self,
        base: &str,
        head: &str,
        title: &str,
        body: &str,
    ) -> Result<String>;
    /// `gh pr checks --watch`: `Ok(true)` — чеки зелёные.
    async fn pr_checks_watch(&self, url: &str) -> Result<bool>;
    async fn pr_comment(&self, url: &str, body: &str) -> Result<()>;
    async fn pr_close(&self, url: &str, comment: &str) -> Result<()>;
    async fn pr_ready(&self, url: &str) -> Result<()>;
    async fn pr_diff(&self, url: &str) -> Result<String>;
    async fn pr_merge_squash_auto(&self, url: &str) -> Result<()>;
    async fn pr_disable_auto_merge(&self, url: &str) -> Result<()>;

    async fn latest_run_id(&self, branch: &str) -> Result<Option<String>>;
    async fn run_log_failed(&self, id: &str) -> Result<String>;
}

/// **C7**: GitHub считает `mergeable` лениво — первый запрос по «остывшему»
/// PR отдаёт `UNKNOWN` и только запускает расчёт. Поэтому переспрашиваем
/// трижды с паузой 3 секунды, как в bash.
pub async fn pr_mergeable(
    gh: &dyn GitHub,
    clock: &dyn crate::clock::Clock,
    url: &str,
) -> Mergeable {
    for attempt in 1..=3 {
        match gh.pr_mergeable_once(url).await {
            Ok(state) if state != Mergeable::Unknown => return state,
            Ok(_) => {}
            Err(e) => tracing::info!("⚠️ Не удалось узнать mergeable ({attempt}/3): {e:#}"),
        }
        if attempt < 3 {
            clock.sleep(std::time::Duration::from_secs(3)).await;
        }
    }
    Mergeable::Unknown
}

/// **C4**: сначала ищем PR по строке `AI-TASK: #N`, затем — фоллбэком по
/// `Closes #N`. Агент обязан ставить обе строки, но ставит не всегда:
/// PR только с `Closes` становился невидимым для цикла и висел навсегда.
pub async fn find_task_pr(gh: &dyn GitHub, issue: u64) -> Result<Option<String>> {
    if let Some(url) = gh
        .pr_search_open_url(&format!("\"AI-TASK: #{issue}\" in:body"))
        .await?
    {
        return Ok(Some(url));
    }
    gh.pr_search_open_url(&format!("\"Closes #{issue}\" in:body"))
        .await
}

/// У задачи стоит метка `blocked`?
pub async fn issue_is_blocked(gh: &dyn GitHub, num: u64, blocked_label: &str) -> Result<bool> {
    Ok(gh
        .issue_labels(num)
        .await?
        .iter()
        .any(|l| l == blocked_label))
}
