//! Промпты агенту и тексты для людей — в одном месте.
//!
//! Перенесены из bash **дословно**: русский текст, эмодзи, формат
//! `VERDICT: APPROVE`. Любая правка формулировок — отдельный коммит после
//! того, как паритет подтверждён: иначе непонятно, что именно изменило
//! поведение агента.

use crate::config::DevMode;

/// Хвост, который дописывается в каждый промпт агента. Вариантов ровно
/// четыре: защита от пинг-понга × режим разработки.
///
/// Защита включается, если задача пришла ИЗ партнёрского репозитория
/// (маркер `ORIGIN`) или уже блокировалась дважды.
pub fn block_hint(
    partner_repo: Option<&str>,
    guard: bool,
    mode: DevMode,
    this_repo: &str,
    issue: u64,
) -> String {
    let Some(partner) = partner_repo else {
        return String::new();
    };
    match (guard, mode) {
        (true, DevMode::GithubApp) => format!(
            "

ВАЖНО: эта задача либо пришла из {partner}, либо уже блокировалась
на него дважды. Перекладывать её обратно ЗАПРЕЩЕНО (защита от
бесконечного пинг-понга). Если ты уверен, что причина всё-таки не в
этом репозитории, — НЕ открывай PR и не делай заглушек, а оставь на
этом issue комментарий, начинающийся строкой CANNOT-FIX-HERE: с
объяснением. Система позовёт человека."
        ),
        (true, DevMode::Local) => format!(
            "

ВАЖНО: эта задача либо пришла из {partner}, либо уже блокировалась
на него дважды. Создавать встречные задачи в {partner} ЗАПРЕЩЕНО
(защита от бесконечного пинг-понга между репозиториями). Если ты
уверен, что причина всё-таки не в этом репозитории, — не делай никаких
коммитов и обходных заглушек, просто заверши работу: система сама
позовёт человека."
        ),
        (false, DevMode::GithubApp) => format!(
            "

ВАЖНО — межрепозиторные ошибки. Если станет очевидно, что причина
проблемы НЕ в этом репозитории, а на стороне {partner} (их API
отвечает ошибкой или контракт не совпадает) — НЕ чини это здесь и не
делай обходных заглушек. Вместо открытия PR оставь на этом issue
комментарий, начинающийся строкой NEEDS-PARTNER: с подробным описанием
проблемы и логами. Сервер сам заведёт задачу в {partner} и вернётся
к этой задаче после починки."
        ),
        (false, DevMode::Local) => format!(
            "

ВАЖНО — межрепозиторные ошибки. Если станет очевидно, что причина
проблемы НЕ в этом репозитории, а на стороне {partner} (например,
их API отвечает ошибкой или контракт не совпадает с ожидаемым) — НЕ
пытайся чинить это здесь и не делай обходных заглушек. Вместо этого
выполни ровно три команды и заверши работу:
1) gh issue create -R {partner} --label ai-task --title \"<краткая суть проблемы>\" --body \"ORIGIN: {this_repo}#{issue}
<подробности, логи, что именно не так>\"
   (первая строка body — ровно этот маркер ORIGIN, он обязателен)
2) gh issue comment {issue} --body \"BLOCKED-BY: {partner}#<номер созданного issue>\"
3) gh issue edit {issue} --add-label blocked"
        ),
    }
}

/// §2.5 — поручение агенту на этом сервере.
pub fn implement_local(
    issue: u64,
    title: &str,
    body: &str,
    base_branch: &str,
    block_hint: &str,
) -> String {
    format!(
        "Задача из GitHub issue #{issue}: «{title}»

{body}

Реализуй эту задачу в текущем репозитории.
Правила:
- следуй инструкциям из CLAUDE.md в корне репозитория;
- делай атомарные коммиты (git add + git commit) с понятными сообщениями;
- НИЧЕГО не пушь и не переключай ветки;
- ветку {base_branch} не трогай.{block_hint}"
    )
}

