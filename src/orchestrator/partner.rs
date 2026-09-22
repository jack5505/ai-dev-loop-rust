//! Сигналы агента про партнёрский репозиторий и защита от пинг-понга.
//!
//! `NEEDS-PARTNER` → сервер сам заводит задачу у партнёра и блокирует
//! эту. `CANNOT-FIX-HERE` (или `NEEDS-PARTNER` при включённой защите) →
//! человеку. В bash функция завершала весь скрипт; здесь она возвращает
//! исход, а решение «останавливаться» принимает вызывающий.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use super::{hand_to_human, Ctx, HumanReason, Outcome, Task};
use crate::markers;
use crate::prompts::{self, msg};

/// §2.4 — `PINGPONG_GUARD` и `BLOCK_HINT`.
pub async fn compute_guard(ctx: &Ctx, task: &mut Task) -> Result<()> {
    let Some(partner) = ctx.cfg.partner_repo.clone() else {
        return Ok(());
    };

    let comments = ctx
        .gh
        .issue_comments(task.issue)
        .await
        .with_context(|| format!("чтение комментариев #{}", task.issue))?;
    let joined = comments
        .iter()
        .map(|c| c.body.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let markers_count = markers::count_blocked_by_lines(&joined);

    task.guard = markers::has_origin_from(&task.body, &partner) || markers_count >= 2;
    task.block_hint = prompts::block_hint(
        Some(&partner),
        task.guard,
        ctx.cfg.dev_mode,
        &ctx.this_repo,
        task.issue,
    );
    Ok(())
}

/// Реакция на сигналы агента. `Ok(None)` — сигналов нет, работаем дальше.
pub async fn handle_signal(
    ctx: &Ctx,
    task: &Task,
    pr: Option<&str>,
    since: DateTime<Utc>,
) -> Result<Option<Outcome>> {
    let Some(partner) = ctx.cfg.partner_repo.clone() else {
        return Ok(None);
    };

    let comments = ctx.gh.issue_comments(task.issue).await?;

    // **C2**: маркер ловим только в комментариях агента, только в начале
    // строки и только после момента поручения.
    let cannot_fix_here =
        markers::find_agent_marker(&comments, "CANNOT-FIX-HERE:", &ctx.self_login, since);
    let needs_partner =
        markers::find_agent_marker(&comments, "NEEDS-PARTNER:", &ctx.self_login, since);

    if cannot_fix_here || (task.guard && needs_partner) {
        let outcome = hand_to_human(
            ctx,
            task,
            HumanReason::PingPongFromAgent {
                pr: pr.map(|s| s.to_string()),
            },
        )
        .await?;
        return Ok(Some(outcome));
    }

    if !needs_partner {
        return Ok(None);
    }

    // **C5**: подстраховка от дубля. Если задача с этим же ORIGIN у
    // партнёра уже заводилась — не создаём вторую. Открытую ждём,
    // закрытую считаем починенной и идём работать дальше.
    let twin = ctx
        .gh
        .search_issues_in_repo(
            &partner,
            &format!(
                "\"ORIGIN: {}#{}\" in:body sort:created-desc",
                ctx.this_repo, task.issue
            ),
            1,
        )
        .await
        .unwrap_or_default()
        .into_iter()
        .next();

    if let Some(twin) = twin {
        if twin.state.as_deref() == Some("CLOSED") {
            tracing::info!(
                "У партнёра уже есть закрытая задача #{} с этим ORIGIN — блокировку не ставлю.",
                twin.number
            );
            return Ok(None);
        }
        let blocker = format!("{partner}#{}", twin.number);
        ctx.gh
            .issue_comment(task.issue, &format!("BLOCKED-BY: {blocker}"))
            .await?;
        ctx.gh
            .issue_edit_labels(task.issue, &[ctx.blocked_label()], &[])
            .await?;
        if let Some(url) = pr {
            ctx.gh
                .pr_close(url, &msg::pr_closed_partner_existing(&partner))
                .await?;
        }
        ctx.tg(&msg::tg_blocked_on_existing_twin(
            task.issue,
            &task.title,
            &partner,
            twin.number,
        ))
        .await;
        tracing::info!("Задача #{} ждёт существующую {blocker}. Стоп.", task.issue);
        return Ok(Some(Outcome::BlockedOnPartner {
            issue: task.issue,
            blocker,
        }));
    }

    // Новая задача у партнёра. Первая строка тела — маркер ORIGIN,
    // по нему работает дедуп выше.
    let details = {
        let joined = comments
            .iter()
            .filter(|c| c.created_at.map(|at| at >= since).unwrap_or(false))
            .map(|c| c.body.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        markers::needs_partner_details(&joined)
    };
    let new_url = ctx
        .gh
        .issue_create(
            &partner,
            &["ai-task"],
            &format!("Из {}#{}: {}", ctx.this_repo, task.issue, task.title),
            &format!("ORIGIN: {}#{}\n\n{details}", ctx.this_repo, task.issue),
        )
        .await?;
    let new_number = new_url.rsplit('/').next().unwrap_or_default().to_string();
    let blocker = format!("{partner}#{new_number}");

    ctx.gh
        .issue_comment(task.issue, &format!("BLOCKED-BY: {blocker}"))
        .await?;
    ctx.gh
        .issue_edit_labels(task.issue, &[ctx.blocked_label()], &[])
        .await?;
    if let Some(url) = pr {
        ctx.gh
            .pr_close(url, &msg::pr_closed_partner_new(&partner))
            .await?;
    }
    ctx.tg(&msg::tg_blocked_on_partner_short(
        task.issue,
        &task.title,
        &partner,
    ))
    .await;
    tracing::info!("Задача #{} ждёт {partner}. Стоп.", task.issue);
    Ok(Some(Outcome::BlockedOnPartner {
        issue: task.issue,
        blocker,
    }))
}

/// Задача оказалась заблокирована агентом (метка `blocked` появилась сама).
/// Возвращает маркер блокера, если он найден.
pub async fn blocker_of(ctx: &Ctx, issue: u64) -> Option<String> {
    let full = ctx.gh.issue_body_and_comments(issue).await.ok()?;
    let mut joined = String::from(full.body_str());
    for c in full.comments.as_deref().unwrap_or(&[]) {
        joined.push('\n');
        joined.push_str(&c.body);
    }
    markers::last_blocked_by(&joined)
}
