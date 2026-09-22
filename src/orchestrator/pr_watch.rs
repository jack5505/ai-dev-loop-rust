//! §2.2 — сторож открытых PR.
//!
//! Итерация заканчивается снятием метки очереди: задача уходит, а
//! открытый PR после этого не сторожит никто. К 2026-09-19 так
//! накопилось 13 открытых PR, пять из них `CONFLICTING`, и цикл об этом
//! не знал. Проход только ДОКЛАДЫВАЕТ: сам ничего не чинит и минуты
//! Actions не жжёт.

use anyhow::Result;
use chrono::Duration;

use super::Ctx;
use crate::gh::model::Mergeable;
use crate::markers;
use crate::prompts::msg;

pub async fn run(ctx: &Ctx) -> Result<()> {
    let stale_before = ctx.clock.now() - Duration::days(ctx.cfg.pr_stale_days);
    let prs = match ctx.gh.pr_list_open(100).await {
        Ok(prs) => prs,
        Err(e) => {
            tracing::info!("⚠️ Не удалось получить список открытых PR: {e:#}");
            return Ok(());
        }
    };

    let mut report = String::new();
    for pr in prs {
        if !markers::body_refers_to_task(pr.body_str()) {
            continue;
        }
        if pr.mergeable_state() == Mergeable::Conflicting {
            report.push_str(&format!(
                "  #{} — конфликт с {}, нужен ребейз\n",
                pr.number, ctx.cfg.base_branch
            ));
        } else if let Some(updated) = pr.updated_at {
            if updated < stale_before {
                report.push_str(&format!(
                    "  #{} — без движения с {}\n",
                    pr.number,
                    updated.format("%Y-%m-%d")
                ));
            }
        }
    }

    if report.is_empty() {
        return Ok(());
    }
    tracing::info!("Открытые AI-PR, требующие внимания:\n{report}");

    // Telegram — не чаще раза в сутки, иначе сводка придёт каждые 15 минут.
    let stamp = ctx.log_dir().join(format!(
        ".pr-watch-{}",
        chrono::Local::now().format("%Y-%m-%d")
    ));
    if stamp.exists() {
        return Ok(());
    }
    if let Err(e) = std::fs::write(&stamp, b"") {
        tracing::info!("⚠️ Не записать отметку {}: {e}", stamp.display());
    }
    ctx.tg(&msg::tg_pr_watch(&ctx.cfg.repo_basename(), &report))
        .await;
    Ok(())
}
