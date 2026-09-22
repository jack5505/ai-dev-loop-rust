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
    args: Vec<String>,
    repo_dir: PathBuf,
}

impl Claude {
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
        Self {
            args,
            repo_dir: cfg.repo_dir.clone(),
        }
    }

    pub fn command_preview(&self) -> String {
        let mut parts = vec!["claude".to_string()];
        parts.extend(self.args.clone());
        shell_words::join(parts)
    }
}

#[async_trait]
impl Agent for Claude {
    async fn run(&self, req: AgentRequest) -> Result<AgentRun> {
        let mut cmd = tokio::process::Command::new("claude");
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

        let mut child = cmd.spawn().context("не удалось запустить claude")?;

        if let Some(data) = req.stdin.as_ref() {
            let mut sink = child.stdin.take().expect("stdin как piped");
            sink.write_all(data.as_bytes())
                .await
                .context("передача диффа агенту через stdin")?;
            sink.shutdown().await.ok();
            drop(sink);
        }

        // `2>&1 | tee лог`: вывод идёт и в журнал юнита, и в файл, и в
        // буфер (ревью читает вердикт из него).
        let mut out_lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
        let mut err_lines = BufReader::new(child.stderr.take().expect("stderr")).lines();
        let mut buf = String::new();
        let mut file = match req.log_file.as_deref() {
            Some(path) => Some(open_log(path).await?),
            None => None,
        };

        loop {
            let line = tokio::select! {
                l = out_lines.next_line() => l.context("чтение stdout агента")?,
                l = err_lines.next_line() => l.context("чтение stderr агента")?,
            };
            match line {
                Some(line) => {
                    println!("{line}");
                    buf.push_str(&line);
                    buf.push('\n');
                    if let Some(f) = file.as_mut() {
                        let _ = f.write_all(line.as_bytes()).await;
                        let _ = f.write_all(b"\n").await;
                    }
                }
                None => break,
            }
        }
        // Второй поток мог не закрыться одновременно с первым.
        while let Ok(Some(line)) = out_lines.next_line().await {
            println!("{line}");
            buf.push_str(&line);
            buf.push('\n');
            if let Some(f) = file.as_mut() {
                let _ = f.write_all(line.as_bytes()).await;
                let _ = f.write_all(b"\n").await;
            }
        }
        while let Ok(Some(line)) = err_lines.next_line().await {
            println!("{line}");
            buf.push_str(&line);
            buf.push('\n');
            if let Some(f) = file.as_mut() {
                let _ = f.write_all(line.as_bytes()).await;
                let _ = f.write_all(b"\n").await;
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
