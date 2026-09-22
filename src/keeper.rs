//! §3 — смотритель бэклога.
//!
//! Раз в неделю даёт агенту два поручения подряд: разобрать затор, а если
//! очередь и после разбора пуста — завести новые задачи. Порядок
//! принципиален: на 2026-09-19 в двух репозиториях было 113 открытых задач
//! и 0 в очереди — конвейер стоял не от голода, а от затора. Заводить
//! новое поверх затора значит гнать задачи в ту же пробку.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::agent::AgentRequest;
use crate::orchestrator::Ctx;
use crate::prompts::keeper as texts;
use crate::queue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeeperOutcome {
    /// Отметка моложе `KEEPER_INTERVAL_DAYS` — не наш круг.
    TooEarly,
    /// Разбор вернул задачи в очередь, новые не нужны.
    Triaged { before: usize, after: usize },
    /// Очередь пуста и после разбора — заведены новые задачи.
    Created { before: usize, created: usize },
}

pub fn stamp_path(log_dir: &std::path::Path) -> PathBuf {
    log_dir.join(".backlog-keeper-last")
}

/// Отметка о прошлом прогоне ещё свежая?
///
/// Таймер тикает раз в час, а работаем раз в `KEEPER_INTERVAL_DAYS` суток:
/// недельный таймер здесь не годится — замок общий с оркестратором, и
/// тик, попавший на многочасовую итерацию, стоил бы целой недели.
pub fn too_early(log_dir: &std::path::Path, interval_days: u64) -> bool {
    let path = stamp_path(log_dir);
    let Ok(meta) = std::fs::metadata(&path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(age) = modified.elapsed() else {
        return false;
    };
    age.as_secs() / 86_400 < interval_days
}

pub async fn run(ctx: &Ctx, force: bool) -> Result<KeeperOutcome> {
    let log_dir = ctx.log_dir();
    std::fs::create_dir_all(&log_dir).with_context(|| format!("создание {}", log_dir.display()))?;

    if !force && too_early(&log_dir, ctx.cfg.keeper_interval_days) {
        return Ok(KeeperOutcome::TooEarly);
    }

    ctx.git
        .sync_base(&ctx.cfg.base_branch)
        .await
        .context("синхронизация клона с базовой веткой")?;

    let authors = queue::authors(&ctx.cfg, &ctx.self_login);
    let partner = ctx
        .cfg
        .partner_repo
        .clone()
        .unwrap_or_else(|| "(не задан)".to_string());
    // Строка запроса для промпта — ровно та, что была в bash.
    let queue_query = format!(
        "{}{}",
        queue::label_filter(&ctx.cfg),
        authors
            .iter()
            .map(|a| format!(" author:{a}"))
            .collect::<String>()
    );
    let stamp = chrono::Local::now().format("%Y-%m-%d").to_string();

    let before = queue::size(ctx.gh.as_ref(), &ctx.cfg, &authors).await?;
    tracing::info!("Очередь до разбора: {before}");

    // ═══ 1. Разбор затора ═══════════════════════════════════════════
    let triage = texts::triage(
        &ctx.this_repo,
        &partner,
        &queue_query,
        before,
        &ctx.cfg.human_label,
        &ctx.cfg.base_branch,
    );
    ctx.agent
        .run(AgentRequest {
            prompt: triage,
            stdin: None,
            log_file: Some(log_dir.join(format!("backlog-triage-{stamp}.log"))),
        })
        .await
        .context("разбор затора агентом")?;

    let after = queue::size(ctx.gh.as_ref(), &ctx.cfg, &authors).await?;
    tracing::info!("Очередь после разбора: {after} (было {before})");

    // Недельный проход состоялся — отмечаемся ДО второго шага: иначе сбой
    // на нём заставил бы повторять разбор каждый час.
    touch_stamp(&log_dir, ctx.dry_run);

    // ═══ 2. Новые задачи — только в пустую очередь ══════════════════
    if after > 0 {
        tracing::info!("В очереди {after} задач — новые не нужны. Готово.");
        ctx.tg(&texts::tg_triaged(&ctx.cfg.repo_basename(), after, before))
            .await;
        return Ok(KeeperOutcome::Triaged { before, after });
    }

    tracing::info!(
        "Очередь пуста и после разбора — завожу новые задачи (до {}).",
        ctx.cfg.max_new_tasks
    );
    let new_tasks = texts::new_tasks(
        &ctx.this_repo,
        &partner,
        ctx.cfg.max_new_tasks,
        &ctx.cfg.task_label,
    );
    ctx.agent
        .run(AgentRequest {
            prompt: new_tasks,
            stdin: None,
            log_file: Some(log_dir.join(format!("backlog-new-{stamp}.log"))),
        })
        .await
        .context("заведение новых задач агентом")?;

    let created = queue::size(ctx.gh.as_ref(), &ctx.cfg, &authors).await?;
    tracing::info!("Заведено задач: {created}. Готово.");
    ctx.tg(&texts::tg_created(
        &ctx.cfg.repo_basename(),
        created,
        ctx.cfg.max_new_tasks,
    ))
    .await;
    Ok(KeeperOutcome::Created { before, created })
}

fn touch_stamp(log_dir: &std::path::Path, dry_run: bool) {
    let path = stamp_path(log_dir);
    if dry_run {
        tracing::info!("(dry-run) touch {}", path.display());
        return;
    }
    if let Err(e) = std::fs::write(&path, b"") {
        tracing::info!("⚠️ Не записать отметку {}: {e}", path.display());
    }
}
