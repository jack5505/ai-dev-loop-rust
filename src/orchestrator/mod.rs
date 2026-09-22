//! Машина состояний итерации.
//!
//! Bash-скрипт линеен: каждый выход — `exit 0` посреди функции, а SIGTERM
//! в любой из петель `sleep 60` убивал процесс до простановки меток.
//! Здесь исход итерации — значение [`Outcome`], у каждого ровно один
//! хендлер, и тексты живут в одном месте, а не в девяти.

pub mod ci;
pub mod implement;
pub mod partner;
pub mod pr_watch;
pub mod review;
pub mod sweep;
pub mod unblock;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::agent::Agent;
use crate::clock::Clock;
use crate::config::{Config, DevMode};
use crate::gh::GitHub;
use crate::git::Vcs;
use crate::notify::Notifier;
use crate::prompts::msg;
use crate::queue;
use crate::shutdown::{Cancelled, Shutdown};
use crate::state::{IterationState, Stage};

/// Чем закончилась итерация. Ни один из вариантов, кроме аварии, не
/// является ошибкой: код возврата 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Очередь пуста — нечего делать.
    QueueEmpty,
    /// Другой запуск ещё работает (**C10**).
    LockBusy,
    /// Агент недоступен: задача мягко возвращается в очередь.
    AgentUnavailable { issue: u64 },
    /// Ждём починки в партнёрском репозитории.
    BlockedOnPartner { issue: u64, blocker: String },
    /// Дальше нужен человек.
    HandedToHuman { issue: u64, why: HumanReason },
    /// PR готов и ждёт решения человека (`AUTO_MERGE=false`).
    PrAwaitingReview { issue: u64, pr: String },
    /// PR поставлен на авто-merge.
    Merged { issue: u64, pr: String },
}

impl Outcome {
    pub fn issue(&self) -> Option<u64> {
        match self {
            Outcome::QueueEmpty | Outcome::LockBusy => None,
            Outcome::AgentUnavailable { issue }
            | Outcome::BlockedOnPartner { issue, .. }
            | Outcome::HandedToHuman { issue, .. }
            | Outcome::PrAwaitingReview { issue, .. }
            | Outcome::Merged { issue, .. } => Some(*issue),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Outcome::QueueEmpty => "queue-empty",
            Outcome::LockBusy => "lock-busy",
            Outcome::AgentUnavailable { .. } => "agent-unavailable",
            Outcome::BlockedOnPartner { .. } => "blocked-on-partner",
            Outcome::HandedToHuman { .. } => "handed-to-human",
            Outcome::PrAwaitingReview { .. } => "pr-awaiting-review",
            Outcome::Merged { .. } => "merged",
        }
    }
}

/// Почему задача ушла человеку. В bash это были восемь разных `exit 0`
/// с текстами по месту.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanReason {
    /// **C6**: метка `blocked` без маркера `BLOCKED-BY`.
    BlockedWithoutMarker,
    /// Агент сам сказал «чинить нужно не здесь», а встречная блокировка
    /// запрещена.
    PingPongFromAgent {
        pr: Option<String>,
    },
    /// Агент нарушил запрет и всё-таки поставил `blocked`.
    PingPongViolation {
        pr: Option<String>,
    },
    NoCommits {
        why: String,
    },
    NoPrFromApp,
    AppTimeout,
    CiStillRed {
        pr: String,
    },
    Conflict {
        pr: String,
    },
    ReviewRequestChanges {
        pr: String,
        why: String,
    },
    ApprovedButConflicting {
        pr: String,
    },
    /// Итерацию остановили сигналом.
    Terminated,
    /// Авария с человекочитаемой цепочкой контекста вместо `$LINENO`.
    OrchestratorCrash {
        context: String,
    },
}

/// Досрочный выход: либо готовый исход (аналог `exit 0` в bash), либо
/// настоящая ошибка.
pub enum Halt {
    Stop(Outcome),
    Error(anyhow::Error),
}

impl From<anyhow::Error> for Halt {
    fn from(e: anyhow::Error) -> Self {
        Halt::Error(e)
    }
}

pub type HResult<T> = Result<T, Halt>;

/// Внешний мир итерации.
pub struct Ctx {
    pub cfg: Config,
    pub gh: Arc<dyn GitHub>,
    pub agent: Arc<dyn Agent>,
    pub notify: Arc<dyn Notifier>,
    pub clock: Arc<dyn Clock>,
    pub git: Arc<dyn Vcs>,
    pub shutdown: Shutdown,
    pub self_login: String,
    /// `owner/name` этого репозитория; пусто, если `PARTNER_REPO` не задан
    /// (bash тоже спрашивал его только в этом случае).
    pub this_repo: String,
    pub dry_run: bool,
}

