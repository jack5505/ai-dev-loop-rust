//! `ai-dev` — один бинарник с подкоммандами вместо двух bash-скриптов.
//!
//! Коды возврата: `0` — штатное завершение (включая «очередь пуста»,
//! «замок занят» и «ушло человеку»), `1` — авария, `2` — ошибка
//! конфигурации, которую не надо доводить до алерта «оркестратор упал».

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use ai_dev::agent::{Agent, Claude, DryAgent};
use ai_dev::clock::{Clock, SystemClock};
use ai_dev::config::{AgentAuth, Config, Report};
use ai_dev::exec::{systemctl, which, Cmd};
use ai_dev::gh::cli::GhCli;
use ai_dev::gh::dry::DryGitHub;
use ai_dev::gh::GitHub;
use ai_dev::git::{Git, Vcs};
use ai_dev::keeper::{self, KeeperOutcome};
use ai_dev::lock;
use ai_dev::logging;
use ai_dev::notify::{Notifier, NullNotifier, RecordingNotifier, Telegram};
use ai_dev::orchestrator::{self, Ctx, Outcome};
use ai_dev::queue;
use ai_dev::shutdown::Shutdown;
use ai_dev::state::IterationState;
use ai_dev::{exit, markers};

#[derive(Parser, Debug)]
#[command(
    name = "ai-dev",
    version,
    about = "Оркестратор AI dev loop: issue → PR → CI → ревью → merge"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Одна итерация цикла (§2).
    Run {
        #[arg(long)]
        instance: Option<String>,
        /// Одна итерация и выход — поведение по умолчанию, флаг для явности.
        #[arg(long)]
        once: bool,
        /// Печатать изменяющие действия, не выполняя их.
        #[arg(long)]
        dry_run: bool,
        /// Взять конкретную задачу вместо головы очереди.
        #[arg(long)]
        issue: Option<u64>,
    },
    /// Смотритель бэклога (§3).
    Backlog {
        #[arg(long)]
        instance: Option<String>,
        /// Игнорировать недельную отметку.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Очередь задач — тот же запрос, что у итерации (C1).
    Queue {
        #[arg(long)]
        instance: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Разблокировка задач, чей блокер закрыт (§2.1).
    Unblock {
        #[arg(long)]
        instance: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Сводка по открытым AI-PR (§2.2).
    WatchPrs {
        #[arg(long)]
        instance: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Замок, отметки, состояние последних итераций.
    Status {
        #[arg(long)]
        instance: Option<String>,
    },
    /// Проверки конфигурации.
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// Окружение: gh, git, claude, токены, юниты.
    Doctor,
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Валидация env, включая C9 (APP_WAIT_MIN vs TimeoutStartSec).
    Check {
        #[arg(long)]
        instance: Option<String>,
    },
}

fn main() -> ExitCode {
    logging::init();
    let cli = Cli::parse();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("не удалось запустить рантайм: {e}");
            return ExitCode::from(exit::CRASH as u8);
        }
    };
    let code = rt.block_on(dispatch(cli));
    ExitCode::from(code as u8)
}

async fn dispatch(cli: Cli) -> i32 {
    match run_command(cli).await {
        Ok(code) => code,
        Err(e) => {
            tracing::info!("❌ {e:#}");
            exit::CRASH
        }
    }
}

async fn run_command(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Run {
            instance,
            once: _,
            dry_run,
            issue,
        } => cmd_run(instance.as_deref(), dry_run, issue).await,
        Command::Backlog {
            instance,
            force,
            dry_run,
        } => cmd_backlog(instance.as_deref(), force, dry_run).await,
        Command::Queue { instance, json } => cmd_queue(instance.as_deref(), json).await,
        Command::Unblock { instance, dry_run } => cmd_unblock(instance.as_deref(), dry_run).await,
        Command::WatchPrs { instance, json } => cmd_watch_prs(instance.as_deref(), json).await,
        Command::Status { instance } => cmd_status(instance.as_deref()).await,
        Command::Config { action } => match action {
            ConfigCmd::Check { instance } => cmd_config_check(instance.as_deref()).await,
        },
        Command::Doctor => cmd_doctor().await,
    }
}

