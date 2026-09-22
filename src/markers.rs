//! Маркеры в комментариях и телах issue — самое дорогое место переноса.
//! Каждая функция здесь закрывает контракт из §4 спецификации, и каждый
//! контракт оплачен инцидентом, поэтому логика вынесена в чистые функции
//! и покрыта фикстурами реальных поломок.

use chrono::{DateTime, Utc};
use regex::Regex;
use std::sync::OnceLock;

use crate::gh::model::Comment;

fn blocked_by_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"BLOCKED-BY: [^#\s]+#[0-9]+").unwrap())
}

fn task_ref_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:AI-TASK: #|Closes #)([0-9]+)").unwrap())
}

/// **C2** — сигнал агента в issue.
///
/// Маркер ищем ТОЛЬКО в комментариях агента (не самого оркестратора) и
/// ТОЛЬКО в начале строки: инструктаж оркестратора сам содержит слова
/// «NEEDS-PARTNER:»/«CANNOT-FIX-HERE:» по-русски, и без этих двух условий
/// задача блокировалась через ~60 секунд после старта, ещё до того как агент
/// успевал ответить. Фильтр по времени добавлен после дубля у партнёра:
/// задача, вернувшаяся из-под `blocked`, натыкалась на свой же прошлый
/// вердикт и заводила задачу повторно.
pub fn find_agent_marker(
    comments: &[Comment],
    marker: &str,
    self_login: &str,
    since: DateTime<Utc>,
) -> bool {
    agent_comments_since(comments, self_login, since)
        .any(|c| starts_line_with_marker(&c.body, marker))
}

/// Тела комментариев агента после момента поручения — в порядке появления.
pub fn agent_comments_since<'a>(
    comments: &'a [Comment],
    self_login: &'a str,
    since: DateTime<Utc>,
) -> impl Iterator<Item = &'a Comment> {
    comments.iter().filter(move |c| {
        c.author.login != self_login
            && match c.created_at {
                Some(at) => at >= since,
                // Без даты сравнивать нечем: считаем комментарий старым,
                // как это делал `fromdateiso8601` на пустом значении (ошибка → не подходит).
                None => false,
            }
    })
}

/// Строка начинается с маркера, допускаются ведущие пробелы —
/// ровно как `grep -qE "^[[:space:]]*MARKER"`.
pub fn starts_line_with_marker(body: &str, marker: &str) -> bool {
    body.lines()
        .any(|line| line.trim_start().starts_with(marker))
}

/// **C3** — `@claude` закончил работу.
///
/// Автор ровно `claude`, комментарий появился после поручения, в теле —
/// «Claude finished». Без этой проверки задача с ответом за 36 секунд
/// висела все `APP_WAIT_MIN` минут и возвращалась в очередь бесконечно.
pub fn claude_finished_since(comments: &[Comment], since: DateTime<Utc>) -> bool {
    comments.iter().any(|c| {
        c.author.login == "claude"
            && c.created_at.map(|at| at >= since).unwrap_or(false)
            && c.body.contains("Claude finished")
    })
}

/// Последний маркер `BLOCKED-BY: owner/repo#N` в тексте → `owner/repo#N`.
/// Аналог `grep -oE ... | tail -n1`.
pub fn last_blocked_by(text: &str) -> Option<String> {
    blocked_by_re()
        .find_iter(text)
        .map(|m| m.as_str().trim_start_matches("BLOCKED-BY: ").to_string())
        .last()
}

/// Ссылка `owner/repo#N` → (`owner/repo`, `N`).
pub fn split_ref(reference: &str) -> Option<(&str, u64)> {
    let (repo, num) = reference.rsplit_once('#')?;
    Some((repo, num.parse().ok()?))
}

/// Сколько строк содержат `BLOCKED-BY:` — аналог `grep -c`.
/// Два и более маркера означают, что задача уже блокировалась дважды,
/// и включают защиту от пинг-понга.
pub fn count_blocked_by_lines(text: &str) -> usize {
    text.lines().filter(|l| l.contains("BLOCKED-BY:")).count()
}

/// Задача пришла из партнёрского репозитория? Маркер `ORIGIN: owner/repo#N`
/// в теле issue.
pub fn has_origin_from(body: &str, partner_repo: &str) -> bool {
    let re = Regex::new(&format!(r"ORIGIN: {}#[0-9]+", regex::escape(partner_repo)))
        .expect("экранированный шаблон");
    re.is_match(body)
}

/// Подробности для задачи у партнёра: строка с `NEEDS-PARTNER:` и до 20
/// строк после неё — аналог `grep -m1 -A20`.
pub fn needs_partner_details(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    match lines.iter().position(|l| l.contains("NEEDS-PARTNER:")) {
        Some(i) => {
            let end = (i + 21).min(lines.len());
            lines[i..end].join("\n")
        }
        None => String::new(),
    }
}

