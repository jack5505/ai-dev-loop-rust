//! git — ровно те команды, что были в bash, и в том же порядке.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::exec::Cmd;

/// Операции с рабочей копией, нужные итерации.
///
/// Пятый трейт помимо четырёх «внешних миров» из спецификации: без него
/// сценарные тесты требовали бы настоящего клона с настоящим origin, а в
/// сборочном контейнере git отсутствует.
#[async_trait]
pub trait Vcs: Send + Sync {
    /// Пролог итерации: fetch + checkout базовой ветки + reset --hard.
    async fn sync_base(&self, base_branch: &str) -> Result<()>;
    async fn checkout(&self, branch: &str) -> Result<()>;
    async fn checkout_new_branch(&self, branch: &str) -> Result<()>;
    /// Игнорирует ошибку — как `|| true` в bash.
    async fn delete_branch(&self, branch: &str) -> Result<()>;
    /// Сколько коммитов появилось поверх базы.
    async fn commits_ahead(&self, base: &str) -> Result<u64>;
    async fn push_new_branch(&self, branch: &str) -> Result<()>;
    async fn push_force_with_lease(&self) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
    dry_run: bool,
}

impl Git {
    pub fn new(dir: impl Into<PathBuf>, dry_run: bool) -> Self {
        Self {
            dir: dir.into(),
            dry_run,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn cmd<I, S>(&self, args: I) -> Cmd
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Cmd::new("git").args(args).cwd(&self.dir)
    }

    /// Изменяющая команда: в сухом прогоне печатается и не выполняется.
    /// Сухой прогон может идти рядом с боевым bash по тому же клону,
    /// поэтому трогать рабочую копию нельзя.
    async fn mutate<I, S>(&self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let cmd = self.cmd(args);
        if self.dry_run {
            tracing::info!("(dry-run) {}", cmd.display());
            return Ok(());
        }
        cmd.check().await?;
        Ok(())
    }

    pub async fn fetch_origin(&self) -> Result<()> {
        // Чтение: рабочую копию не меняет, поэтому идёт и в сухом прогоне.
        self.cmd(["fetch", "origin"]).check().await?;
        Ok(())
    }

    pub async fn reset_hard(&self, refspec: &str) -> Result<()> {
        self.mutate(["reset", "--hard", refspec]).await
    }

    pub async fn current_branch(&self) -> Result<String> {
        Ok(self
            .cmd(["rev-parse", "--abbrev-ref", "HEAD"])
            .check()
            .await?
            .trimmed()
            .to_string())
    }
}

#[async_trait]
impl Vcs for Git {
    async fn sync_base(&self, base_branch: &str) -> Result<()> {
        self.fetch_origin().await?;
        self.checkout(base_branch).await?;
        self.reset_hard(&format!("origin/{base_branch}")).await?;
        Ok(())
    }

    async fn checkout(&self, branch: &str) -> Result<()> {
        self.mutate(["checkout", branch]).await
    }

    async fn checkout_new_branch(&self, branch: &str) -> Result<()> {
        self.mutate(["checkout", "-B", branch]).await
    }

    async fn delete_branch(&self, branch: &str) -> Result<()> {
        if self.dry_run {
            tracing::info!("(dry-run) git branch -D {branch}");
            return Ok(());
        }
        let out = self.cmd(["branch", "-D", branch]).output().await?;
        if !out.ok() {
            tracing::info!("⚠️ git branch -D {branch}: код {}", out.code);
        }
        Ok(())
    }

    async fn commits_ahead(&self, base: &str) -> Result<u64> {
        let out = self
            .cmd(["rev-list", "--count", &format!("{base}..HEAD")])
            .check()
            .await?;
        out.trimmed()
            .trim()
            .parse()
            .with_context(|| format!("не число в выводе rev-list: {:?}", out.trimmed()))
    }

    async fn push_new_branch(&self, branch: &str) -> Result<()> {
        self.mutate(["push", "-u", "origin", branch, "--force-with-lease"])
            .await
    }

    async fn push_force_with_lease(&self) -> Result<()> {
        self.mutate(["push", "--force-with-lease"]).await
    }
}