/// Загрузка конфигурации с правильным кодом возврата: ошибка конфигурации
/// не должна выглядеть как падение оркестратора.
fn load_config(instance: Option<&str>) -> Result<Config, i32> {
    match Config::load(instance) {
        Ok(cfg) => Ok(cfg),
        Err(e) => {
            tracing::info!("⚙️ Ошибка конфигурации: {e:#}");
            Err(exit::CONFIG)
        }
    }
}

fn print_report(report: &Report) {
    for w in &report.warnings {
        tracing::info!("⚠️ {w}");
    }
    for e in &report.errors {
        tracing::info!("⚙️ {e}");
    }
}

struct Built {
    ctx: Ctx,
}

/// Сборка окружения итерации. `need_repo_name` — нужен ли `gh repo view`
/// (bash спрашивал его только при заданном `PARTNER_REPO`).
async fn build_ctx(
    instance: Option<&str>,
    dry_run: bool,
    need_repo_name: bool,
) -> Result<Result<Built, i32>> {
    let cfg = match load_config(instance) {
        Ok(cfg) => cfg,
        Err(code) => return Ok(Err(code)),
    };
    let report = cfg.validate();
    if !report.ok() {
        print_report(&report);
        return Ok(Err(exit::CONFIG));
    }
    for w in &report.warnings {
        tracing::info!("⚠️ {w}");
    }

    let real: Arc<dyn GitHub> = Arc::new(GhCli::new(&cfg.repo_dir));
    let gh_api: Arc<dyn GitHub> = if dry_run {
        Arc::new(DryGitHub::new(real.clone()))
    } else {
        real.clone()
    };

    let agent: Arc<dyn Agent> = if dry_run {
        Arc::new(DryAgent)
    } else {
        Arc::new(Claude::from_config(&cfg))
    };

    let notify: Arc<dyn Notifier> = if dry_run {
        Arc::new(RecordingNotifier::default())
    } else {
        match cfg.telegram() {
            Some((token, chat)) => Arc::new(Telegram::new(token, chat)),
            None => Arc::new(NullNotifier),
        }
    };

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let git: Arc<dyn Vcs> = Arc::new(Git::new(&cfg.repo_dir, dry_run));
    let shutdown = Shutdown::listen();

    // Логин владельца токена: им пишутся комментарии, и по нему же
    // отличается инструктаж оркестратора от сигнала агента (C2).
    let self_login = real
        .self_login()
        .await
        .context("gh api user — проверь авторизацию gh (ai-dev doctor)")?;
    let this_repo = if need_repo_name || cfg.partner_repo.is_some() {
        real.repo_name_with_owner().await.context("gh repo view")?
    } else {
        String::new()
    };

    Ok(Ok(Built {
        ctx: Ctx {
            cfg,
            gh: gh_api,
            agent,
            notify,
            clock,
            git,
            shutdown,
            self_login,
            this_repo,
            dry_run,
        },
    }))
}

async fn cmd_run(instance: Option<&str>, dry_run: bool, issue: Option<u64>) -> Result<i32> {
    let built = match build_ctx(instance, dry_run, false).await? {
        Ok(b) => b,
        Err(code) => return Ok(code),
    };
    let ctx = built.ctx;

    // C9 проверяем на старте run: юнит с маленьким TimeoutStartSec убьёт
    // итерацию посреди ожидания.
    let mut report = Report::default();
    ctx.cfg.check_unit_timeout(&mut report).await;
    if !report.ok() {
        print_report(&report);
        return Ok(exit::CONFIG);
    }
    for w in &report.warnings {
        tracing::info!("⚠️ {w}");
    }

    let _guard = match lock::try_acquire(&ctx.cfg.lock_file)? {
        Some(g) => g,
        None => {
            tracing::info!("Другой запуск ещё работает — выходим.");
            return Ok(exit::OK);
        }
    };

    match orchestrator::run(&ctx, issue).await {
        Ok(outcome) => {
            log_outcome(&outcome);
            Ok(exit::OK)
        }
        Err(_) => Ok(exit::CRASH),
    }
}

