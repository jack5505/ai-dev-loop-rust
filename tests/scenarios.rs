//! Сценарные тесты на фейковых `GitHub`/`Agent`/`Clock`/`Vcs` (§11.2).
//!
//! Каждый сценарий — реальная поломка боевого цикла: очередь пуста; агент
//! без коммитов; ответ за 36 секунд без PR; PR найден до обращения к
//! `@claude`; `CONFLICTING` → rework → `MERGEABLE`; два круга
//! `REQUEST_CHANGES` → человек; SIGTERM посреди ожидания.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};

use ai_dev::agent::{Agent, AgentRequest, AgentRun};
use ai_dev::clock::{Clock, TestClock};
use ai_dev::config::{Config, DevMode};
use ai_dev::gh::model::{Comment, Issue, Mergeable, Pr};
use ai_dev::gh::GitHub;
use ai_dev::git::Vcs;
use ai_dev::notify::RecordingNotifier;
use ai_dev::orchestrator::{self, Ctx, HumanReason, Outcome};
use ai_dev::shutdown::Shutdown;

const T0: i64 = 1_700_000_000;

fn at(offset: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(T0 + offset, 0).unwrap()
}

/// Сценарий ответов: пока в очереди больше одного значения — отдаём по
/// порядку, последнее остаётся «залипшим».
#[derive(Debug)]
struct Script<T>(Mutex<VecDeque<T>>);

impl<T: Clone> Script<T> {
    fn new(items: impl IntoIterator<Item = T>) -> Self {
        Self(Mutex::new(items.into_iter().collect()))
    }

    fn next(&self) -> Option<T> {
        let mut q = self.0.lock().unwrap();
        if q.len() > 1 {
            q.pop_front()
        } else {
            q.front().cloned()
        }
    }
}

// ─── Фейковый GitHub ────────────────────────────────────────────────

struct FakeGitHub {
    self_login: String,
    repo: String,
    queue: Vec<Issue>,
    labels: Mutex<HashMap<u64, Vec<String>>>,
    comments: Mutex<HashMap<u64, Vec<Comment>>>,
    pr_comments: Mutex<HashMap<String, Vec<Comment>>>,
    ai_task_pr: Script<Option<String>>,
    closes_pr: Option<String>,
    head_sha: Script<String>,
    mergeable: Script<Mergeable>,
    merge_state: String,
    checks: Script<bool>,
    open_prs: Vec<Pr>,
    actions: Mutex<Vec<String>>,
    comment_reads: Mutex<usize>,
    /// После N-го чтения комментариев дёрнуть остановку: так в тесте
    /// SIGTERM приходит именно посреди ожидания.
    cancel_after_comment_reads: Mutex<Option<(usize, Shutdown)>>,
}

impl Default for FakeGitHub {
    fn default() -> Self {
        Self {
            self_login: "jack5505".into(),
            repo: "jack5505/mahalla-android".into(),
            queue: Vec::new(),
            labels: Mutex::new(HashMap::new()),
            comments: Mutex::new(HashMap::new()),
            pr_comments: Mutex::new(HashMap::new()),
            ai_task_pr: Script::new([None]),
            closes_pr: None,
            head_sha: Script::new(["sha-a".to_string()]),
            mergeable: Script::new([Mergeable::Mergeable]),
            merge_state: "CLEAN".into(),
            checks: Script::new([true]),
            open_prs: Vec::new(),
            actions: Mutex::new(Vec::new()),
            comment_reads: Mutex::new(0),
            cancel_after_comment_reads: Mutex::new(None),
        }
    }
}

impl FakeGitHub {
    fn record(&self, action: impl Into<String>) {
        self.actions.lock().unwrap().push(action.into());
    }

    fn actions(&self) -> Vec<String> {
        self.actions.lock().unwrap().clone()
    }

    fn has_action_with(&self, needle: &str) -> bool {
        self.actions().iter().any(|a| a.contains(needle))
    }

    fn count_actions_with(&self, needle: &str) -> usize {
        self.actions().iter().filter(|a| a.contains(needle)).count()
    }
}

