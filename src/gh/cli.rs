//! Реализация трейта [`GitHub`] через подпроцесс `gh`.
//!
//! Все вызовы идут с рабочим каталогом `REPO_DIR`: репозиторий `gh`
//! определяет по клону, как и bash-версия. В клонах-форках это важно —
//! без `gh repo set-default` он резолвит upstream и очередь оказывается
//! «пустой».

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::de::DeserializeOwned;

use super::model::{Comment, Issue, Mergeable, Pr};
use super::GitHub;
use crate::exec::Cmd;

#[derive(Debug, Clone)]
pub struct GhCli {
    repo_dir: PathBuf,
}

impl GhCli {
    pub fn new(repo_dir: impl Into<PathBuf>) -> Self {
        Self {
            repo_dir: repo_dir.into(),
        }
    }

    fn cmd<I, S>(&self, args: I) -> Cmd
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Cmd::new("gh").args(args).cwd(&self.repo_dir)
    }

    async fn json<T: DeserializeOwned, I, S>(&self, args: I) -> Result<T>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let cmd = self.cmd(args);
        let out = cmd.check().await?;
        serde_json::from_str(out.trimmed())
            .with_context(|| format!("разбор JSON из `{}`", cmd.display()))
    }

    pub fn repo_dir(&self) -> &Path {
        &self.repo_dir
    }
}

#[async_trait]
impl GitHub for GhCli {
    async fn self_login(&self) -> Result<String> {
        Ok(self
            .cmd(["api", "user", "--jq", ".login"])
            .check()
            .await?
            .trimmed()
            .to_string())
    }

    async fn repo_name_with_owner(&self) -> Result<String> {
        Ok(self
            .cmd([
                "repo",
                "view",
                "--json",
                "nameWithOwner",
                "--jq",
                ".nameWithOwner",
            ])
            .check()
            .await?
            .trimmed()
            .to_string())
    }

    async fn issues_with_label(&self, label: &str) -> Result<Vec<Issue>> {
        self.json([
            "issue",
            "list",
            "--state",
            "open",
            "--label",
            label,
            "--limit",
            "200",
            "--json",
            "number,title",
        ])
        .await
    }

    async fn search_issues(&self, query: &str, limit: u32) -> Result<Vec<Issue>> {
        self.json([
            "issue",
            "list",
            "--state",
            "open",
            "--search",
            query,
            "--json",
            "number,title,body,createdAt",
            "--limit",
            &limit.to_string(),
        ])
        .await
    }

    async fn search_issues_in_repo(
        &self,
        repo: &str,
        query: &str,
        limit: u32,
    ) -> Result<Vec<Issue>> {
        self.json([
            "issue",
            "list",
            "-R",
            repo,
            "--state",
            "all",
            "--limit",
            &limit.to_string(),
            "--search",
            query,
            "--json",
            "number,state",
        ])
        .await
    }

    async fn issue_labels(&self, num: u64) -> Result<Vec<String>> {
        let issue: Issue = self
            .json(["issue", "view", &num.to_string(), "--json", "labels"])
            .await?;
        Ok(issue.label_names())
    }

    async fn issue_view(&self, num: u64) -> Result<Issue> {
        self.json([
            "issue",
            "view",
            &num.to_string(),
            "--json",
            "number,title,body,createdAt,labels,state",
        ])
        .await
    }

    async fn issue_body_and_comments(&self, num: u64) -> Result<Issue> {
        self.json(["issue", "view", &num.to_string(), "--json", "body,comments"])
            .await
    }

    async fn issue_comments(&self, num: u64) -> Result<Vec<Comment>> {
        let issue: Issue = self
            .json(["issue", "view", &num.to_string(), "--json", "comments"])
            .await?;
        Ok(issue.comments.unwrap_or_default())
    }

    async fn issue_state(&self, repo: Option<&str>, num: u64) -> Result<String> {
        let num = num.to_string();
        let mut args: Vec<&str> = vec!["issue", "view", &num];
        if let Some(r) = repo {
            args.push("-R");
            args.push(r);
        }
        args.extend(["--json", "state"]);
        let issue: Issue = self.json(args).await?;
        Ok(issue.state.unwrap_or_else(|| "UNKNOWN".to_string()))
    }

    async fn issue_edit_labels(&self, num: u64, add: &[&str], remove: &[&str]) -> Result<()> {
        let num = num.to_string();
        let mut args: Vec<&str> = vec!["issue", "edit", &num];
        for label in add {
            args.push("--add-label");
            args.push(label);
        }
        for label in remove {
            args.push("--remove-label");
            args.push(label);
        }
        self.cmd(args).check().await?;
        Ok(())
    }

    async fn issue_comment(&self, num: u64, body: &str) -> Result<()> {
        self.cmd(["issue", "comment", &num.to_string(), "--body", body])
            .check()
            .await?;
        Ok(())
    }