/// §2.6 — поручение `@claude` на GitHub Actions. Требование двух строк в
/// теле PR критично: по ним цикл потом находит PR (**C4**).
pub fn assign_to_app(issue: u64, base_branch: &str, block_hint: &str) -> String {
    format!(
        "@claude Реализуй задачу из этого issue.
Требования:
- следуй CLAUDE.md репозитория;
- где возможно, прогони сборку и тесты у себя перед пушем;
- открой pull request в ветку {base_branch};
- в описании PR обязательно укажи две строки: «AI-TASK: #{issue}» и «Closes #{issue}».{block_hint}"
    )
}

/// §2.7 — красный CI, режим local.
pub fn ci_fix_local(attempt: u32, max: u32, fail_tail: &str, block_hint: &str) -> String {
    format!(
        "CI на GitHub упал (попытка {attempt} из {max}). Конец лога упавших шагов:

```
{fail_tail}
```

Найди причину, исправь код и закоммить исправление. Ничего не пушь.{block_hint}"
    )
}

/// §2.7 — красный CI, режим github-app.
pub fn ci_fix_app(attempt: u32, max: u32, fail_tail: &str, block_hint: &str) -> String {
    format!(
        "@claude CI упал (попытка {attempt} из {max}). Конец лога упавших шагов:

```
{fail_tail}
```

Найди причину, исправь и запушь коммит в эту же ветку.{block_hint}"
    )
}

/// §2.8 — промпт ревьюера. **C11**: защита от ложных блокеров внутри
/// текста, ревьюер ошибается примерно в трети блокеров.
pub fn review(base_branch: &str) -> String {
    format!(
        "Ты строгий, но честный код-ревьюер. На stdin — дифф pull request'а.
Проверь: безопасность (секреты, инъекции, права), корректность логики,
обработку ошибок, качество кода. Пиши кратко и по делу, по-русски.

ПРЕЖДЕ ЧЕМ НАЗВАТЬ ЧТО-ТО БЛОКЕРОМ — проверь себя:
- CI на этом PR уже зелёный: сборка и тесты прошли. Поэтому замечание вида
  «это не скомпилируется» почти наверняка твоя ошибка — перепроверь по коду
  или не пиши его вовсе;
- дифф показан относительно базы ветки, а НЕ результата мержа. Прежде чем
  писать про порядок строк, дубли или «ветка отстала», проверь фактом:
  `git merge-tree --write-tree origin/{base_branch} <ветка PR>`;
- замечание без конкретного файла и строки — не замечание.
Ложный блокер дороже пропущенного: по нему будет переписан рабочий код.

САМОЙ ПОСЛЕДНЕЙ строкой выведи ровно одно из двух:
VERDICT: APPROVE
VERDICT: REQUEST_CHANGES"
    )
}

/// Ревью публикуется в PR как есть.
pub fn review_comment(round: u32, max: u32, review: &str) -> String {
    format!(
        "## 🤖 Авто-ревью (круг {round} из {max})

{review}"
    )
}

/// Поручение влить базу при конфликте.
pub fn rework_conflict(base_branch: &str) -> String {
    format!(
        "PR конфликтует с веткой `{base_branch}`. Влей свежий `{base_branch}` в ветку PR, разреши конфликты, ничего из изменений PR не потеряв, и запушь в ту же ветку. Логику задачи при этом не меняй."
    )
}

/// **C11**: поручение на доработку требует проверять замечания фактом.
pub fn rework_review(round: u32, max: u32, base_branch: &str) -> String {
    format!(
        "Авто-ревью вернуло `REQUEST_CHANGES` по этому PR (круг {round} из {max}). Сами замечания — в комментарии выше.

Не бросайся исправлять всё подряд: этот ревьюер ошибается примерно в трети
блокеров, и всегда одинаково — судит по диффу относительно базы ветки,
игнорируя зелёный CI и результат мержа.
По каждому замечанию сначала установи факт: по коду, по статусу CI и по
`git merge-tree --write-tree origin/{base_branch} HEAD`. Затем:
- подтверждённое — исправь и запушь коммит в эту же ветку;
- ошибочное — код НЕ трогай, ответь отдельным комментарием в PR, что именно
  неверно и чем это опровергается.
Если подтверждённых замечаний не нашлось вовсе — не коммить ничего, только ответь."
    )
}