fn log_outcome(outcome: &Outcome) {
    match outcome {
        Outcome::QueueEmpty => {}
        Outcome::LockBusy => {}
        Outcome::AgentUnavailable { issue } => {
            tracing::info!("Итог: задача #{issue} осталась в очереди (агент недоступен).")
        }
        Outcome::BlockedOnPartner { issue, blocker } => {
            tracing::info!("Итог: задача #{issue} ждёт {blocker}.")
        }
        Outcome::HandedToHuman { issue, why } => {
            tracing::info!("Итог: задача #{issue} у человека ({why:?}).")
        }
        Outcome::PrAwaitingReview { issue, pr } => {
            tracing::info!("Итог: задача #{issue} — PR ждёт решения: {pr}")
        }
        Outcome::Merged { issue, pr } => {
            tracing::info!("Итог: задача #{issue} — PR на авто-merge: {pr}")
        }
    }
}

async fn cmd_backlog(instance: Option<&str>, force: bool, dry_run: bool) -> Result<i32> {
    let built = match build_ctx(instance, dry_run, true).await? {
        Ok(b) => b,
        Err(code) => return Ok(code),
    };
    let ctx = built.ctx;

    // Отметку проверяем ДО замка: смысла ждать чужую итерацию нет.
    if !force && keeper::too_early(&ctx.log_dir(), ctx.cfg.keeper_interval_days) {
        return Ok(exit::OK);
    }

    // C10: замок общий с оркестратором — смотритель переставляет метки,
    // и делать это под работающей итерацией нельзя.
    let _guard = match lock::try_acquire(&ctx.cfg.lock_file)? {
        Some(g) => g,
        None => {
            tracing::info!("Итерация ai-dev идёт — смотритель попробует через час.");
            return Ok(exit::OK);
        }
    };

    match keeper::run(&ctx, force).await {
        Ok(KeeperOutcome::TooEarly) => Ok(exit::OK),
        Ok(_) => Ok(exit::OK),
        Err(e) => {
            tracing::info!("❌ Смотритель упал: {e:#}");
            ctx.tg(&ai_dev::prompts::keeper::tg_crash(
                &ctx.cfg.repo_basename(),
                &format!("{e:#}"),
            ))
            .await;
            Ok(exit::CRASH)
        }
    }
}

async fn cmd_queue(instance: Option<&str>, as_json: bool) -> Result<i32> {
    let built = match build_ctx(instance, false, false).await? {
        Ok(b) => b,
        Err(code) => return Ok(code),
    };
    let ctx = built.ctx;
    let authors = queue::authors(&ctx.cfg, &ctx.self_login);
    let issues = queue::fetch(ctx.gh.as_ref(), &ctx.cfg, &authors, 200).await?;

    if as_json {
        let rows: Vec<serde_json::Value> = issues
            .iter()
            .map(|i| {
                serde_json::json!({
                    "number": i.number,
                    "title": i.title,
                    "createdAt": i.created_at,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "filter": queue::label_filter(&ctx.cfg),
                "authors": authors,
                "count": issues.len(),
                "issues": rows,
            }))?
        );
        return Ok(exit::OK);
    }

    println!("Запрос: {} author:<каждый>", queue::label_filter(&ctx.cfg));
    println!("Авторы: {}", authors.join(", "));
    println!("В очереди: {}", issues.len());
    for i in &issues {
        let created = i
            .created_at
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "?".to_string());
        println!("  #{:<6} {created}  {}", i.number, i.title);
    }
    Ok(exit::OK)
}

async fn cmd_unblock(instance: Option<&str>, dry_run: bool) -> Result<i32> {
    let built = match build_ctx(instance, dry_run, false).await? {
        Ok(b) => b,
        Err(code) => return Ok(code),
    };
    let ctx = built.ctx;
    let _guard = match lock::try_acquire(&ctx.cfg.lock_file)? {
        Some(g) => g,
        None => {
            tracing::info!("Другой запуск ещё работает — выходим.");
            return Ok(exit::OK);
        }
    };
    orchestrator::unblock::run(&ctx).await?;
    Ok(exit::OK)
}

