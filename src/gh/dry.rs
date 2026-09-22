//! `--dry-run` как декоратор над реальным [`GitHub`], а не `if` по всему коду.
//!
//! Читающие запросы идут как обычно — иначе сверять нечего. Каждое
//! изменяющее действие печатается ровно в том виде, в каком оно пошло бы
//! наружу, и не выполняется. Это основной инструмент сверки с bash-версией.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;

use super::model::{Comment, Issue, Mergeable, Pr};
use super::GitHub;

/// Журнал намеченных действий: нужен и человеку в логе, и тестам.
#[derive(Debug, Default, Clone)]
pub struct DryLog(Arc<Mutex<Vec<String>>>);

impl DryLog {
    pub fn push(&self, action: String) {
        tracing::info!("(dry-run) {action}");
        self.0.lock().expect("журнал dry-run").push(action);
    }

    pub fn actions(&self) -> Vec<String> {
        self.0.lock().expect("журнал dry-run").clone()
    }
}

pub struct DryGitHub {
    inner: Arc<dyn GitHub>,
    log: DryLog,
}

impl DryGitHub {
    pub fn new(inner: Arc<dyn GitHub>) -> Self {
        Self {
            inner,
            log: DryLog::default(),
        }
    }

    pub fn log(&self) -> DryLog {
        self.log.clone()
    }
}

/// Многострочные тела комментариев в логе сжимаем до первой строки:
/// последовательность действий читать важнее, чем текст целиком.
fn head(body: &str) -> String {
    let first = body.lines().next().unwrap_or("").trim();
    let rest = body.lines().count().saturating_sub(1);
    if rest > 0 {
        format!("{first} …(+{rest} строк)")
    } else {
        first.to_string()
    }
}

#[async_trait]
impl GitHub for DryGitHub {
    async fn self_login(&self) -> Result<String> {
        self.inner.self_login().await
    }

    async fn repo_name_with_owner(&self) -> Result<String> {
        self.inner.repo_name_with_owner().await
    }

    async fn issues_with_label(&self, label: &str) -> Result<Vec<Issue>> {
        self.inner.issues_with_label(label).await
    }

    async fn search_issues(&self, query: &str, limit: u32) -> Result<Vec<Issue>> {
        self.inner.search_issues(query, limit).await
    }

    async fn search_issues_in_repo(
        &self,
        repo: &str,
        query: &str,
        limit: u32,
    ) -> Result<Vec<Issue>> {
        self.inner.search_issues_in_repo(repo, query, limit).await
    }

    async fn issue_labels(&self, num: u64) -> Result<Vec<String>> {
        self.inner.issue_labels(num).await
    }

    async fn issue_view(&self, num: u64) -> Result<Issue> {
        self.inner.issue_view(num).await
    }

    async fn issue_body_and_comments(&self, num: u64) -> Result<Issue> {
        self.inner.issue_body_and_comments(num).await
    }

    async fn issue_comments(&self, num: u64) -> Result<Vec<Comment>> {
        self.inner.issue_comments(num).await
    }

    async fn issue_state(&self, repo: Option<&str>, num: u64) -> Result<String> {
        self.inner.issue_state(repo, num).await
    }

    async fn issue_edit_labels(&self, num: u64, add: &[&str], remove: &[&str]) -> Result<()> {
        let mut parts = Vec::new();
        for l in add {
            parts.push(format!("+{l}"));
        }
        for l in remove {
            parts.push(format!("-{l}"));
        }
        self.log
            .push(format!("gh issue edit #{num} {}", parts.join(" ")));
        Ok(())
    }

    async fn issue_comment(&self, num: u64, body: &str) -> Result<()> {
        self.log
            .push(format!("gh issue comment #{num}: {}", head(body)));
        Ok(())
    }

    async fn issue_create(
        &self,
        repo: &str,
        labels: &[&str],
        title: &str,
        _body: &str,
    ) -> Result<String> {
        self.log.push(format!(
            "gh issue create -R {repo} --label {} «{title}»",
            labels.join(",")
        ));
        Ok(format!("https://github.com/{repo}/issues/0"))
    }

    async fn pr_list_open(&self, limit: u32) -> Result<Vec<Pr>> {
        self.inner.pr_list_open(limit).await
    }

    async fn pr_search_open_url(&self, query: &str) -> Result<Option<String>> {
        self.inner.pr_search_open_url(query).await
    }

    async fn pr_head_sha(&self, url: &str) -> Result<String> {
        self.inner.pr_head_sha(url).await
    }

    async fn pr_head_branch(&self, url: &str) -> Result<String> {
        self.inner.pr_head_branch(url).await
    }

    async fn pr_mergeable_once(&self, url: &str) -> Result<Mergeable> {
        self.inner.pr_mergeable_once(url).await
    }

    async fn pr_merge_state(&self, url: &str) -> Result<String> {
        self.inner.pr_merge_state(url).await
    }

    async fn pr_comments(&self, url: &str) -> Result<Vec<Comment>> {
        self.inner.pr_comments(url).await
    }

    async fn pr_create_draft(
        &self,
        base: &str,
        head_branch: &str,
        title: &str,
        _body: &str,
    ) -> Result<String> {
        self.log.push(format!(
            "gh pr create --draft --base {base} --head {head_branch} «{title}»"
        ));
        Ok("https://github.com/dry-run/pull/0".to_string())
    }

    async fn pr_checks_watch(&self, url: &str) -> Result<bool> {
        // Ждать CI по-настоящему в сухом прогоне нечего: коммитов мы не
        // делали, поэтому считаем чеки зелёными и идём дальше — так видна
        // вся последовательность действий до конца итерации.
        self.log
            .push(format!("gh pr checks {url} --watch → считаю зелёным"));
        Ok(true)
    }

    async fn pr_comment(&self, url: &str, body: &str) -> Result<()> {
        self.log
            .push(format!("gh pr comment {url}: {}", head(body)));
        Ok(())
    }

    async fn pr_close(&self, url: &str, comment: &str) -> Result<()> {
        self.log
            .push(format!("gh pr close {url}: {}", head(comment)));
        Ok(())
    }

    async fn pr_ready(&self, url: &str) -> Result<()> {
        self.log.push(format!("gh pr ready {url}"));
        Ok(())
    }

    async fn pr_diff(&self, url: &str) -> Result<String> {
        self.inner.pr_diff(url).await
    }

    async fn pr_merge_squash_auto(&self, url: &str) -> Result<()> {
        self.log.push(format!("gh pr merge {url} --squash --auto"));
        Ok(())
    }

    async fn pr_disable_auto_merge(&self, url: &str) -> Result<()> {
        self.log.push(format!("gh pr merge {url} --disable-auto"));
        Ok(())
    }

    async fn latest_run_id(&self, branch: &str) -> Result<Option<String>> {
        self.inner.latest_run_id(branch).await
    }

    async fn run_log_failed(&self, id: &str) -> Result<String> {
        self.inner.run_log_failed(id).await
    }
}