#[async_trait]
impl GitHub for FakeGitHub {
    async fn self_login(&self) -> Result<String> {
        Ok(self.self_login.clone())
    }

    async fn repo_name_with_owner(&self) -> Result<String> {
        Ok(self.repo.clone())
    }

    async fn issues_with_label(&self, _label: &str) -> Result<Vec<Issue>> {
        Ok(Vec::new())
    }

    async fn search_issues(&self, _query: &str, limit: u32) -> Result<Vec<Issue>> {
        Ok(self.queue.iter().take(limit as usize).cloned().collect())
    }

    async fn search_issues_in_repo(
        &self,
        _repo: &str,
        _query: &str,
        _limit: u32,
    ) -> Result<Vec<Issue>> {
        Ok(Vec::new())
    }

    async fn issue_labels(&self, num: u64) -> Result<Vec<String>> {
        Ok(self
            .labels
            .lock()
            .unwrap()
            .get(&num)
            .cloned()
            .unwrap_or_default())
    }

    async fn issue_view(&self, num: u64) -> Result<Issue> {
        Ok(self
            .queue
            .iter()
            .find(|i| i.number == num)
            .cloned()
            .unwrap_or_default())
    }

    async fn issue_body_and_comments(&self, num: u64) -> Result<Issue> {
        let mut issue = self.issue_view(num).await?;
        issue.comments = Some(self.issue_comments(num).await?);
        Ok(issue)
    }

    async fn issue_comments(&self, num: u64) -> Result<Vec<Comment>> {
        let mut reads = self.comment_reads.lock().unwrap();
        *reads += 1;
        let reads_now = *reads;
        drop(reads);
        if let Some((after, shutdown)) = self.cancel_after_comment_reads.lock().unwrap().as_ref() {
            if reads_now >= *after {
                shutdown.trigger();
            }
        }
        Ok(self
            .comments
            .lock()
            .unwrap()
            .get(&num)
            .cloned()
            .unwrap_or_default())
    }

    async fn issue_state(&self, _repo: Option<&str>, _num: u64) -> Result<String> {
        Ok("OPEN".into())
    }

    async fn issue_edit_labels(&self, num: u64, add: &[&str], remove: &[&str]) -> Result<()> {
        let mut labels = self.labels.lock().unwrap();
        let entry = labels.entry(num).or_default();
        for l in add {
            if !entry.iter().any(|x| x == l) {
                entry.push(l.to_string());
            }
        }
        entry.retain(|x| !remove.iter().any(|r| r == x));
        drop(labels);
        let mut parts: Vec<String> = add.iter().map(|l| format!("+{l}")).collect();
        parts.extend(remove.iter().map(|l| format!("-{l}")));
        self.record(format!("issue edit #{num} {}", parts.join(" ")));
        Ok(())
    }

    async fn issue_comment(&self, num: u64, body: &str) -> Result<()> {
        self.record(format!("issue comment #{num}: {body}"));
        Ok(())
    }

    async fn issue_create(
        &self,
        repo: &str,
        _labels: &[&str],
        title: &str,
        _body: &str,
    ) -> Result<String> {
        self.record(format!("issue create -R {repo}: {title}"));
        Ok(format!("https://github.com/{repo}/issues/777"))
    }

    async fn pr_list_open(&self, _limit: u32) -> Result<Vec<Pr>> {
        Ok(self.open_prs.clone())
    }

    async fn pr_search_open_url(&self, query: &str) -> Result<Option<String>> {
        if query.contains("AI-TASK") {
            Ok(self.ai_task_pr.next().flatten())
        } else {
            Ok(self.closes_pr.clone())
        }
    }

    async fn pr_head_sha(&self, _url: &str) -> Result<String> {
        Ok(self.head_sha.next().unwrap_or_default())
    }

    async fn pr_head_branch(&self, _url: &str) -> Result<String> {
        Ok("claude/issue-1".into())
    }

    async fn pr_mergeable_once(&self, _url: &str) -> Result<Mergeable> {
        Ok(self.mergeable.next().unwrap_or(Mergeable::Unknown))
    }

