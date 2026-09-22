//! §2.8 и §5 — ревью, мержабельность, merge.

use super::{
    drop_task_label, hand_to_human, on_cancelled, partner, Ctx, HResult, Halt, HumanReason,
    Outcome, Task,
};
use crate::agent::AgentRequest;
use crate::config::DevMode;
use crate::gh;
use crate::gh::model::{merge_state_is_conflict, Mergeable};
use crate::markers;
use crate::prompts::{self, msg};
use crate::state::Stage;

pub async fn run(ctx: &Ctx, task: &mut Task) -> HResult<Outcome> {
    let pr = task
        .pr
        .clone()
        .ok_or_else(|| anyhow::anyhow!("внутренняя ошибка: PR неизвестен перед ревью"))?;
    task.set_stage(ctx, Stage::Reviewing);

    // PR от App может быть уже не draft — ошибку глушим.
    if let Err(e) = ctx.gh.pr_ready(&pr).await {
        tracing::info!("(PR уже не draft: {})", first_line(&format!("{e:#}")));
    }

    // **C7**: mergeable считается лениво, поэтому три попытки.
    let mut merge_state = gh::pr_mergeable(ctx.gh.as_ref(), ctx.clock.as_ref(), &pr).await;
    if merge_state == Mergeable::Conflicting {
        tracing::info!(
            "PR конфликтует с {} — прошу агента подтянуть ветку.",
            ctx.cfg.base_branch
        );
        if rework(
            ctx,
            task,
            &pr,
            &prompts::rework_conflict(&ctx.cfg.base_branch),
        )
        .await?
        {
            merge_state = gh::pr_mergeable(ctx.gh.as_ref(), ctx.clock.as_ref(), &pr).await;
        }
        if merge_state == Mergeable::Conflicting {
            let outcome = hand_to_human(ctx, task, HumanReason::Conflict { pr }).await?;
            return Err(Halt::Stop(outcome));
        }
    }

    // ─── Дожим ревью ────────────────────────────────────────────────
    let max = ctx.cfg.max_review_rounds;
    let mut round: u32 = 1;
    let mut review_ok = false;
    let mut rework_why =
        format!("авто-ревью осталось при `REQUEST_CHANGES` после {max} круга доработки");

    loop {
        let diff = ctx.gh.pr_diff(&pr).await?;
        let run = ctx
            .shutdown
            .guard(ctx.agent.run(AgentRequest {
                prompt: prompts::review(&ctx.cfg.base_branch),
                stdin: Some(diff),
                log_file: None,
            }))
            .await;
        let review = match run {
            Err(c) => return Err(on_cancelled(ctx, task, c).await),
            Ok(r) => r?.output,
        };

        ctx.gh
            .pr_comment(&pr, &prompts::review_comment(round, max, &review))
            .await?;

        if markers::review_approved(&review) {
            review_ok = true;
            tracing::info!("Ревью пройдено на круге {round} ✅");
            break;
        }
        if round >= max {
            tracing::info!("Ревью не пройдено за {max} круга — передаю человеку.");
            break;
        }

        tracing::info!("Ревью вернуло REQUEST_CHANGES (круг {round}) — отдаю замечания агенту.");
        if !rework(
            ctx,
            task,
            &pr,
            &prompts::rework_review(round, max, &ctx.cfg.base_branch),
        )
        .await?
        {
            rework_why = "агент не доработал PR по замечаниям ревью".to_string();
            tracing::info!("Доработки по ревью не случилось — передаю человеку.");
            break;
        }

        // Доработка могла сломать сборку — до следующего круга перепроверяем.
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
        if !green {
            rework_why = "после доработки по замечаниям ревью CI стал красным".to_string();
            tracing::info!("После доработки CI покраснел — передаю человеку.");
            break;
        }
        round += 1;
    }

    // ═══ 5. Merge ═══════════════════════════════════════════════════
    // `mergeStateStatus` проверяем ДО авто-merge: `--auto` просто ставит PR
    // в очередь на слияние, когда чеки позеленеют, и молчит про уже
    // существующий конфликт — такой PR завис бы незамеченным. Более поздний
    // конфликт ловит проход sweep в начале следующего круга.
    let status = ctx
        .gh
        .pr_merge_state(&pr)
        .await
        .unwrap_or_else(|_| "UNKNOWN".to_string());
    let conflict = merge_state_is_conflict(&status);

    let outcome = if review_ok {
        if conflict {
            hand_to_human(
                ctx,
                task,
                HumanReason::ApprovedButConflicting { pr: pr.clone() },
            )
            .await?
        } else if ctx.cfg.auto_merge {
            ctx.gh.pr_merge_squash_auto(&pr).await?;
            ctx.gh
                .issue_comment(task.issue, &msg::auto_merge_queued(&pr))
                .await?;
            ctx.tg(&msg::tg_auto_merge_queued(task.issue, &task.title, &pr))
                .await;
            tracing::info!("Авто-merge включён для {pr}");
            Outcome::Merged {
                issue: task.issue,
                pr: pr.clone(),
            }
        } else {
            ctx.gh
                .issue_comment(task.issue, &msg::pr_awaiting_human(&pr))
                .await?;
            ctx.tg(&msg::tg_pr_awaiting_human(task.issue, &task.title, &pr))
                .await;
            tracing::info!("PR ждёт человека: {pr}");
            Outcome::PrAwaitingReview {
                issue: task.issue,
                pr: pr.clone(),
            }
        }
    } else {
        // Раньше это тонуло в том же нейтральном «глянь, когда будет
        // минутка», что и обычный approve без авто-merge, и без метки —
        // PR мог зависнуть незамеченным.
        hand_to_human(
            ctx,
            task,
            HumanReason::ReviewRequestChanges {
                pr: pr.clone(),
                why: rework_why,
            },
        )
        .await?
    };

    drop_task_label(ctx, task.issue).await?;
    Ok(outcome)
}