/// Тело PR, который создаёт сам оркестратор (режим local).
pub fn pr_body(issue: u64) -> String {
    format!(
        "Closes #{issue}

Автономная реализация (ai-dev loop v2). Проверки выполняет GitHub Actions."
    )
}

/// Тексты для людей: комментарии в issue и PR, сообщения в Telegram.
/// Держим рядом, чтобы у каждого исхода итерации был ровно один набор
/// формулировок.
pub mod msg {
    /// §2.1 — `blocked` без маркера (**C6**).
    pub fn blocked_without_marker(partner_repo: &str, human_label: &str) -> String {
        format!(
            "🛑 Метка `blocked` стоит без маркера `BLOCKED-BY: owner/repo#N`, поэтому система не знает, чего эта задача ждёт, и разблокировать её сама не может.

Что сделать: либо добавь комментарий вида `BLOCKED-BY: {partner_repo}#<номер>` и верни метку `blocked` — тогда задача разблокируется автоматически, когда блокер закроют; либо просто сними `{human_label}`, чтобы задача вернулась в очередь."
        )
    }

    pub fn tg_blocked_without_marker(issue: u64, title: &str) -> String {
        format!("🛑 AI dev loop: #{issue} «{title}» помечена blocked без маркера BLOCKED-BY — не знаю, чего она ждёт. Нужен ты.")
    }

    pub fn unblocked(reference: &str) -> String {
        format!(
            "🔓 Блокировка снята: {reference} закрыт. Задача вернулась в очередь на перепроверку."
        )
    }

    pub fn tg_pr_watch(repo: &str, report: &str) -> String {
        format!("👁 AI dev loop ({repo}): открытые PR требуют внимания:\n{report}")
    }