    async fn pr_merge_state(&self, _url: &str) -> Result<String> {
        Ok(self.merge_state.clone())
    }

    async fn pr_comments(&self, url: &str) -> Result<Vec<Comment>> {
        Ok(self
            .pr_comments
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .unwrap_or_default())
    }

    async fn pr_create_draft(
        &self,
        base: &str,
        head: &str,
        title: &str,
        _body: &str,
    ) -> Result<String> {
        self.record(format!(
            "pr create --draft --base {base} --head {head}: {title}"
        ));
        Ok("https://github.com/acme/repo/pull/1".into())
    }

    async fn pr_checks_watch(&self, _url: &str) -> Result<bool> {
        Ok(self.checks.next().unwrap_or(true))
    }

    async fn pr_comment(&self, url: &str, body: &str) -> Result<()> {
        self.record(format!("pr comment {url}: {body}"));
        Ok(())
    }

    async fn pr_close(&self, url: &str, comment: &str) -> Result<()> {
        self.record(format!("pr close {url}: {comment}"));
        Ok(())
    }

    async fn pr_ready(&self, url: &str) -> Result<()> {
        self.record(format!("pr ready {url}"));
        Ok(())
    }

    async fn pr_diff(&self, _url: &str) -> Result<String> {
        Ok("diff --git a/x b/x\n+строка\n".into())
    }

    async fn pr_merge_squash_auto(&self, url: &str) -> Result<()> {
        self.record(format!("pr merge {url} --squash --auto"));
        Ok(())
    }

    async fn pr_disable_auto_merge(&self, url: &str) -> Result<()> {
        self.record(format!("pr merge {url} --disable-auto"));
        Ok(())
    }

    async fn latest_run_id(&self, _branch: &str) -> Result<Option<String>> {
        Ok(Some("1".into()))
    }

    async fn run_log_failed(&self, _id: &str) -> Result<String> {
        Ok("шаг упал: тест".into())
    }
}

// ─── Фейковые агент и рабочая копия ─────────────────────────────────

struct FakeAgent {
    responses: Script<AgentRun>,
    prompts: Mutex<Vec<String>>,
}

impl FakeAgent {
    fn new(responses: impl IntoIterator<Item = AgentRun>) -> Self {
        Self {
            responses: Script::new(responses),
            prompts: Mutex::new(Vec::new()),
        }
    }

    fn ok(output: &str) -> AgentRun {
        AgentRun {
            success: true,
            output: output.to_string(),
        }
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait]
impl Agent for FakeAgent {
    async fn run(&self, req: AgentRequest) -> Result<AgentRun> {
        self.prompts.lock().unwrap().push(req.prompt.clone());
        Ok(self.responses.next().unwrap_or_else(|| FakeAgent::ok("")))
    }
}

struct FakeVcs {
    commits_ahead: u64,
    actions: Mutex<Vec<String>>,
}

impl FakeVcs {
    fn new(commits_ahead: u64) -> Self {
        Self {
            commits_ahead,
            actions: Mutex::new(Vec::new()),
        }
    }

    fn actions(&self) -> Vec<String> {
        self.actions.lock().unwrap().clone()
    }

    fn record(&self, a: impl Into<String>) {
        self.actions.lock().unwrap().push(a.into());
    }
}

#[async_trait]
impl Vcs for FakeVcs {
    async fn sync_base(&self, base: &str) -> Result<()> {
        self.record(format!("sync {base}"));
        Ok(())
    }

    async fn checkout(&self, branch: &str) -> Result<()> {
        self.record(format!("checkout {branch}"));
        Ok(())
    }

    async fn checkout_new_branch(&self, branch: &str) -> Result<()> {
        self.record(format!("checkout -B {branch}"));
        Ok(())
    }

    async fn delete_branch(&self, branch: &str) -> Result<()> {
        self.record(format!("branch -D {branch}"));
        Ok(())
    }

    async fn commits_ahead(&self, _base: &str) -> Result<u64> {
        Ok(self.commits_ahead)
    }