    async fn issue_create(
        &self,
        repo: &str,
        labels: &[&str],
        title: &str,
        body: &str,
    ) -> Result<String> {
        let mut args: Vec<&str> = vec!["issue", "create", "-R", repo];
        for label in labels {
            args.push("--label");
            args.push(label);
        }
        args.extend(["--title", title, "--body", body]);
        Ok(self.cmd(args).check().await?.trimmed().to_string())
    }

    async fn pr_list_open(&self, limit: u32) -> Result<Vec<Pr>> {
        // Одним запросом на все PR: поштучный опрос `mergeable` занимал бы
        // минуты на каждом круге таймера.
        self.json([
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            &limit.to_string(),
            "--json",
            "number,url,body,updatedAt,mergeable,mergeStateStatus,autoMergeRequest,headRefName",
        ])
        .await
    }

    async fn pr_search_open_url(&self, query: &str) -> Result<Option<String>> {
        let prs: Vec<Pr> = self
            .json([
                "pr", "list", "--state", "open", "--search", query, "--json", "url",
            ])
            .await?;
        Ok(prs.into_iter().map(|p| p.url).find(|u| !u.is_empty()))
    }

    async fn pr_head_sha(&self, url: &str) -> Result<String> {
        let pr: Pr = self
            .json(["pr", "view", url, "--json", "headRefOid"])
            .await?;
        Ok(pr.head_ref_oid.unwrap_or_default())
    }

    async fn pr_head_branch(&self, url: &str) -> Result<String> {
        let pr: Pr = self
            .json(["pr", "view", url, "--json", "headRefName"])
            .await?;
        Ok(pr.head_ref_name.unwrap_or_default())
    }

    async fn pr_mergeable_once(&self, url: &str) -> Result<Mergeable> {
        let pr: Pr = self
            .json(["pr", "view", url, "--json", "mergeable"])
            .await?;
        Ok(pr.mergeable_state())
    }

    async fn pr_merge_state(&self, url: &str) -> Result<String> {
        let pr: Pr = self
            .json(["pr", "view", url, "--json", "mergeStateStatus"])
            .await?;
        Ok(pr
            .merge_state_status
            .unwrap_or_else(|| "UNKNOWN".to_string()))
    }

    async fn pr_comments(&self, url: &str) -> Result<Vec<Comment>> {
        #[derive(serde::Deserialize)]
        struct Wrap {
            #[serde(default)]
            comments: Vec<Comment>,
        }
        let wrap: Wrap = self.json(["pr", "view", url, "--json", "comments"]).await?;
        Ok(wrap.comments)
    }

    async fn pr_create_draft(
        &self,
        base: &str,
        head: &str,
        title: &str,
        body: &str,
    ) -> Result<String> {
        Ok(self
            .cmd([
                "pr", "create", "--draft", "--base", base, "--head", head, "--title", title,
                "--body", body,
            ])
            .check()
            .await?
            .trimmed()
            .to_string())
    }

    async fn pr_checks_watch(&self, url: &str) -> Result<bool> {
        let code = self
            .cmd(["pr", "checks", url, "--watch"])
            .status_inherit()
            .await?;
        Ok(code == 0)
    }

    async fn pr_comment(&self, url: &str, body: &str) -> Result<()> {
        self.cmd(["pr", "comment", url, "--body", body])
            .check()
            .await?;
        Ok(())
    }

    async fn pr_close(&self, url: &str, comment: &str) -> Result<()> {
        self.cmd(["pr", "close", url, "--comment", comment])
            .check()
            .await?;
        Ok(())
    }

    async fn pr_ready(&self, url: &str) -> Result<()> {
        self.cmd(["pr", "ready", url]).check().await?;
        Ok(())
    }

    async fn pr_diff(&self, url: &str) -> Result<String> {
        Ok(self.cmd(["pr", "diff", url]).check().await?.stdout)
    }

    async fn pr_merge_squash_auto(&self, url: &str) -> Result<()> {
        self.cmd(["pr", "merge", url, "--squash", "--auto"])
            .check()
            .await?;
        Ok(())
    }

    async fn pr_disable_auto_merge(&self, url: &str) -> Result<()> {
        self.cmd(["pr", "merge", url, "--disable-auto"])
            .check()
            .await?;
        Ok(())
    }

    async fn latest_run_id(&self, branch: &str) -> Result<Option<String>> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Run {
            database_id: u64,
        }
        let runs: Vec<Run> = self
            .json([
                "run",
                "list",
                "--branch",
                branch,
                "--limit",
                "1",
                "--json",
                "databaseId",
            ])
            .await?;
        Ok(runs.first().map(|r| r.database_id.to_string()))
    }

    async fn run_log_failed(&self, id: &str) -> Result<String> {
        let out = self
            .cmd(["run", "view", id, "--log-failed"])
            .output()
            .await?;
        if !out.ok() {
            anyhow::bail!("gh run view {id} --log-failed: код {}", out.code);
        }
        Ok(out.stdout)
    }
}