impl Ctx {
    pub fn log_dir(&self) -> PathBuf {
        self.cfg.log_dir()
    }

    pub async fn tg(&self, text: &str) {
        self.notify.send(text).await;
    }

    pub fn human_label(&self) -> &str {
        &self.cfg.human_label
    }

    pub fn blocked_label(&self) -> &str {
        &self.cfg.blocked_label
    }
}

/// Задача, взятая в работу.
#[derive(Debug, Clone)]
pub struct Task {
    pub issue: u64,
    pub title: String,
    pub body: String,
    pub branch: String,
    /// `PINGPONG_GUARD`: задача пришла из партнёрского репо или уже
    /// блокировалась дважды.
    pub guard: bool,
    /// Хвост, дописываемый в каждый промпт агента.
    pub block_hint: String,
    pub pr: Option<String>,
    pub state: IterationState,
}

impl Task {
    pub fn set_stage(&mut self, ctx: &Ctx, stage: Stage) {
        self.state.stage = stage;
        self.state.pr = self.pr.clone();
        self.state.updated_at = ctx.clock.now();
        if let Err(e) = self.state.save(&ctx.log_dir()) {
            tracing::info!("⚠️ Не удалось записать состояние итерации: {e:#}");
        }
    }
}

/// Одна итерация целиком (§2).
pub async fn run(ctx: &Ctx, issue_override: Option<u64>) -> Result<Outcome> {
    let mut current: Option<(u64, String)> = None;
    let result = iteration(ctx, issue_override, &mut current).await;
    let outcome = match result {
        Ok(o) => o,
        Err(Halt::Stop(o)) => o,
        Err(Halt::Error(e)) => {
            // Аналог `trap ERR + on_error`, только с цепочкой контекста
            // вместо номера строки.
            let context = format!("{e:#}");
            tracing::info!("❌ Ошибка: {context}");
            if let Some((issue, _)) = &current {
                if let Err(e2) = ctx
                    .gh
                    .issue_edit_labels(*issue, &[ctx.human_label()], &[])
                    .await
                {
                    tracing::info!("⚠️ Не удалось поставить метку: {e2:#}");
                }
                if let Err(e2) = ctx
                    .gh
                    .issue_comment(*issue, &msg::crash_issue(&context, *issue))
                    .await
                {
                    tracing::info!("⚠️ Не удалось написать комментарий: {e2:#}");
                }
            }
            ctx.tg(&msg::tg_crash(&context, current.as_ref().map(|(n, _)| *n)))
                .await;
            return Err(e);
        }
    };
    if let Some(issue) = outcome.issue() {
        if let Some(mut state) = IterationState::load(&ctx.log_dir(), issue) {
            state.stage = Stage::Finished;
            state.outcome = Some(outcome.label().to_string());
            state.updated_at = ctx.clock.now();
            let _ = state.save(&ctx.log_dir());
        }
    }
    Ok(outcome)
}

async fn iteration(
    ctx: &Ctx,
    issue_override: Option<u64>,
    current: &mut Option<(u64, String)>,
) -> HResult<Outcome> {
    std::fs::create_dir_all(ctx.log_dir())
        .with_context(|| format!("создание {}", ctx.log_dir().display()))?;

    // ═══ 1. Следующая задача из очереди ═════════════════════════════
    ctx.git
        .sync_base(&ctx.cfg.base_branch)
        .await
        .context("синхронизация клона с базовой веткой")?;

    // Сначала — PR, чей авто-merge завис из-за конфликта, возникшего уже
    // после постановки в очередь.
    sweep::run(ctx)
        .await
        .context("проход по зависшим авто-merge")?;
    // Затем возвращаем в очередь задачи, чей блокер в соседнем репо закрыт.
    unblock::run(ctx).await.context("разблокировка задач")?;
    // И докладываем про открытые PR, которые зависли без внимания.
    pr_watch::run(ctx).await.context("сторож открытых PR")?;

    let authors = queue::authors(&ctx.cfg, &ctx.self_login);
    let issue = match issue_override {
        Some(num) => Some(
            ctx.gh
                .issue_view(num)
                .await
                .with_context(|| format!("чтение задачи #{num}"))?,
        ),
        None => queue::head(ctx.gh.as_ref(), &ctx.cfg, &authors)
            .await
            .context("запрос очереди")?,
    };
    let Some(issue) = issue else {
        tracing::info!("Очередь пуста — нечего делать. ✅");
        return Ok(Outcome::QueueEmpty);
    };

    let mut task = Task {
        issue: issue.number,
        title: issue.title.clone(),
        body: issue.body_str().to_string(),
        branch: format!("ai/issue-{}", issue.number),
        guard: false,
        block_hint: String::new(),
        pr: None,
        state: IterationState::new(issue.number, ctx.clock.now()),
    };
    *current = Some((task.issue, task.title.clone()));
    tracing::info!(
        "Задача: #{} — {} (режим: {})",
        task.issue,
        task.title,
        ctx.cfg.dev_mode.as_str()
    );
    task.set_stage(ctx, Stage::Taken);

    if ctx.cfg.dev_mode == DevMode::Local {
        ctx.git.checkout_new_branch(&task.branch).await?;
    }
    ctx.gh.issue_comment(task.issue, msg::taken()).await?;

    // ═══ 2.4. Защита от пинг-понга ══════════════════════════════════
    partner::compute_guard(ctx, &mut task).await?;

    // ═══ 2.5 / 2.6. Реализация ══════════════════════════════════════
    implement::run(ctx, &mut task).await?;

    // ═══ 3. PR → цикл: ждём CI, чиним, снова ждём ═══════════════════
    ci::run(ctx, &mut task).await?;

    // ═══ 4b, 5. Ревью и merge ══════════════════════════════════════
    review::run(ctx, &mut task).await
}