async fn cmd_watch_prs(instance: Option<&str>, as_json: bool) -> Result<i32> {
    let built = match build_ctx(instance, false, false).await? {
        Ok(b) => b,
        Err(code) => return Ok(code),
    };
    let ctx = built.ctx;

    if !as_json {
        orchestrator::pr_watch::run(&ctx).await?;
        return Ok(exit::OK);
    }

    // JSON-режим: без отметок и уведомлений, только данные.
    let prs = ctx.gh.pr_list_open(100).await?;
    let stale_before = ctx.clock.now() - chrono::Duration::days(ctx.cfg.pr_stale_days);
    let rows: Vec<serde_json::Value> = prs
        .iter()
        .filter(|p| markers::body_refers_to_task(p.body_str()))
        .map(|p| {
            let stale = p.updated_at.map(|u| u < stale_before).unwrap_or(false);
            serde_json::json!({
                "number": p.number,
                "url": p.url,
                "mergeable": p.mergeable_state().as_str(),
                "updatedAt": p.updated_at,
                "stale": stale,
                "issue": markers::task_number_in_body(p.body_str()),
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(exit::OK)
}

async fn cmd_status(instance: Option<&str>) -> Result<i32> {
    let cfg = match load_config(instance) {
        Ok(cfg) => cfg,
        Err(code) => return Ok(code),
    };
    println!(
        "Инстанс:      {}",
        cfg.instance
            .clone()
            .unwrap_or_else(|| "(из окружения)".into())
    );
    if let Some(src) = &cfg.source {
        println!("Конфиг:       {}", src.display());
    }
    println!("Репозиторий:  {}", cfg.repo_dir.display());
    println!(
        "Режим:        {}, база {}, авто-merge: {}",
        cfg.dev_mode.as_str(),
        cfg.base_branch,
        if cfg.auto_merge { "да" } else { "нет" }
    );
    let busy = lock::is_busy(&cfg.lock_file).unwrap_or(false);
    println!(
        "Замок:        {} — {}",
        cfg.lock_file.display(),
        if busy {
            "занят (идёт итерация)"
        } else {
            "свободен"
        }
    );

    let log_dir = cfg.log_dir();
    let stamp = keeper::stamp_path(&log_dir);
    match std::fs::metadata(&stamp).and_then(|m| m.modified()) {
        Ok(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            println!(
                "Смотритель:   последний прогон {} (интервал {} дн.)",
                dt.format("%Y-%m-%d %H:%M"),
                cfg.keeper_interval_days
            );
        }
        Err(_) => println!("Смотритель:   ни разу не запускался"),
    }
    let pr_stamp = log_dir.join(format!(
        ".pr-watch-{}",
        chrono::Local::now().format("%Y-%m-%d")
    ));
    println!(
        "Сводка PR:    {}",
        if pr_stamp.exists() {
            "сегодня уже отправлена"
        } else {
            "сегодня ещё не отправлялась"
        }
    );

    let states = IterationState::all(&log_dir);
    if states.is_empty() {
        println!("Итерации:     состояний нет ({})", log_dir.display());
    } else {
        println!("Итерации:");
        for s in states {
            println!(
                "  #{:<6} {:<12} {:<18} {}",
                s.issue,
                s.stage.as_str(),
                s.outcome.clone().unwrap_or_else(|| "-".into()),
                s.pr.clone().unwrap_or_else(|| "-".into())
            );
        }
    }
    Ok(exit::OK)
}

async fn cmd_config_check(instance: Option<&str>) -> Result<i32> {
    let cfg = match load_config(instance) {
        Ok(cfg) => cfg,
        Err(code) => return Ok(code),
    };
    let mut report = cfg.validate();
    cfg.check_unit_timeout(&mut report).await;

    println!(
        "Инстанс:          {}",
        cfg.instance
            .clone()
            .unwrap_or_else(|| "(из окружения)".into())
    );
    println!("REPO_DIR:         {}", cfg.repo_dir.display());
    println!("DEV_MODE:         {}", cfg.dev_mode.as_str());
    println!(
        "Аутентификация:   {}",
        match cfg.auth {
            Some(AgentAuth::Oauth) => "CLAUDE_CODE_OAUTH_TOKEN",
            Some(AgentAuth::ApiKey) => "ANTHROPIC_API_KEY",
            None => "не задана",
        }
    );
    println!("APP_WAIT_MIN:     {} мин", cfg.app_wait_min);
    println!("LOCK_FILE:        {}", cfg.lock_file.display());
    println!(
        "PARTNER_REPO:     {}",
        cfg.partner_repo
            .clone()
            .unwrap_or_else(|| "(не задан)".into())
    );
    println!(
        "ALLOWED_AUTHORS:  {}",
        if cfg.allowed_authors.is_empty() {
            "(владелец токена)".to_string()
        } else {
            cfg.allowed_authors.join(", ")
        }
    );
    println!(
        "Telegram:         {}",
        if cfg.telegram().is_some() {
            "настроен"
        } else {
            "выключен"
        }
    );

    for w in &report.warnings {
        println!("⚠️  {w}");
    }
    for e in &report.errors {
        println!("⚙️  {e}");
    }
    if report.ok() {
        println!("✅ Конфигурация пригодна.");
        Ok(exit::OK)
    } else {
        Ok(exit::CONFIG)
    }
}

async fn cmd_doctor() -> Result<i32> {
    let mut problems = 0;

    println!("— Инструменты —");
    for tool in ["gh", "git", "claude", "jq", "flock", "curl"] {
        match which(tool) {
            Some(p) => println!("  ✅ {tool:<7} {}", p.display()),
            None => {
                // jq и flock нужны только bash-версии: Rust их не вызывает.
                let critical = matches!(tool, "gh" | "git" | "claude");
                if critical {
                    problems += 1;
                    println!("  ❌ {tool:<7} не найден в PATH");
                } else {
                    println!("  ·  {tool:<7} не найден (нужен только bash-версии)");
                }
            }
        }
    }

    println!("— Авторизация gh —");
    match Cmd::new("gh").args(["auth", "status"]).output().await {
        Ok(out) if out.ok() => {
            for line in out.stdout.lines().chain(out.stderr.lines()).take(4) {
                if !line.trim().is_empty() {
                    println!("  {}", line.trim());
                }
            }
        }
        Ok(out) => {
            problems += 1;
            println!("  ❌ gh auth status: код {}", out.code);
        }
        Err(e) => {
            problems += 1;
            println!("  ❌ {e:#}");
        }
    }

    println!("— Инстансы —");
    let mut found = Vec::new();
    for dir in instance_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(rest) = name.strip_prefix("ai-dev-") {
                if let Some(inst) = rest.strip_suffix(".env") {
                    if !found.iter().any(|(i, _): &(String, _)| i == inst) {
                        found.push((inst.to_string(), e.path()));
                    }
                }
            }
        }
    }
    if found.is_empty() {
        println!("  ·  конфигов ai-dev-<инстанс>.env не найдено");
    }
    for (inst, path) in &found {
        match Config::load(Some(inst)) {
            Ok(cfg) => {
                let mut report = cfg.validate();
                cfg.check_unit_timeout(&mut report).await;
                let mark = if report.ok() { "✅" } else { "⚙️" };
                println!(
                    "  {mark} {inst:<9} {} → {} ({})",
                    path.display(),
                    cfg.repo_dir.display(),
                    cfg.dev_mode.as_str()
                );
                for e in &report.errors {
                    problems += 1;
                    println!("       ⚙️ {e}");
                }
                for w in &report.warnings {
                    println!("       ⚠️ {w}");
                }
            }
            Err(e) => {
                problems += 1;
                println!("  ❌ {inst}: {e:#}");
            }
        }
    }

    println!("— Юниты —");
    let mut any_unit = false;
    for user_scope in [false, true] {
        let out = systemctl(user_scope)
            .args([
                "list-units",
                "--all",
                "--no-legend",
                "--no-pager",
                "ai-dev*",
            ])
            .output()
            .await;
        if let Ok(out) = out {
            for line in out.stdout.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                any_unit = true;
                println!(
                    "  {} {}",
                    if user_scope { "--user" } else { "system" },
                    line.trim()
                );
            }
        }
    }
    if !any_unit {
        println!("  ·  юнитов ai-dev* не видно (это нормально без прав на чужой журнал)");
    }
    println!(
        "Диагностика журнала: journalctl -u 'ai-dev@*' -n 200 --no-pager -q \
         (для rootless — с ключом --user)"
    );

    if problems == 0 {
        println!("✅ Окружение пригодно.");
        Ok(exit::OK)
    } else {
        println!("Найдено проблем: {problems}");
        Ok(exit::CONFIG)
    }
}

fn instance_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("AI_DEV_CONFIG_DIR") {
        dirs.push(std::path::PathBuf::from(d));
    }
    dirs.push(std::path::PathBuf::from("/etc"));
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME") {
        dirs.push(std::path::PathBuf::from(d));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(std::path::PathBuf::from(home).join(".config"));
    }
    dirs
}
