//! Единая точка запуска внешних процессов: `gh`, `git`, `claude`, `systemctl`.
//!
//! Bash полагался на `set -e` и `pipefail`; здесь вместо `$LINENO` —
//! человекочитаемая цепочка `.context()`, поэтому каждая ошибка запуска
//! несёт саму команду.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

/// Результат завершившегося процесса.
#[derive(Debug, Clone)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == 0
    }

    /// stdout без хвостового перевода строки — как `$(...)` в bash.
    pub fn trimmed(&self) -> &str {
        self.stdout.trim_end_matches('\n')
    }
}

/// Команда на запуск. Собирается явно, чтобы её можно было напечатать
/// в `--dry-run` ровно в том виде, в каком она пошла бы наружу.
#[derive(Debug, Clone)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env_remove: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub stdin: Option<String>,
}

impl Cmd {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env_remove: Vec::new(),
            env_set: Vec::new(),
            stdin: None,
        }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(it.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// C8: Telegram-токен агенту не показываем. Аналог `env -u NAME`.
    pub fn env_remove(mut self, name: impl Into<String>) -> Self {
        self.env_remove.push(name.into());
        self
    }

    /// Переменная окружения для дочернего процесса.
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env_set.push((name.into(), value.into()));
        self
    }

    pub fn stdin(mut self, data: impl Into<String>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    /// Человекочитаемая строка команды для логов и `--dry-run`.
    pub fn display(&self) -> String {
        let mut parts = Vec::with_capacity(self.args.len() + 1);
        parts.push(self.program.clone());
        parts.extend(self.args.iter().cloned());
        shell_words::join(parts)
    }

    fn build(&self) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.program);
        c.args(&self.args);
        // Отмена итерации по SIGTERM должна забирать с собой и дочерний
        // процесс: иначе `gh pr checks --watch` остаётся висеть сиротой.
        c.kill_on_drop(true);
        if let Some(dir) = &self.cwd {
            c.current_dir(dir);
        }
        for name in &self.env_remove {
            c.env_remove(name);
        }
        for (name, value) in &self.env_set {
            c.env(name, value);
        }
        c
    }

    /// Запустить и дождаться: stdout и stderr захватываются.
    pub async fn output(&self) -> Result<Output> {
        let mut cmd = self.build();
        cmd.stdin(if self.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("не удалось запустить: {}", self.display()))?;

        if let Some(data) = &self.stdin {
            let mut sink = child.stdin.take().expect("stdin запрошен как piped");
            sink.write_all(data.as_bytes())
                .await
                .with_context(|| format!("запись в stdin: {}", self.display()))?;
            sink.shutdown().await.ok();
            drop(sink);
        }

        let out = child
            .wait_with_output()
            .await
            .with_context(|| format!("ожидание процесса: {}", self.display()))?;

        Ok(Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// Запустить и потребовать нулевой код возврата.
    pub async fn check(&self) -> Result<Output> {
        let out = self.output().await?;
        if !out.ok() {
            anyhow::bail!(
                "`{}` завершилась с кодом {}{}",
                self.display(),
                out.code,
                first_line_of(&out.stderr)
            );
        }
        Ok(out)
    }

    /// Запуск без перехвата потоков: нужен `gh pr checks --watch`,
    /// чей прогресс должен идти в журнал юнита как раньше.
    pub async fn status_inherit(&self) -> Result<i32> {
        let mut cmd = self.build();
        cmd.stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let status = cmd
            .status()
            .await
            .with_context(|| format!("ожидание процесса: {}", self.display()))?;
        Ok(status.code().unwrap_or(-1))
    }
}

fn first_line_of(stderr: &str) -> String {
    match stderr.lines().find(|l| !l.trim().is_empty()) {
        Some(l) => format!(": {}", l.trim()),
        None => String::new(),
    }
}

/// `systemctl` с правильной областью.
///
/// В rootless-раскладке юниты пользовательские, а в неинтерактивной сессии
/// (юнит, ssh-команда) `XDG_RUNTIME_DIR` не выставлен — без него
/// `systemctl --user` не найдёт шину и промолчит.
pub fn systemctl(user_scope: bool) -> Cmd {
    let mut cmd = Cmd::new("systemctl");
    if user_scope {
        cmd = cmd.arg("--user");
        if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
            let dir = format!("/run/user/{}", unsafe { libc::getuid() });
            if Path::new(&dir).is_dir() {
                cmd = cmd.env("XDG_RUNTIME_DIR", dir);
            }
        }
    }
    cmd
}

/// Есть ли исполняемый файл в PATH (для `ai-dev doctor`).
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}