/// Просим агента доработать PR и ждём новый коммит в той же ветке.
/// `Ok(true)` — коммит появился.
async fn rework(ctx: &Ctx, task: &mut Task, pr: &str, instruction: &str) -> HResult<bool> {
    let old_sha = ctx.gh.pr_head_sha(pr).await?;
    let prompt = format!("{instruction}{}", task.block_hint);

    if ctx.cfg.dev_mode == DevMode::Local {
        let run = ctx
            .shutdown
            .guard(
                ctx.agent.run(AgentRequest {
                    prompt,
                    stdin: None,
                    log_file: Some(
                        ctx.log_dir()
                            .join(format!("issue-{}-rework.log", task.issue)),
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
        if ctx.cfg.partner_repo.is_some()
            && gh::issue_is_blocked(ctx.gh.as_ref(), task.issue, ctx.blocked_label()).await?
        {
            return Ok(false);
        }
        if let Err(e) = ctx.git.push_force_with_lease().await {
            tracing::info!("⚠️ push не удался: {e:#}");
            return Ok(false);
        }
        return Ok(ctx.gh.pr_head_sha(pr).await? != old_sha);
    }

    // github-app: просим фикс комментарием и ждём смены headRefOid.
    ctx.gh.pr_comment(pr, &format!("@claude {prompt}")).await?;
    let asked_at = ctx.clock.now();
    task.state.asked_at = Some(asked_at);
    task.set_stage(ctx, Stage::Reviewing);
    let deadline = asked_at + chrono::Duration::minutes(ctx.cfg.app_wait_min as i64);

    while ctx.clock.now() < deadline {
        if let Err(c) = ctx
            .shutdown
            .sleep(ctx.clock.as_ref(), std::time::Duration::from_secs(60))
            .await
        {
            return Err(on_cancelled(ctx, task, c).await);
        }
        if let Some(outcome) = partner::handle_signal(ctx, task, Some(pr), asked_at).await? {
            return Err(Halt::Stop(outcome));
        }
        if ctx.gh.pr_head_sha(pr).await? != old_sha {
            return Ok(true);
        }
        let comments = ctx.gh.pr_comments(pr).await?;
        if markers::claude_finished_since(&comments, asked_at) {
            tracing::info!("@claude отчитался, но коммита нет.");
            return Ok(false);
        }
    }
    tracing::info!(
        "Ответ от @claude не пришёл за {} мин.",
        ctx.cfg.app_wait_min
    );
    Ok(false)
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}