    async fn push_new_branch(&self, branch: &str) -> Result<()> {
        self.record(format!("push -u origin {branch}"));
        Ok(())
    }

    async fn push_force_with_lease(&self) -> Result<()> {
        self.record("push --force-with-lease".to_string());
        Ok(())
    }
}

// ─── Сборка окружения ───────────────────────────────────────────────

fn tmp_repo(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ai-dev-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".ai-logs")).unwrap();
    dir
}

fn issue(number: u64, title: &str, body: &str) -> Issue {
    Issue {
        number,
        title: title.into(),
        body: Some(body.into()),
        created_at: Some(at(-86_400)),
        ..Default::default()
    }
}

struct Harness {
    ctx: Ctx,
    gh: Arc<FakeGitHub>,
    agent: Arc<FakeAgent>,
    vcs: Arc<FakeVcs>,
    notify: RecordingNotifier,
    clock: TestClock,
}

fn harness(
    name: &str,
    mode: DevMode,
    gh: FakeGitHub,
    agent: FakeAgent,
    vcs: FakeVcs,
    shutdown: Shutdown,
    tweak: impl FnOnce(&mut Config),
) -> Harness {
    let mut cfg = Config {
        repo_dir: tmp_repo(name),
        dev_mode: mode,
        ci_start_wait: 1,
        ..Default::default()
    };
    tweak(&mut cfg);

    let gh = Arc::new(gh);
    let agent = Arc::new(agent);
    let vcs = Arc::new(vcs);
    let notify = RecordingNotifier::default();
    let clock = TestClock::new(T0);

    let ctx = Ctx {
        cfg,
        gh: gh.clone(),
        agent: agent.clone(),
        notify: Arc::new(notify.clone()),
        clock: Arc::new(clock.clone()),
        git: vcs.clone(),
        shutdown,
        self_login: "jack5505".into(),
        this_repo: "jack5505/mahalla-android".into(),
        dry_run: false,
    };
    Harness {
        ctx,
        gh,
        agent,
        vcs,
        notify,
        clock,
    }
}

// ─── Сценарии ───────────────────────────────────────────────────────

#[tokio::test]
async fn queue_empty_does_nothing() {
    let h = harness(
        "queue-empty",
        DevMode::GithubApp,
        FakeGitHub::default(),
        FakeAgent::new([FakeAgent::ok("")]),
        FakeVcs::new(0),
        Shutdown::never(),
        |_| {},
    );
    let outcome = orchestrator::run(&h.ctx, None).await.unwrap();
    assert_eq!(outcome, Outcome::QueueEmpty);
    assert!(h.gh.actions().is_empty(), "{:?}", h.gh.actions());
    assert!(h.notify.messages().is_empty());
}

#[tokio::test]
async fn existing_pr_is_reused_without_calling_claude() {
    // Задача могла вернуться из-под blocked с готовым PR: второй вызов
    // @claude — холостой прогон Actions.
    let gh = FakeGitHub {
        queue: vec![issue(309, "Счётчик refresh-провалов", "тело")],
        ai_task_pr: Script::new([Some("https://github.com/acme/repo/pull/5".to_string())]),
        ..Default::default()
    };
    let h = harness(
        "pr-reused",
        DevMode::GithubApp,
        gh,
        FakeAgent::new([FakeAgent::ok("всё хорошо\nVERDICT: APPROVE")]),
        FakeVcs::new(1),
        Shutdown::never(),
        |_| {},
    );

    let outcome = orchestrator::run(&h.ctx, None).await.unwrap();
    assert_eq!(
        outcome,
        Outcome::PrAwaitingReview {
            issue: 309,
            pr: "https://github.com/acme/repo/pull/5".into()
        }
    );
    assert!(!h.gh.has_action_with("@claude Реализуй"));
    assert!(h.gh.has_action_with("🤖 Взял в работу"));
    assert!(h.gh.has_action_with("👀 PR готов и ждёт вашего решения"));
    // Метка очереди снимается последним действием — как в §5 bash.
    assert_eq!(
        h.gh.actions().last().unwrap(),
        "issue edit #309 -ai-task",
        "{:?}",
        h.gh.actions()
    );
}

