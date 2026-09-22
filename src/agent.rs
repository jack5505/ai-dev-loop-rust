//! Агент — `claude -p`. Единственная точка, где ему отдаётся промпт.
//!
//! **C8**: Telegram-токен вычищается из окружения перед каждым запуском,
//! иначе секрет уезжает в промпт вместе с окружением процесса.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::{AgentAuth, Config};

/// Что просят у агента.
#[derive(Debug, Clone, Default)]
pub struct AgentRequest {
    pub prompt: String,
    /// Дифф для ревью: единственное место, где используется stdin.
    pub stdin: Option<String>,
    /// Куда сложить копию вывода (`tee` в bash).
    pub log_file: Option<PathBuf>,
}

/// Чем закончился запуск.
#[derive(Debug, Clone, Default)]
pub struct AgentRun {
    /// Нулевой код возврата. `false` = «агент недоступен»: скорее всего
    /// исчерпан лимит подписки, и задача мягко возвращается в очередь.
    pub success: bool,
    pub output: String,
}

#[async_trait]
pub trait Agent: Send + Sync {
    async fn run(&self, req: AgentRequest) -> Result<AgentRun>;
}

pub struct Claude {
    program: String,
    args: Vec<String>,
    repo_dir: PathBuf,
}

impl Claude {
    pub fn new(
        program: impl Into<String>,
        args: Vec<String>,
        repo_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program: program.into(),
            args,
            repo_dir: repo_dir.into(),
        }
    }

    pub fn from_config(cfg: &Config) -> Self {
        // Бюджетный лимит имеет смысл только с API-ключом.
        let mut args = vec![
            "-p".to_string(),
            "--dangerously-skip-permissions".to_string(),
            "--model".to_string(),
            cfg.claude_model.clone(),
        ];
        if cfg.auth == Some(AgentAuth::ApiKey) {
            args.push("--max-budget-usd".to_string());
            args.push(cfg.max_budget_usd.clone());
        }
        Self::new("claude", args, cfg.repo_dir.clone())
    }

    pub fn command_preview(&self) -> String {
        let mut parts = vec![self.program.clone()];
        parts.extend(self.args.clone());
        shell_words::join(parts)
    }
}

#[async_trait]
impl Agent for Claude {
    async fn run(&self, req: AgentRequest) -> Result<AgentRun> {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&self.args)
            .arg(&req.prompt)
            .current_dir(&self.repo_dir)
            // C8: секреты Telegram агенту не показываем.
            .env_remove("TELEGRAM_BOT_TOKEN")
            .env_remove("TELEGRAM_CHAT_ID")
            .stdin(if req.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("не удалось запустить {}", self.program))?;

        if let Some(data) = req.stdin.as_ref() {
            let mut sink = child.stdin.take().expect("stdin как piped");
            sink.write_all(data.as_bytes())
                .await
                .context("передача диффа агенту через stdin")?;
            sink.shutdown().await.ok();
            drop(sink);
        }

        // `2>&1 | tee лог`: оба потока идут в журнал юнита и в файл, но в
        // `output` попадает ТОЛЬКО stdout. Ревью читает вердикт из него, а
        // предупреждения CLI на stderr не должны уезжать комментарием в PR:
        // в bash ревью снималось как `$(claude …)` без `2>&1`, и stderr туда
        // не попадал.
        let mut out_lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
        let mut err_lines = BufReader::new(child.stderr.take().expect("stderr")).lines();
        let mut buf = String::new();
        let mut file = match req.log_file.as_deref() {
            Some(path) => Some(open_log(path).await?),
            None => None,
        };
        let mut out_open = true;
        let mut err_open = true;

        while out_open || err_open {
            let (line, is_stdout) = tokio::select! {
                l = out_lines.next_line(), if out_open => {
                    match l.context("чтение stdout агента")? {
                        Some(line) => (line, true),
                        None => { out_open = false; continue; }
                    }
                }
                l = err_lines.next_line(), if err_open => {
                    match l.context("чтение stderr агента")? {
                        Some(line) => (line, false),
                        None => { err_open = false; continue; }
                    }
                }
            };
            println!("{line}");
            if let Some(f) = file.as_mut() {
                let _ = f.write_all(line.as_bytes()).await;
                let _ = f.write_all(b"\n").await;
            }
            if is_stdout {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
        if let Some(f) = file.as_mut() {
            let _ = f.flush().await;
        }

        let status = child.wait().await.context("ожидание claude")?;
        Ok(AgentRun {
            success: status.success(),
            output: buf,
        })
    }
}

async fn open_log(path: &Path) -> Result<tokio::fs::File> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("создание {}", dir.display()))?;
    }
    tokio::fs::File::create(path)
        .await
        .with_context(|| format!("создание {}", path.display()))
}

/// Агент для `--dry-run`: промпт печатается, работа не делается.
///
/// Ревью получает синтетический `VERDICT: APPROVE` — иначе сухой прогон
/// не доходил бы до конца итерации и последовательность действий была бы
/// видна не до конца.
#[derive(Debug, Default, Clone)]
pub struct DryAgent;

#[async_trait]
impl Agent for DryAgent {
    async fn run(&self, req: AgentRequest) -> Result<AgentRun> {
        let first = req.prompt.lines().next().unwrap_or("").trim();
        tracing::info!(
            "(dry-run) claude -p «{first}…» ({} строк промпта{})",
            req.prompt.lines().count(),
            if req.stdin.is_some() {
                ", дифф на stdin"
            } else {
                ""
            }
        );
        Ok(AgentRun {
            success: true,
            output: "(dry-run: агент не запускался)\nVERDICT: APPROVE".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Регрессия, найденная первой боевой итерацией: предупреждение CLI
    /// со stderr уехало в комментарий с авто-ревью прямо в PR. В `output`
    /// должен попадать только stdout, а в лог — оба потока.
    #[tokio::test]
    async fn stderr_reaches_the_log_but_not_the_output() {
        let dir = std::env::temp_dir().join(format!("ai-dev-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("каталог теста");
        let log = dir.join("agent.log");

        let agent = Claude::new(
            "sh",
            vec![
                "-c".to_string(),
                "printf 'замечаний нет\nVERDICT: APPROVE\n';                  printf 'Ignoring 4 permissions.allow entries\n' >&2"
                    .to_string(),
            ],
            &dir,
        );
        let run = agent
            .run(AgentRequest {
                prompt: String::new(),
                stdin: None,
                log_file: Some(log.clone()),
            })
            .await
            .expect("запуск агента");

        assert!(run.success);
        assert!(run.output.contains("VERDICT: APPROVE"));
        assert!(
            !run.output.contains("Ignoring"),
            "stderr не должен попадать в вывод: {:?}",
            run.output
        );
        let logged = std::fs::read_to_string(&log).expect("лог агента");
        assert!(logged.contains("Ignoring"), "stderr обязан быть в логе");
        assert!(logged.contains("VERDICT: APPROVE"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