    pub fn taken() -> &'static str {
        "🤖 Взял в работу."
    }

    pub fn agent_unavailable() -> &'static str {
        "⏸️ Claude сейчас недоступен (возможно, исчерпан лимит подписки). Задача остаётся в очереди — попробую в следующий круг."
    }

    pub fn pingpong_stopped() -> &'static str {
        "🛑 Встречная блокировка запрещена (защита от пинг-понга) — задача передана человеку."
    }

    pub fn pingpong_pr_closed() -> &'static str {
        "🛑 Агент считает, что чинить нужно не здесь, а встречная блокировка запрещена (защита от пинг-понга) — задача передана человеку."
    }

    pub fn tg_pingpong(issue: u64, title: &str, pr: Option<&str>) -> String {
        match pr {
            Some(url) => format!("🛑 AI dev loop: #{issue} «{title}» — агенты двух репо не договорились, где чинить. Нужен ты: {url}"),
            None => format!("🛑 AI dev loop: #{issue} «{title}» — агенты двух репо не договорились, где чинить. Нужен ты."),
        }
    }

    pub fn tg_blocked_on_partner(issue: u64, title: &str, partner_repo: &str) -> String {
        format!("⏳ AI dev loop: #{issue} «{title}» заблокирована — причина на стороне {partner_repo}, агент завёл там задачу. Вернусь к ней после починки.")
    }

    pub fn tg_blocked_on_partner_short(issue: u64, title: &str, partner_repo: &str) -> String {
        format!(
            "⏳ AI dev loop: #{issue} «{title}» заблокирована — причина на стороне {partner_repo}."
        )
    }

    pub fn tg_blocked_on_existing_twin(
        issue: u64,
        title: &str,
        partner_repo: &str,
        twin: u64,
    ) -> String {
        format!("⏳ AI dev loop: #{issue} «{title}» заблокирована — ждёт {partner_repo}#{twin} (задача там уже была).")
    }

    pub fn pr_closed_partner_existing(partner_repo: &str) -> String {
        format!("⏳ Причина на стороне {partner_repo} — задача там уже заведена ранее. После починки эта задача автоматически вернётся в очередь.")
    }

    pub fn pr_closed_partner_new(partner_repo: &str) -> String {
        format!("⏳ Причина на стороне {partner_repo} — задача заведена там. После починки эта задача автоматически вернётся в очередь.")
    }

    pub fn pr_closed_partner_after_ci(partner_repo: &str, issue: u64) -> String {
        format!("⏳ Причина на стороне {partner_repo} — агент завёл там задачу (см. маркер BLOCKED-BY в issue #{issue}). PR закрыт; после починки задача автоматически вернётся в очередь и будет перепроверена.")
    }

    pub fn no_commits(why: &str) -> String {
        format!("🛑 Агент завершил работу без единого коммита. {why} Нужен человек.")
    }

    pub fn tg_no_commits(issue: u64, title: &str, why: &str) -> String {
        format!("🛑 AI dev loop: #{issue} «{title}» — агент остановился без коммитов. {why}")
    }

    pub fn nocommit_why_plain() -> &'static str {
        "Похоже, задача сформулирована непонятно."
    }

    pub fn nocommit_why_guard(partner_repo: &str) -> String {
        format!("Задача связана с {partner_repo}, и агент считает, что чинить нужно не здесь, но встречная блокировка запрещена (защита от пинг-понга).")
    }

    pub fn app_no_pr_verdict(human_label: &str) -> String {
        format!("🛑 @claude завершил работу и PR не открыл — нужен человек. Если задача уже сделана, закрой issue; если нет — переформулируй и сними метку `{human_label}` (ход работы: комментарии и вкладка Actions).")
    }

    pub fn tg_app_no_pr_verdict(issue: u64, title: &str) -> String {
        format!(
            "🛑 AI dev loop: #{issue} «{title}» — @claude отработал, но PR не открыл. Нужен ты."
        )
    }

    pub fn app_timeout(wait_min: u64) -> String {
        format!("🛑 @claude не открыл PR за {wait_min} минут — нужен человек (ход работы: комментарии и вкладка Actions).")
    }

    pub fn tg_app_timeout(issue: u64, title: &str, wait_min: u64) -> String {
        format!("🛑 AI dev loop: #{issue} «{title}» — PR от @claude не появился за {wait_min} мин. Нужен ты.")
    }

    pub fn ci_still_red(max: u32, pr: &str) -> String {
        format!(
            "🛑 После {max} попыток CI всё ещё красный — нужен человек.
PR (draft): {pr}. Логи CI: вкладка Checks в PR."
        )
    }

    pub fn tg_ci_still_red(issue: u64, title: &str, max: u32, pr: &str) -> String {
        format!("🛑 AI dev loop: #{issue} «{title}» — {max} попытки, CI всё ещё красный. Нужна твоя помощь: {pr}")
    }

    pub fn conflict_unresolved(base_branch: &str, pr: &str) -> String {
        format!("🛑 PR конфликтует с `{base_branch}`, автоматически разрешить не вышло — нужен человек: {pr}")
    }

    pub fn tg_conflict_unresolved(issue: u64, title: &str, base_branch: &str, pr: &str) -> String {
        format!(
            "🛑 AI dev loop: #{issue} «{title}» — PR конфликтует с {base_branch}. Нужен ты: {pr}"
        )
    }

    /// Финальный `REQUEST_CHANGES`. Причина (`why`) в bash присваивалась в
    /// переменную `REWORK_WHY` и нигде не читалась — три разных исхода
    /// приходили человеку одним текстом. Причину возвращаем в сообщение.
    pub fn review_request_changes(why: &str, pr: &str) -> String {
        format!("⚠️ Авто-ревью запросило правки — нужен человек: {pr}\nПричина: {why}.")
    }

    pub fn tg_review_request_changes(issue: u64, title: &str, pr: &str) -> String {
        format!("⚠️ AI dev loop: #{issue} «{title}» — ревью запросило правки, нужен ты: {pr}")
    }

    pub fn approved_but_conflicting(base_branch: &str, pr: &str) -> String {
        format!("⚠️ Ревью одобрено, но PR конфликтует с `{base_branch}` — авто-merge не запускаю. Нужен ручной rebase: {pr}")
    }

    pub fn tg_approved_but_conflicting(
        issue: u64,
        title: &str,
        base_branch: &str,
        pr: &str,
    ) -> String {
        format!("⚠️ AI dev loop: #{issue} «{title}» одобрен ревью, но конфликт с {base_branch} — нужен ручной rebase: {pr}")
    }

    pub fn auto_merge_queued(pr: &str) -> String {
        format!("✅ Ревью пройдено, PR поставлен на авто-merge: {pr}")
    }

    pub fn tg_auto_merge_queued(issue: u64, title: &str, pr: &str) -> String {
        format!("✅ AI dev loop: #{issue} «{title}» готово и уходит в авто-merge: {pr}")
    }

    pub fn pr_awaiting_human(pr: &str) -> String {
        format!("👀 PR готов и ждёт вашего решения: {pr}")
    }

    pub fn tg_pr_awaiting_human(issue: u64, title: &str, pr: &str) -> String {
        format!("👀 AI dev loop: #{issue} «{title}» — PR готов, глянь, когда будет минутка: {pr}")
    }

    /// Зависший авто-merge: конфликт возник ПОСЛЕ постановки в очередь.
    pub fn stuck_merge_pr(base_branch: &str) -> String {
        format!("⚠️ Авто-merge был поставлен в очередь, но PR теперь конфликтует с `{base_branch}` (кто-то смёржил другой PR раньше). Авто-merge снят — нужен ручной rebase.")
    }

    pub fn stuck_merge_issue(pr: &str, base_branch: &str) -> String {
        format!("⚠️ PR {pr} ждал авто-merge, но возник конфликт с `{base_branch}` — нужен человек.")
    }

    pub fn tg_stuck_merge(pr: &str, issue: u64) -> String {
        format!("⚠️ AI dev loop: PR {pr} (issue #{issue}) — конфликт после постановки в авто-merge, нужен ручной rebase.")
    }

    /// Падение оркестратора. Вместо `$LINENO` — цепочка контекста, и в
    /// алерте верная команда: юниты шаблонные, `ai-dev.service` не
    /// существует, и диагностика каждый раз начиналась с ложного тупика.
    pub fn crash_issue(context: &str, issue: u64) -> String {
        format!("🛑 Оркестратор упал с ошибкой: {context}. Логи: `.ai-logs/issue-{issue}-*` на сервере. Нужен человек.")
    }

    pub fn tg_crash(context: &str, issue: Option<u64>) -> String {
        let tail = match issue {
            Some(n) => format!(", задача #{n}"),
            None => String::new(),
        };
        format!("🛑 AI dev loop: оркестратор упал ({context}){tail}. Загляни на сервер: journalctl -u 'ai-dev@*' -n 200 --no-pager -q")
    }

    /// Остановка по сигналу: задача не должна вернуться в очередь молча.
    pub fn terminated(issue: u64) -> String {
        format!("🛑 Итерацию остановили сигналом (`systemctl stop` или таймаут юнита) — работа по задаче #{issue} прервана на полпути. Нужен человек: проверь, остался ли PR и в каком он состоянии.")
    }

    pub fn tg_terminated(issue: u64, title: &str) -> String {
        format!("🛑 AI dev loop: #{issue} «{title}» — итерацию остановили сигналом, задача передана человеку.")
    }
}

