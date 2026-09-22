//! §2.5 / §2.6 — реализация задачи агентом.

use super::{
    blocked_on_partner_local, hand_to_human, nocommit_why, on_cancelled, partner, Ctx, HResult,
    Halt, HumanReason, Outcome, Task,
};
use crate::agent::AgentRequest;
use crate::config::DevMode;
use crate::gh;
use crate::markers;
use crate::prompts::{self, msg};
use crate::state::Stage;

pub async fn run(ctx: &Ctx, task: &mut Task) -> HResult<()> {
    match ctx.cfg.dev_mode {
        DevMode::Local => local(ctx, task).await,
        DevMode::GithubApp => app(ctx, task).await,
    }
}

async fn local(ctx: &Ctx, task: &mut Task) -> HResult<()> {
    task.set_stage(ctx, Stage::Implementing);

    let prompt = prompts::implement_local(
        task.issue,
        &task.title,
        &task.body,
        &ctx.cfg.base_branch,
        &task.block_hint,
    );
    let run = ctx
        .shutdown
        .guard(ctx.agent.run(AgentRequest {
            prompt,
            stdin: None,
            log_file: Some(ctx.log_dir().join(format!("issue-{}-impl.log", task.issue))),
        }))
        .await;
    let run = match run {
        Err(c) => return Err(on_cancelled(ctx, task, c).await),
        Ok(r) => r?,
    };

    if !run.success {
        // Claude не отработал (скорее всего — лимит подписки). Мягко
        // отступаем: задача остаётся в очереди до следующего круга.
        ctx.gh
            .issue_comment(task.issue, msg::agent_unavailable())
            .await?;
        ctx.git.checkout(&ctx.cfg.base_branch).await?;
        ctx.git.delete_branch(&task.branch).await?;
        tracing::info!("Claude недоступен — задача возвращена в очередь.");
        return Err(Halt::Stop(Outcome::AgentUnavailable { issue: task.issue }));
    }

    // Агент мог заблокировать задачу на партнёрский репозиторий.
    if ctx.cfg.partner_repo.is_some()
        && gh::issue_is_blocked(ctx.gh.as_ref(), task.issue, ctx.blocked_label()).await?
    {
        if task.guard {
            // Агент нарушил запрет — жёстко останавливаем пинг-понг.
            let outcome =
                hand_to_human(ctx, task, HumanReason::PingPongViolation { pr: None }).await?;
            ctx.git.checkout(&ctx.cfg.base_branch).await?;
            ctx.git.delete_branch(&task.branch).await?;
            return Err(Halt::Stop(outcome));
        }
        let blocker = partner::blocker_of(ctx, task.issue)
            .await
            .unwrap_or_else(|| ctx.cfg.partner_repo.clone().unwrap_or_default());
        let outcome = blocked_on_partner_local(ctx, task, blocker).await?;
        return Err(Halt::Stop(outcome));
    }

    // Агент обязан был что-то закоммитить.
    let ahead = ctx
        .git
        .commits_ahead(&format!("origin/{}", ctx.cfg.base_branch))
        .await?;
    if ahead == 0 {
        let why = nocommit_why(ctx, task);
        let outcome = hand_to_human(ctx, task, HumanReason::NoCommits { why }).await?;
        return Err(Halt::Stop(outcome));
    }
    Ok(())
}

async fn app(ctx: &Ctx, task: &mut Task) -> HResult<()> {
    let assigned_at = ctx.clock.now();
    task.state.assigned_at = Some(assigned_at);
    task.set_stage(ctx, Stage::WaitingApp);
    let mut verdict_no_pr = false;

    // Задача могла вернуться в очередь (сняли blocked, перезапустили юнит)
    // уже с готовым PR от прошлого прогона. Звать @claude второй раз —
    // холостой прогон Actions и лишние комментарии в issue ради того же
    // результата, поэтому сначала ищем существующий PR.
    task.pr = gh::find_task_pr(ctx.gh.as_ref(), task.issue).await?;

    if let Some(url) = task.pr.clone() {
        tracing::info!(
            "По задаче #{} уже открыт PR: {url} — @claude не зову.",
            task.issue
        );
    } else {
        ctx.gh
            .issue_comment(
                task.issue,
                &prompts::assign_to_app(task.issue, &ctx.cfg.base_branch, &task.block_hint),
            )
            .await?;
        tracing::info!(
            "Задача поручена @claude, жду появления PR (до {} мин)…",
            ctx.cfg.app_wait_min
        );

        let deadline = assigned_at + chrono::Duration::minutes(ctx.cfg.app_wait_min as i64);
        while ctx.clock.now() < deadline {
            if let Err(c) = ctx
                .shutdown
                .sleep(ctx.clock.as_ref(), std::time::Duration::from_secs(60))
                .await
            {
                return Err(on_cancelled(ctx, task, c).await);
            }
            if let Some(outcome) = partner::handle_signal(ctx, task, None, assigned_at).await? {
                return Err(Halt::Stop(outcome));
            }
            task.pr = gh::find_task_pr(ctx.gh.as_ref(), task.issue).await?;
            if task.pr.is_some() {
                break;
            }
            // **C3**: агент отчитался, но PR не открыл — ждать больше нечего.
            let comments = ctx.gh.issue_comments(task.issue).await?;
            if markers::claude_finished_since(&comments, assigned_at) {
                verdict_no_pr = true;
                break;
            }
        }
    }

    let Some(url) = task.pr.clone() else {
        let why = if verdict_no_pr {
            HumanReason::NoPrFromApp
        } else {
            HumanReason::AppTimeout
        };
        let outcome = hand_to_human(ctx, task, why).await?;
        return Err(Halt::Stop(outcome));
    };
    tracing::info!("PR от @claude: {url}");
    task.set_stage(ctx, Stage::WaitingCi);
    Ok(())
}