/// Единственный хендлер «дальше нужен человек».
pub async fn hand_to_human(ctx: &Ctx, task: &Task, why: HumanReason) -> Result<Outcome> {
    let issue = task.issue;
    let title = &task.title;
    let human = ctx.human_label().to_string();

    match &why {
        HumanReason::BlockedWithoutMarker => {
            let partner = ctx.cfg.partner_repo.clone().unwrap_or_default();
            ctx.gh
                .issue_edit_labels(issue, &[&human], &[ctx.blocked_label()])
                .await?;
            ctx.gh
                .issue_comment(issue, &msg::blocked_without_marker(&partner, &human))
                .await?;
            ctx.tg(&msg::tg_blocked_without_marker(issue, title)).await;
            tracing::info!("Issue #{issue}: blocked без BLOCKED-BY — передал человеку.");
        }
        HumanReason::PingPongFromAgent { pr } => {
            ctx.gh.issue_edit_labels(issue, &[&human], &[]).await?;
            if let Some(url) = pr {
                ctx.gh.pr_close(url, msg::pingpong_pr_closed()).await?;
            }
            ctx.tg(&msg::tg_pingpong(issue, title, None)).await;
            tracing::info!("Пинг-понг остановлен, задача у человека.");
        }
        HumanReason::PingPongViolation { pr } => {
            ctx.gh
                .issue_edit_labels(issue, &[&human], &[ctx.blocked_label()])
                .await?;
            match pr {
                Some(url) => {
                    ctx.gh.pr_close(url, msg::pingpong_stopped()).await?;
                    ctx.tg(&msg::tg_pingpong(issue, title, Some(url))).await;
                }
                None => {
                    ctx.gh.issue_comment(issue, msg::pingpong_stopped()).await?;
                    ctx.tg(&msg::tg_pingpong(issue, title, None)).await;
                }
            }
            tracing::info!("Пинг-понг остановлен, задача у человека.");
        }
        HumanReason::NoCommits { why: reason } => {
            ctx.gh.issue_edit_labels(issue, &[&human], &[]).await?;
            ctx.gh
                .issue_comment(issue, &msg::no_commits(reason))
                .await?;
            ctx.tg(&msg::tg_no_commits(issue, title, reason)).await;
            tracing::info!("Агент остановился без коммитов — передал человеку.");
        }
        HumanReason::NoPrFromApp => {
            ctx.gh.issue_edit_labels(issue, &[&human], &[]).await?;
            tracing::info!("@claude завершил работу без PR — задача уходит человеку.");
            ctx.gh
                .issue_comment(issue, &msg::app_no_pr_verdict(&human))
                .await?;
            ctx.tg(&msg::tg_app_no_pr_verdict(issue, title)).await;
        }
        HumanReason::AppTimeout => {
            ctx.gh.issue_edit_labels(issue, &[&human], &[]).await?;
            ctx.gh
                .issue_comment(issue, &msg::app_timeout(ctx.cfg.app_wait_min))
                .await?;
            ctx.tg(&msg::tg_app_timeout(issue, title, ctx.cfg.app_wait_min))
                .await;
            tracing::info!(
                "PR от @claude не появился за {} мин — передал человеку.",
                ctx.cfg.app_wait_min
            );
        }
        HumanReason::CiStillRed { pr } => {
            ctx.gh.issue_edit_labels(issue, &[&human], &[]).await?;
            ctx.gh
                .issue_comment(issue, &msg::ci_still_red(ctx.cfg.max_iterations, pr))
                .await?;
            ctx.tg(&msg::tg_ci_still_red(
                issue,
                title,
                ctx.cfg.max_iterations,
                pr,
            ))
            .await;
            tracing::info!("Передал человеку. Стоп.");
        }
        HumanReason::Conflict { pr } => {
            ctx.gh.issue_edit_labels(issue, &[&human], &[]).await?;
            ctx.gh
                .issue_comment(issue, &msg::conflict_unresolved(&ctx.cfg.base_branch, pr))
                .await?;
            ctx.tg(&msg::tg_conflict_unresolved(
                issue,
                title,
                &ctx.cfg.base_branch,
                pr,
            ))
            .await;
            tracing::info!("Конфликт не разрешён. Стоп.");
        }
        HumanReason::ReviewRequestChanges { pr, why: reason } => {
            ctx.gh.issue_edit_labels(issue, &[&human], &[]).await?;
            ctx.gh
                .issue_comment(issue, &msg::review_request_changes(reason, pr))
                .await?;
            ctx.tg(&msg::tg_review_request_changes(issue, title, pr))
                .await;
            tracing::info!("Ревью запросило правки — передал человеку.");
        }
        HumanReason::ApprovedButConflicting { pr } => {
            ctx.gh.issue_edit_labels(issue, &[&human], &[]).await?;
            ctx.gh
                .issue_comment(
                    issue,
                    &msg::approved_but_conflicting(&ctx.cfg.base_branch, pr),
                )
                .await?;
            ctx.tg(&msg::tg_approved_but_conflicting(
                issue,
                title,
                &ctx.cfg.base_branch,
                pr,
            ))
            .await;
            tracing::info!("PR одобрен, но конфликтует — передал человеку.");
        }
        HumanReason::Terminated => {
            ctx.gh.issue_edit_labels(issue, &[&human], &[]).await?;
            ctx.gh.issue_comment(issue, &msg::terminated(issue)).await?;
            ctx.tg(&msg::tg_terminated(issue, title)).await;
            tracing::info!("Остановка по сигналу: задача #{issue} передана человеку.");
        }
        HumanReason::OrchestratorCrash { context } => {
            ctx.gh.issue_edit_labels(issue, &[&human], &[]).await?;
            ctx.gh
                .issue_comment(issue, &msg::crash_issue(context, issue))
                .await?;
            ctx.tg(&msg::tg_crash(context, Some(issue))).await;
        }
    }

    Ok(Outcome::HandedToHuman { issue, why })
}