#[tokio::test]
async fn verdict_without_pr_does_not_wait_three_hours() {
    // #237: ответ за 36 секунд — ожидание 180 минут и бесконечный возврат
    // задачи в очередь.
    let mut comments = HashMap::new();
    comments.insert(
        237,
        vec![Comment::new(
            "claude",
            "**Claude finished** its work",
            at(36),
        )],
    );
    let gh = FakeGitHub {
        queue: vec![issue(237, "Задача без PR", "тело")],
        comments: Mutex::new(comments),
        ..Default::default()
    };
    let h = harness(
        "verdict-no-pr",
        DevMode::GithubApp,
        gh,
        FakeAgent::new([FakeAgent::ok("")]),
        FakeVcs::new(0),
        Shutdown::never(),
        |_| {},
    );

    let outcome = orchestrator::run(&h.ctx, None).await.unwrap();
    assert_eq!(
        outcome,
        Outcome::HandedToHuman {
            issue: 237,
            why: HumanReason::NoPrFromApp
        }
    );
    assert!(h.gh.has_action_with("issue edit #237 +needs-human"));
    assert!(h.gh.has_action_with("PR не открыл"));
    // Ждали один круг, а не APP_WAIT_MIN.
    assert!(
        h.clock.now() <= at(120),
        "ожидание затянулось до {}",
        h.clock.now()
    );
    // Метку очереди на раннем выходе не снимаем: задача должна остаться
    // видимой для смотрителя бэклога.
    assert!(!h.gh.has_action_with("-ai-task"));
}

#[tokio::test]
async fn agent_without_commits_goes_to_human() {
    let gh = FakeGitHub {
        queue: vec![issue(1, "Непонятная задача", "тело")],
        ..Default::default()
    };
    let h = harness(
        "no-commits",
        DevMode::Local,
        gh,
        FakeAgent::new([FakeAgent::ok("сделал вид, что поработал")]),
        FakeVcs::new(0),
        Shutdown::never(),
        |_| {},
    );

    let outcome = orchestrator::run(&h.ctx, None).await.unwrap();
    match outcome {
        Outcome::HandedToHuman {
            issue: 1,
            why: HumanReason::NoCommits { why },
        } => assert!(why.contains("сформулирована непонятно"), "{why}"),
        other => panic!("ожидался NoCommits, получено {other:?}"),
    }
    assert!(h
        .vcs
        .actions()
        .contains(&"checkout -B ai/issue-1".to_string()));
    assert!(h.gh.has_action_with("без единого коммита"));
    // PR не создавали.
    assert!(!h.gh.has_action_with("pr create"));
}

#[tokio::test]
async fn conflicting_pr_is_reworked_then_merges() {
    let gh = FakeGitHub {
        queue: vec![issue(42, "Конфликтующая задача", "тело")],
        ai_task_pr: Script::new([Some("https://github.com/acme/repo/pull/9".to_string())]),
        mergeable: Script::new([Mergeable::Conflicting, Mergeable::Mergeable]),
        head_sha: Script::new(["sha-a".to_string(), "sha-b".to_string()]),
        ..Default::default()
    };
    let h = harness(
        "conflict-rework",
        DevMode::GithubApp,
        gh,
        FakeAgent::new([FakeAgent::ok("ок\nVERDICT: APPROVE")]),
        FakeVcs::new(1),
        Shutdown::never(),
        |cfg| cfg.auto_merge = true,
    );

    let outcome = orchestrator::run(&h.ctx, None).await.unwrap();
    assert_eq!(
        outcome,
        Outcome::Merged {
            issue: 42,
            pr: "https://github.com/acme/repo/pull/9".into()
        }
    );
    assert!(h.gh.has_action_with("@claude PR конфликтует с веткой"));
    assert!(h.gh.has_action_with("--squash --auto"));
    assert_eq!(h.gh.count_actions_with("🤖 Авто-ревью"), 1);
}