/// Номер задачи из тела PR: `AI-TASK: #N` либо `Closes #N`, первый найденный.
pub fn task_number_in_body(body: &str) -> Option<u64> {
    task_ref_re()
        .captures(body)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

/// Тело PR ссылается на задачу цикла? Фильтр сторожа открытых PR (§2.2).
pub fn body_refers_to_task(body: &str) -> bool {
    task_ref_re().is_match(body)
}

/// Вердикт ревью. Последняя строка ответа обязана быть одной из двух,
/// но исторически цикл искал подстроку в любом месте — сохраняем это
/// поведение, чтобы порт не начал расходиться с bash на первом же ревью.
pub fn review_approved(review: &str) -> bool {
    review.contains("VERDICT: APPROVE")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
    }

    const SELF: &str = "jack5505";

    #[test]
    fn c2_orchestrator_instruction_is_not_a_signal() {
        // Реальный инцидент: в инструктаже оркестратора сам текст маркера
        // упомянут по-русски и в середине строки — ловиться он не должен.
        let comments = vec![Comment::new(
            SELF,
            "@claude Реализуй задачу. Если причина не здесь — оставь комментарий, \
             начинающийся строкой NEEDS-PARTNER: с описанием.",
            at(10),
        )];
        assert!(!find_agent_marker(&comments, "NEEDS-PARTNER:", SELF, at(0)));
    }

    #[test]
    fn c2_marker_must_start_the_line() {
        let comments = vec![Comment::new(
            "claude",
            "Я посмотрел логи и думаю, что это NEEDS-PARTNER: их API отдаёт 500",
            at(10),
        )];
        assert!(!find_agent_marker(&comments, "NEEDS-PARTNER:", SELF, at(0)));

        let comments = vec![Comment::new(
            "claude",
            "Разобрался.\n  NEEDS-PARTNER: их API отдаёт 500 на /v1/me",
            at(10),
        )];
        assert!(find_agent_marker(&comments, "NEEDS-PARTNER:", SELF, at(0)));
    }

    #[test]
    fn c2_old_marker_does_not_fire_again() {
        // android#226 → mahalla#217, через 6 суток он же → mahalla#272:
        // старый вердикт срабатывал снова после возврата из blocked.
        let comments = vec![Comment::new(
            "claude",
            "NEEDS-PARTNER: контракт не совпадает",
            at(-86400),
        )];
        assert!(!find_agent_marker(&comments, "NEEDS-PARTNER:", SELF, at(0)));
        assert!(find_agent_marker(
            &comments,
            "NEEDS-PARTNER:",
            SELF,
            at(-90000)
        ));
    }

    #[test]
    fn c3_claude_finished_needs_author_time_and_text() {
        let good = vec![Comment::new(
            "claude",
            "**Claude finished** its work",
            at(36),
        )];
        assert!(claude_finished_since(&good, at(0)));

        let wrong_author = vec![Comment::new(SELF, "Claude finished", at(36))];
        assert!(!claude_finished_since(&wrong_author, at(0)));

        let too_old = vec![Comment::new("claude", "Claude finished", at(-10))];
        assert!(!claude_finished_since(&too_old, at(0)));

        let other_text = vec![Comment::new("claude", "работаю…", at(36))];
        assert!(!claude_finished_since(&other_text, at(0)));
    }

    #[test]
    fn last_blocked_by_wins() {
        let text = "BLOCKED-BY: jack5505/mahalla#100\nпотом\nBLOCKED-BY: jack5505/mahalla#217";
        assert_eq!(
            last_blocked_by(text).as_deref(),
            Some("jack5505/mahalla#217")
        );
        assert_eq!(last_blocked_by("метка blocked без маркера"), None);
        assert_eq!(
            split_ref("jack5505/mahalla#217"),
            Some(("jack5505/mahalla", 217))
        );
    }

    #[test]
    fn pingpong_guard_counts_markers() {
        let text = "BLOCKED-BY: a/b#1\nтекст\nBLOCKED-BY: a/b#2";
        assert_eq!(count_blocked_by_lines(text), 2);
        assert!(has_origin_from(
            "ORIGIN: jack5505/mahalla#12\nдетали",
            "jack5505/mahalla"
        ));
        assert!(!has_origin_from(
            "ORIGIN: other/repo#12",
            "jack5505/mahalla"
        ));
    }

    #[test]
    fn c4_task_number_from_pr_body() {
        assert_eq!(task_number_in_body("AI-TASK: #314\nCloses #314"), Some(314));
        // PR #315 в mahalla-android получил только Closes — и стал невидим.
        assert_eq!(task_number_in_body("Closes #314"), Some(314));
        assert_eq!(task_number_in_body("просто описание"), None);
        assert!(body_refers_to_task("Closes #1"));
        assert!(!body_refers_to_task("fixes #1"));
    }

    #[test]
    fn needs_partner_details_takes_marker_and_20_lines() {
        let mut text = String::from("шум\nNEEDS-PARTNER: суть\n");
        for i in 0..30 {
            text.push_str(&format!("строка {i}\n"));
        }
        let details = needs_partner_details(&text);
        assert!(details.starts_with("NEEDS-PARTNER: суть"));
        assert_eq!(details.lines().count(), 21);
        assert!(details.contains("строка 19"));
        assert!(!details.contains("строка 20"));
    }

    #[test]
    fn verdict_parsing() {
        assert!(review_approved("всё хорошо\nVERDICT: APPROVE"));
        assert!(!review_approved("есть замечания\nVERDICT: REQUEST_CHANGES"));
    }
}