/// Перевод отмены по сигналу в исход итерации: метки ставятся, уборка
/// доводится до конца — ровно то, чего не умел bash.
pub async fn on_cancelled(ctx: &Ctx, task: &Task, _c: Cancelled) -> Halt {
    match hand_to_human(ctx, task, HumanReason::Terminated).await {
        Ok(o) => Halt::Stop(o),
        Err(e) => Halt::Error(e),
    }
}

/// Блокировка на партнёрский репозиторий с уборкой ветки (режим local).
pub async fn blocked_on_partner_local(ctx: &Ctx, task: &Task, blocker: String) -> Result<Outcome> {
    ctx.tg(&msg::tg_blocked_on_partner(
        task.issue,
        &task.title,
        ctx.cfg.partner_repo.as_deref().unwrap_or(""),
    ))
    .await;
    ctx.git.checkout(&ctx.cfg.base_branch).await?;
    ctx.git.delete_branch(&task.branch).await?;
    tracing::info!(
        "Задача #{} ждёт {}. Стоп.",
        task.issue,
        ctx.cfg.partner_repo.as_deref().unwrap_or("")
    );
    Ok(Outcome::BlockedOnPartner {
        issue: task.issue,
        blocker,
    })
}

/// Финальное снятие метки очереди. В bash это последняя строка §5 — до неё
/// доходят только исходы после ревью, поэтому и здесь она не в хендлере
/// [`hand_to_human`].
pub async fn drop_task_label(ctx: &Ctx, issue: u64) -> Result<()> {
    ctx.gh
        .issue_edit_labels(issue, &[], &[&ctx.cfg.task_label])
        .await?;
    tracing::info!("Итерация завершена. 🎉");
    Ok(())
}

/// Текст причины «агент не закоммитил» (§2.5).
pub fn nocommit_why(ctx: &Ctx, task: &Task) -> String {
    if task.guard {
        msg::nocommit_why_guard(ctx.cfg.partner_repo.as_deref().unwrap_or(""))
    } else {
        msg::nocommit_why_plain().to_string()
    }
}
