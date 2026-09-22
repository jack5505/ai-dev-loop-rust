//! §2.7 — цикл ожидания CI и починки красного.

use super::{hand_to_human, on_cancelled, partner, Ctx, HResult, Halt, HumanReason, Outcome, Task};
use crate::agent::AgentRequest;
use crate::config::DevMode;
use crate::gh;
use crate::markers;
use crate::prompts::{self, msg};
use crate::state::Stage;

/// Сколько строк хвоста лога упавших шагов отдаём агенту.
const FAIL_TAIL_LINES: usize = 150;

pub async fn run(ctx: &Ctx, task: &mut Task) -> HResult<()> {
    if ctx.cfg.dev_mode == DevMode::Local {
        ctx.git.push_new_branch(&task.branch).await?;
        let url = ctx
            .gh
            .pr_create_draft(
                &ctx.cfg.base_branch,
                &task.branch,
                &format!("AI: {}", task.title),
                &prompts::pr_body(task.issue),
            )
            .await?;
        tracing::info!("Draft-PR создан: {url}");
        task.pr = Some(url);
    }
    task.set_stage(ctx, Stage::WaitingCi);

    let pr = task
        .pr
        .clone()
        .ok_or_else(|| anyhow::anyhow!("внутренняя ошибка: PR неизвестен перед ожиданием CI"))?;

    // Ветку для `gh run list` берём у самого PR: в режиме github-app её
    // выбирает @claude, и запрос по `ai/issue-N` не нашёл бы ни одного
    // run'а — агент получал «(не удалось скачать лог CI)» вместо лога.
    let ci_branch = match ctx.gh.pr_head_branch(&pr).await {
        Ok(b) if !b.is_empty() => b,
        _ => task.branch.clone(),
    };

    let mut success = false;
    for attempt in 1..=ctx.cfg.max_iterations {
        tracing::info!(
            "Жду результаты CI (попытка {attempt} из {})…",
            ctx.cfg.max_iterations
        );
        // Даём Actions время создать run.
        if let Err(c) = ctx
            .shutdown
            .sleep(
                ctx.clock.as_ref(),
                std::time::Duration::from_secs(ctx.cfg.ci_start_wait),
            )
            .await
        {
            return Err(on_cancelled(ctx, task, c).await);
        }

        let green = match ctx.shutdown.guard(ctx.gh.pr_checks_watch(&pr)).await {
            Err(c) => return Err(on_cancelled(ctx, task, c).await),
            Ok(r) => r?,
        };
        if green {
            success = true;
            tracing::info!("CI зелёный ✅");
            break;
        }

        tracing::info!("CI красный — забираю лог упавших шагов");
        let fail_tail = fail_tail(ctx, &ci_branch).await;
        let path = ctx
            .log_dir()
            .join(format!("issue-{}-ci-fail-{attempt}.log", task.issue));
        if let Err(e) = std::fs::write(&path, &fail_tail) {
            tracing::info!("⚠️ Не записать {}: {e}", path.display());
        }

        // Последняя попытка исчерпана — чинить больше не даём.
        if attempt == ctx.cfg.max_iterations {
            break;
        }

        match ctx.cfg.dev_mode {
            DevMode::Local => {
                let prompt = prompts::ci_fix_local(
                    attempt,
                    ctx.cfg.max_iterations,
                    &fail_tail,
                    &task.block_hint,
                );
                let run = ctx
                    .shutdown
                    .guard(
                        ctx.agent.run(AgentRequest {
                            prompt,
                            stdin: None,
                            log_file: Some(
                                ctx.log_dir()
                                    .join(format!("issue-{}-fix-{attempt}.log", task.issue)),
                            ),
                        }),
                    )
                    .await;
                match run {
                    Err(c) => return Err(on_cancelled(ctx, task, c).await),
                    Ok(Ok(r)) if !r.success => {
                        tracing::info!("⚠️ claude завершился с ошибкой, идём дальше")
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => tracing::info!("⚠️ claude завершился с ошибкой: {e:#}"),
                }

                // Агент решил, что причина на стороне партнёрского репозитория.
                if ctx.cfg.partner_repo.is_some()
                    && gh::issue_is_blocked(ctx.gh.as_ref(), task.issue, ctx.blocked_label())
                        .await?
                {
                    if task.guard {
                        let outcome = hand_to_human(
                            ctx,
                            task,
                            HumanReason::PingPongViolation {
                                pr: Some(pr.clone()),
                            },
                        )
                        .await?;
                        return Err(Halt::Stop(outcome));
                    }
                    let partner = ctx.cfg.partner_repo.clone().unwrap_or_default();
                    ctx.gh
                        .pr_close(&pr, &msg::pr_closed_partner_after_ci(&partner, task.issue))
                        .await?;
                    ctx.tg(&msg::tg_blocked_on_partner(
                        task.issue,
                        &task.title,
                        &partner,
                    ))
                    .await;
                    tracing::info!("Задача #{} ждёт {partner}. Стоп.", task.issue);
                    let blocker = partner::blocker_of(ctx, task.issue)
                        .await
                        .unwrap_or(partner);
                    return Err(Halt::Stop(Outcome::BlockedOnPartner {
                        issue: task.issue,
                        blocker,
                    }));
                }

                ctx.git.push_force_with_lease().await?;
            }
            DevMode::GithubApp => {
                let old_sha = ctx.gh.pr_head_sha(&pr).await?;
                ctx.gh
                    .pr_comment(
                        &pr,
                        &prompts::ci_fix_app(
                            attempt,
                            ctx.cfg.max_iterations,
                            &fail_tail,
                            &task.block_hint,
                        ),
                    )
                    .await?;
                tracing::info!("Жду фикс от @claude (до {} мин)…", ctx.cfg.app_wait_min);

                let asked_at = ctx.clock.now();
                task.state.asked_at = Some(asked_at);
                task.set_stage(ctx, Stage::WaitingCi);
                let deadline = asked_at + chrono::Duration::minutes(ctx.cfg.app_wait_min as i64);
                let mut fixed = false;
                while ctx.clock.now() < deadline {
                    if let Err(c) = ctx
                        .shutdown
                        .sleep(ctx.clock.as_ref(), std::time::Duration::from_secs(60))
                        .await
                    {
                        return Err(on_cancelled(ctx, task, c).await);
                    }
                    if let Some(outcome) =
                        partner::handle_signal(ctx, task, Some(&pr), asked_at).await?
                    {
                        return Err(Halt::Stop(outcome));
                    }
                    if ctx.gh.pr_head_sha(&pr).await? != old_sha {
                        fixed = true;
                        break;
                    }
                    // Агент отчитался, но коммита нет — сам он уже не запушит.
                    let comments = ctx.gh.pr_comments(&pr).await?;
                    if markers::claude_finished_since(&comments, asked_at) {
                        tracing::info!(
                            "@claude завершил работу, но коммит не запушил — передаю человеку."
                        );
                        break;
                    }
                }
                if !fixed {
                    if ctx.clock.now() >= deadline {
                        tracing::info!(
                            "Фикс от @claude не пришёл за {} мин — передаю человеку.",
                            ctx.cfg.app_wait_min
                        );
                    }
                    break;
                }
            }
        }
    }

    // §4a. Не справился → зовём человека.
    if !success {
        let outcome = hand_to_human(ctx, task, HumanReason::CiStillRed { pr }).await?;
        return Err(Halt::Stop(outcome));
    }
    Ok(())
}

/// Хвост лога упавших шагов. Не смогли скачать — отдаём агенту честную
/// заглушку, как это делал bash.
async fn fail_tail(ctx: &Ctx, branch: &str) -> String {
    let fallback = "(не удалось скачать лог CI)".to_string();
    let run_id = match ctx.gh.latest_run_id(branch).await {
        Ok(Some(id)) => id,
        Ok(None) => return fallback,
        Err(e) => {
            tracing::info!("⚠️ Не найти последний run для {branch}: {e:#}");
            return fallback;
        }
    };
    let log = match ctx.gh.run_log_failed(&run_id).await {
        Ok(log) => log,
        Err(e) => {
            tracing::info!("⚠️ Не скачать лог run {run_id}: {e:#}");
            return fallback;
        }
    };
    let lines: Vec<&str> = log.lines().collect();
    let start = lines.len().saturating_sub(FAIL_TAIL_LINES);
    lines[start..].join("\n")
}
