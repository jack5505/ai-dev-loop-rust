//! Зависшие авто-merge: конфликт возник ПОСЛЕ постановки PR в очередь.
//!
//! `gh pr merge --auto` просто ставит PR в очередь на слияние, когда
//! позеленеют чеки. Если после этого в базовую ветку прилетел другой
//! коммит и вызвал конфликт, PR тихо висит вечно: процесс итерации уже
//! завершился, метка очереди снята, и сам оркестратор к этой задаче
//! больше не вернётся. Поэтому каждый круг перепроверяем все открытые PR
//! наших веток — до того как браться за следующую задачу.

use anyhow::Result;

use super::Ctx;
use crate::gh::model::merge_state_is_conflict;
use crate::markers;
use crate::prompts::msg;

pub async fn run(ctx: &Ctx) -> Result<()> {
    let prs = match ctx.gh.pr_list_open(100).await {
        Ok(prs) => prs,
        Err(e) => {
            tracing::info!("⚠️ Не удалось получить список открытых PR: {e:#}");
            return Ok(());
        }
    };

    for pr in prs {
        let Some(branch) = pr.head_ref_name.as_deref() else {
            continue;
        };
        if !branch.starts_with("ai/issue-") {
            continue;
        }
        if !pr.auto_merge_queued() {
            continue;
        }
        let state = pr.merge_state_status.as_deref().unwrap_or("");
        if !merge_state_is_conflict(state) {
            continue;
        }
        let Some(issue) = markers::task_number_in_body(pr.body_str()) else {
            continue;
        };
        // Уже у человека — второй раз не трогаем.
        match ctx.gh.issue_labels(issue).await {
            Ok(labels) if labels.iter().any(|l| l == ctx.human_label()) => continue,
            Ok(_) => {}
            Err(e) => {
                tracing::info!("⚠️ Не прочитать метки #{issue}: {e:#}");
                continue;
            }
        }

        // Дальше каждое действие — как `|| true` в bash: сбой одного шага
        // не должен ронять весь проход.
        if let Err(e) = ctx.gh.pr_disable_auto_merge(&pr.url).await {
            tracing::info!("⚠️ Не снять авто-merge с {}: {e:#}", pr.url);
        }
        if let Err(e) = ctx
            .gh
            .issue_edit_labels(issue, &[ctx.human_label()], &[])
            .await
        {
            tracing::info!("⚠️ Не поставить метку на #{issue}: {e:#}");
        }
        if let Err(e) = ctx
            .gh
            .pr_comment(&pr.url, &msg::stuck_merge_pr(&ctx.cfg.base_branch))
            .await
        {
            tracing::info!("⚠️ Не написать в {}: {e:#}", pr.url);
        }
        if let Err(e) = ctx
            .gh
            .issue_comment(
                issue,
                &msg::stuck_merge_issue(&pr.url, &ctx.cfg.base_branch),
            )
            .await
        {
            tracing::info!("⚠️ Не написать в #{issue}: {e:#}");
        }
        ctx.tg(&msg::tg_stuck_merge(&pr.url, issue)).await;
        tracing::info!(
            "Обнаружил зависший конфликт: {} — передал человеку.",
            pr.url
        );
    }
    Ok(())
}