/// Промпты смотрителя бэклога (§3).
pub mod keeper {
    pub fn triage(
        this_repo: &str,
        partner_repo: &str,
        queue_query: &str,
        queue_size: usize,
        human_label: &str,
        base_branch: &str,
    ) -> String {
        format!(
            "Ты смотритель бэклога репозитория {this_repo}. Разбери затор.
Партнёрский репозиторий проекта: {partner_repo}.

ОЧЕРЕДЬ задач AI-агента — это ровно такой запрос:
  gh issue list --state open --search '{queue_query}'
Сейчас в ней {queue_size} задач. Твоя цель — вернуть в неё то, что уже можно
делать, и снять с людей то, что людям больше не нужно.

Разбери три группы.

1) Задачи с меткой `{human_label}`. По каждой выясни из комментариев, ПОЧЕМУ
   она там. Если причина уже отпала — блокер закрыт, нужный PR влит,
   контракт появился, задача сделана другим PR — сними метку `{human_label}`
   и напиши комментарием, что именно изменилось. Если задача дублирует
   закрытую — закрой со ссылкой на оригинал. Если человек действительно
   нужен — НЕ трогай её вовсе.

2) Задачи с меткой `blocked`. Рабочий маркер — комментарий вида
   `BLOCKED-BY: owner/repo#N`; без него автоматика разблокировать не умеет.
   Если блокер назван только в заголовке или в теле — проверь его состояние:
   закрыт → сними `blocked`; открыт → добавь недостающий комментарий
   `BLOCKED-BY: owner/repo#N`, чтобы задача разблокировалась сама.

3) Открытые pull request'ы с маркером `AI-TASK: #N` или `Closes #N`.
   Если два PR закрывают одну и ту же issue — оставь более полный, второй
   закрой с объяснением. Если PR конфликтует с `{base_branch}` или висит без
   движения — напиши в нём коротким комментарием, чего он ждёт.

