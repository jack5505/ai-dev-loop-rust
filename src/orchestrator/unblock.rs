//! §2.1 — межрепозиторная разблокировка.
//!
//! Метка `blocked` = задача ждёт починки в другом репозитории. Маркер
//! в комментарии: `BLOCKED-BY: owner/repo#123`. Каждый круг проверяем:
//! блокер закрыт → снимаем метку, задача сама возвращается в очередь.

use anyhow::Result;

use super::{hand_to_human, Ctx, HumanReason, Task};
use crate::markers;
use crate::prompts::msg;
use crate::state::IterationState;

pub async fn run(ctx: &Ctx) -> Result<()> {
    if ctx.cfg.partner_repo.is_none() {
        return Ok(());
    }

    let issues = ctx.gh.issues_with_label(ctx.blocked_label()).await?;
    for issue in issues {
        let full = match ctx.gh.issue_body_and_comments(issue.number).await {
            Ok(full) => full,
            Err(e) => {
                tracing::info!("⚠️ Не прочитать #{}: {e:#}", issue.number);
                continue;
            }
        };
        let mut joined = String::from(full.body_str());
        for c in full.comments.as_deref().unwrap_or(&[]) {
            joined.push('\n');
            joined.push_str(&c.body);
        }

        let Some(reference) = markers::last_blocked_by(&joined) else {
            // **C6**. Раньше здесь был молчаливый `continue`, и такая задача
            // пропадала навсегда: из очереди исключена, в needs-human не
            // попадает, ни в один алерт не приходит. К 2026-09-19 так
            // накопилось 10 штук начиная с 03.09.
            let task = Task {
                issue: issue.number,
                title: issue.title.clone(),
                body: String::new(),
                branch: String::new(),
                guard: false,
                block_hint: String::new(),
                pr: None,
                state: IterationState::new(issue.number, ctx.clock.now()),
            };
            hand_to_human(ctx, &task, HumanReason::BlockedWithoutMarker).await?;
            continue;
        };

        let Some((repo, blocker)) = markers::split_ref(&reference) else {
            tracing::info!(
                "⚠️ Маркер «{reference}» не разобрать — пропускаю #{}",
                issue.number
            );
            continue;
        };
        let state = ctx
            .gh
            .issue_state(Some(repo), blocker)
            .await
            .unwrap_or_else(|_| "UNKNOWN".to_string());
        if state == "CLOSED" {
            ctx.gh
                .issue_edit_labels(issue.number, &[], &[ctx.blocked_label()])
                .await?;
            ctx.gh
                .issue_comment(issue.number, &msg::unblocked(&reference))
                .await?;
            tracing::info!("Разблокировал issue #{} (ждал {reference})", issue.number);
        }
    }
    Ok(())
}