#[tokio::test]
async fn two_request_changes_rounds_end_with_human() {
    let gh = FakeGitHub {
        queue: vec![issue(7, "Спорная задача", "тело")],
        ai_task_pr: Script::new([Some("https://github.com/acme/repo/pull/7".to_string())]),
        head_sha: Script::new([
            "sha-a".to_string(),
            "sha-b".to_string(),
            "sha-c".to_string(),
        ]),
        ..Default::default()
    };
    let h = harness(
        "request-changes",
        DevMode::GithubApp,
        gh,
        FakeAgent::new([FakeAgent::ok("замечания\nVERDICT: REQUEST_CHANGES")]),
        FakeVcs::new(1),
        Shutdown::never(),
        |cfg| cfg.max_review_rounds = 2,
    );

    let outcome = orchestrator::run(&h.ctx, None).await.unwrap();
    match outcome {
        Outcome::HandedToHuman {
            issue: 7,
            why: HumanReason::ReviewRequestChanges { why, .. },
        } => assert!(
            why.contains("авто-ревью осталось при"),
            "причина потерялась: {why}"
        ),
        other => panic!("ожидался ReviewRequestChanges, получено {other:?}"),
    }
    assert_eq!(h.gh.count_actions_with("🤖 Авто-ревью"), 2);
    assert!(h.gh.has_action_with("issue edit #7 +needs-human"));
    assert_eq!(h.gh.actions().last().unwrap(), "issue edit #7 -ai-task");
    // Причина попала и в комментарий человеку.
    assert!(h.gh.has_action_with("Причина: авто-ревью осталось при"));
}

#[tokio::test]
async fn sigterm_mid_wait_still_marks_the_issue() {
    // Корневая причина затора 2026-09-14: SIGTERM посреди `sleep 60`
    // убивал bash до простановки метки, задача возвращалась в очередь и
    // держала всё за собой.
    let shutdown = Shutdown::never();
    let gh = FakeGitHub {
        queue: vec![issue(314, "Задача в ожидании", "тело")],
        cancel_after_comment_reads: Mutex::new(Some((1, shutdown.clone()))),
        ..Default::default()
    };
    let h = harness(
        "sigterm",
        DevMode::GithubApp,
        gh,
        FakeAgent::new([FakeAgent::ok("")]),
        FakeVcs::new(0),
        shutdown,
        |_| {},
    );

    let outcome = orchestrator::run(&h.ctx, None).await.unwrap();
    assert_eq!(
        outcome,
        Outcome::HandedToHuman {
            issue: 314,
            why: HumanReason::Terminated
        }
    );
    assert!(h.gh.has_action_with("issue edit #314 +needs-human"));
    assert!(h.gh.has_action_with("остановили сигналом"));
    assert!(h
        .notify
        .messages()
        .iter()
        .any(|m| m.contains("остановили сигналом")));
}

#[tokio::test]
async fn keeper_does_not_create_tasks_while_queue_is_not_empty() {
    // Порядок принципиален: заводить новое поверх затора — гнать задачи
    // в ту же пробку.
    let gh = FakeGitHub {
        queue: vec![issue(1, "Уже в очереди", "тело")],
        ..Default::default()
    };
    let h = harness(
        "keeper",
        DevMode::Local,
        gh,
        FakeAgent::new([FakeAgent::ok("разобрал")]),
        FakeVcs::new(0),
        Shutdown::never(),
        |_| {},
    );

    let outcome = ai_dev::keeper::run(&h.ctx, true).await.unwrap();
    assert_eq!(
        outcome,
        ai_dev::keeper::KeeperOutcome::Triaged {
            before: 1,
            after: 1
        }
    );
    // Ровно одно поручение: разбор затора. Второго — про новые задачи — нет.
    let prompts = h.agent.prompts();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(prompts[0].contains("Разбери затор"));
    // Отметка о недельном проходе поставлена.
    assert!(ai_dev::keeper::stamp_path(&h.ctx.log_dir()).exists());
}