ЧТО МОЖНО: gh issue edit (метки), gh issue comment, gh issue close,
gh pr comment, gh pr close. Читать код и историю — сколько нужно.

ЧТО НЕЛЬЗЯ: менять код, коммитить, пушить, мержить PR, создавать новые
issue (это отдельный шаг, он будет после тебя), трогать репозиторий
{partner_repo}.

Действуй консервативно: сомневаешься — не трогай. Лучше оставить лишнее
человеку, чем вернуть в очередь то, что не готово.

В конце выведи короткую сводку: что разблокировал, что закрыл, что оставил
человеку и почему."
        )
    }

    pub fn new_tasks(
        this_repo: &str,
        partner_repo: &str,
        max_new_tasks: u32,
        task_label: &str,
    ) -> String {
        format!(
            "Ты смотритель бэклога репозитория {this_repo}. Очередь задач
AI-агента пуста даже после разбора затора — значит нужна новая работа.

Заведи НЕ БОЛЬШЕ {max_new_tasks} задач. Это жёсткий потолок.

Откуда брать: следуй CLAUDE.md репозитория, посмотри документацию
(docs/, ADR, CHANGELOG), расхождения контракта с реальностью, замечания
авто-ревью в открытых PR, места с TODO/FIXME, дыры в тестах, вещи,
которые прошлые задачи осознанно оставили на потом.

Требования к каждой задаче:
- ПЕРЕД созданием найди, нет ли такой уже: поиском и по открытым, и по
  закрытым issue. Есть — не заводи, это главный источник мусора;
- задача должна быть выполнима в ЭТОМ репозитории целиком. Всё, что
  упирается в {partner_repo}, заводить нельзя:
  для этого у цикла есть собственный механизм NEEDS-PARTNER;
- размер — одна задача на один PR, а не эпик;
- в теле: что не так сейчас, что должно стать, как проверить результат;
- создавать так: gh issue create --label {task_label} --title '...' --body '...'

ЧТО НЕЛЬЗЯ: менять код, коммитить, пушить, трогать существующие issue и PR,
заводить задачи в {partner_repo}, превышать потолок.

В конце выведи список заведённого: номер, заголовок, одна строка обоснования."
        )
    }

    pub fn tg_triaged(repo: &str, after: usize, before: usize) -> String {
        format!("🧹 AI dev loop ({repo}): разбор бэклога вернул в очередь {after} задач (было {before}). Новые не заводил.")
    }

    pub fn tg_created(repo: &str, new: usize, max_new_tasks: u32) -> String {
        format!("🌱 AI dev loop ({repo}): очередь была пуста, смотритель завёл {new} задач (потолок {max_new_tasks}).")
    }

    pub fn tg_crash(repo: &str, context: &str) -> String {
        format!("🛑 AI dev loop: смотритель бэклога ({repo}) упал: {context}.")
    }
}
