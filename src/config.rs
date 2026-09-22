//! Конфигурация. Файлы остаются те же, что у bash-версии: `KEY=value`,
//! права 0600 — такой формат читают и systemd (`EnvironmentFile=`), и мы.
//!
//! Приоритет: переменная окружения важнее файла. Под systemd переменные
//! уже в окружении, а `--instance` нужен для ручного запуска.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::exec::systemctl;

/// Где Claude думает над задачей.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevMode {
    /// Claude работает на этом сервере.
    Local,
    /// Код пишет `@claude` на раннерах GitHub Actions.
    GithubApp,
}

impl DevMode {
    pub fn as_str(self) -> &'static str {
        match self {
            DevMode::Local => "local",
            DevMode::GithubApp => "github-app",
        }
    }
}

/// Аутентификация агента: ровно одно из двух.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAuth {
    /// Подписка Pro/Max: `CLAUDE_CODE_OAUTH_TOKEN`.
    Oauth,
    /// `ANTHROPIC_API_KEY` — только с ним осмысленен `--max-budget-usd`.
    ApiKey,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub instance: Option<String>,
    pub source: Option<PathBuf>,

    pub repo_dir: PathBuf,
    pub base_branch: String,
    pub task_label: String,
    pub human_label: String,
    pub blocked_label: String,
    pub max_iterations: u32,
    pub max_review_rounds: u32,
    pub pr_stale_days: i64,
    pub max_budget_usd: String,
    pub claude_model: String,
    pub auto_merge: bool,
    pub ci_start_wait: u64,
    pub partner_repo: Option<String>,
    /// Пусто = только владелец gh-токена. Разворачивается в `SELF_LOGIN`
    /// на старте итерации (**C1**).
    pub allowed_authors: Vec<String>,
    pub dev_mode: DevMode,
    pub app_wait_min: u64,
    pub lock_file: PathBuf,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    pub max_new_tasks: u32,
    pub keeper_interval_days: u64,
    pub auth: Option<AgentAuth>,
}

/// Результат проверки конфигурации: ошибки и предупреждения раздельно,
/// потому что ошибка конфигурации — это код возврата 2, а не алерт
/// «оркестратор упал».
#[derive(Debug, Default)]
pub struct Report {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

impl Default for Config {
    /// Дефолты из §15 приложения А. Нужны тестам и как единственное место,
    /// где эти значения записаны в коде.
    fn default() -> Self {
        Self {
            instance: None,
            source: None,
            repo_dir: PathBuf::from("."),
            base_branch: "main".into(),
            task_label: "ai-task".into(),
            human_label: "needs-human".into(),
            blocked_label: "blocked".into(),
            max_iterations: 3,
            max_review_rounds: 2,
            pr_stale_days: 3,
            max_budget_usd: "5".into(),
            claude_model: "sonnet".into(),
            auto_merge: false,
            ci_start_wait: 30,
            partner_repo: None,
            allowed_authors: Vec::new(),
            dev_mode: DevMode::Local,
            app_wait_min: 180,
            lock_file: PathBuf::from("/tmp/ai-dev.lock"),
            telegram_bot_token: None,
            telegram_chat_id: None,
            max_new_tasks: 5,
            keeper_interval_days: 7,
            auth: None,
        }
    }
}

impl Config {
    /// Загрузить конфигурацию инстанса.
    pub fn load(instance: Option<&str>) -> Result<Self> {
        let mut file_vars = BTreeMap::new();
        let mut source = None;
        if let Some(name) = instance {
            if let Some(path) = find_instance_file(name) {
                file_vars =
                    parse_env_file(&path).with_context(|| format!("разбор {}", path.display()))?;
                source = Some(path);
            } else {
                bail!(
                    "конфиг инстанса «{name}» не найден: искал {}",
                    candidate_files(name)
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }

        let get = |key: &str| -> Option<String> {
            // Окружение важнее файла: под systemd переменные уже заданы.
            match std::env::var(key) {
                Ok(v) if !v.is_empty() => Some(v),
                _ => file_vars.get(key).cloned().filter(|v| !v.is_empty()),
            }
        };
        let text = |key: &str, default: &str| get(key).unwrap_or_else(|| default.to_string());
        let num = |key: &str, default: u64| -> Result<u64> {
            match get(key) {
                Some(v) => v
                    .trim()
                    .parse::<u64>()
                    .with_context(|| format!("{key}={v}: ожидалось целое число")),
                None => Ok(default),
            }
        };

        let repo_dir =
            PathBuf::from(get("REPO_DIR").context("задайте REPO_DIR — путь к клону репозитория")?);

        let dev_mode = match text("DEV_MODE", "local").as_str() {
            "local" => DevMode::Local,
            "github-app" => DevMode::GithubApp,
            other => bail!("DEV_MODE={other}: ожидалось local или github-app"),
        };

        // Замок уникален для репозитория — циклы двух репо не мешают друг другу.
        let lock_file = match get("LOCK_FILE") {
            Some(v) => PathBuf::from(v),
            None => {
                let base = repo_dir
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "repo".to_string());
                PathBuf::from(format!("/tmp/ai-dev-{base}.lock"))
            }
        };

        let oauth = get("CLAUDE_CODE_OAUTH_TOKEN");
        let api_key = get("ANTHROPIC_API_KEY");
        let auth = match (oauth.is_some(), api_key.is_some()) {
            (true, false) => Some(AgentAuth::Oauth),
            (false, true) => Some(AgentAuth::ApiKey),
            _ => None,
        };

        Ok(Config {
            instance: instance.map(|s| s.to_string()),
            source,
            repo_dir,
            base_branch: text("BASE_BRANCH", "main"),
            task_label: text("TASK_LABEL", "ai-task"),
            human_label: text("HUMAN_LABEL", "needs-human"),
            blocked_label: text("BLOCKED_LABEL", "blocked"),
            max_iterations: num("MAX_ITERATIONS", 3)? as u32,
            max_review_rounds: num("MAX_REVIEW_ROUNDS", 2)? as u32,
            pr_stale_days: num("PR_STALE_DAYS", 3)? as i64,
            max_budget_usd: text("MAX_BUDGET_USD", "5"),
            claude_model: text("CLAUDE_MODEL", "sonnet"),
            auto_merge: text("AUTO_MERGE", "false") == "true",
            ci_start_wait: num("CI_START_WAIT", 30)?,
            partner_repo: get("PARTNER_REPO"),
            allowed_authors: get("ALLOWED_AUTHORS")
                .map(|v| v.split_whitespace().map(|s| s.to_string()).collect())
                .unwrap_or_default(),
            dev_mode,
            app_wait_min: num("APP_WAIT_MIN", 180)?,
            lock_file,
            telegram_bot_token: get("TELEGRAM_BOT_TOKEN"),
            telegram_chat_id: get("TELEGRAM_CHAT_ID"),
            max_new_tasks: num("MAX_NEW_TASKS", 5)? as u32,
            keeper_interval_days: num("KEEPER_INTERVAL_DAYS", 7)?,
            auth,
        })
    }

    pub fn log_dir(&self) -> PathBuf {
        self.repo_dir.join(".ai-logs")
    }

    pub fn repo_basename(&self) -> String {
        self.repo_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.repo_dir.display().to_string())
    }

    pub fn telegram(&self) -> Option<(&str, &str)> {
        match (&self.telegram_bot_token, &self.telegram_chat_id) {
            (Some(t), Some(c)) => Some((t.as_str(), c.as_str())),
            _ => None,
        }
    }

    /// Проверки, не требующие внешних процессов.
    pub fn validate(&self) -> Report {
        let mut r = Report::default();

        if !self.repo_dir.is_dir() {
            r.errors.push(format!(
                "REPO_DIR={} — каталога нет",
                self.repo_dir.display()
            ));
        } else if !self.repo_dir.join(".git").exists() {
            r.errors.push(format!(
                "REPO_DIR={} — это не git-репозиторий (нет .git)",
                self.repo_dir.display()
            ));
        }

        match self.auth {
            None => r.errors.push(
                "нужен ровно один из CLAUDE_CODE_OAUTH_TOKEN и ANTHROPIC_API_KEY \
                 (сейчас заданы оба или ни одного)"
                    .to_string(),
            ),
            Some(AgentAuth::ApiKey) => {
                if self.max_budget_usd.trim().parse::<f64>().is_err() {
                    r.errors.push(format!(
                        "MAX_BUDGET_USD={} — ожидалось число",
                        self.max_budget_usd
                    ));
                }
            }
            Some(AgentAuth::Oauth) => {}
        }

        match (&self.telegram_bot_token, &self.telegram_chat_id) {
            (Some(_), None) => r
                .errors
                .push("TELEGRAM_BOT_TOKEN задан, а TELEGRAM_CHAT_ID нет".to_string()),
            (None, Some(_)) => r
                .errors
                .push("TELEGRAM_CHAT_ID задан, а TELEGRAM_BOT_TOKEN нет".to_string()),
            (None, None) => r
                .warnings
                .push("Telegram не настроен — уведомления выключены".to_string()),
            (Some(_), Some(_)) => {}
        }

        if let Some(p) = &self.partner_repo {
            let parts: Vec<&str> = p.split('/').collect();
            if parts.len() != 2 || parts.iter().any(|s| s.is_empty()) {
                r.errors
                    .push(format!("PARTNER_REPO={p} — ожидался формат owner/name"));
            }
        } else {
            r.warnings
                .push("PARTNER_REPO не задан — межрепозиторная блокировка выключена".to_string());
        }

        if self.max_iterations == 0 {
            r.errors
                .push("MAX_ITERATIONS=0 — CI не будет проверен".to_string());
        }
        if self.max_review_rounds == 0 {
            r.errors
                .push("MAX_REVIEW_ROUNDS=0 — ревью не будет выполнено".to_string());
        }

        r
    }

    /// **C9**: `APP_WAIT_MIN` обязан быть строго меньше `TimeoutStartSec`
    /// юнита, иначе SIGTERM прилетает посреди ожидания.
    ///
    /// Юниты бывают системные и пользовательские (rootless-раскладка), поэтому
    /// спрашиваем сначала `systemctl`, затем `systemctl --user`.
    pub async fn check_unit_timeout(&self, report: &mut Report) {
        if self.dev_mode != DevMode::GithubApp {
            return;
        }
        let Some(instance) = &self.instance else {
            report
                .warnings
                .push("C9 не проверен: инстанс не назван (нужен --instance <name>)".to_string());
            return;
        };
        let unit = format!("ai-dev@{instance}.service");
        let Some(timeout_sec) = unit_timeout_sec(&unit).await else {
            report.warnings.push(format!(
                "C9 не проверен: не удалось прочитать TimeoutStartSec у {unit}"
            ));
            return;
        };
        let wait_sec = self.app_wait_min * 60;
        if wait_sec >= timeout_sec {
            report.errors.push(format!(
                "C9: APP_WAIT_MIN={} мин ({} с) ≥ TimeoutStartSec={} с у {unit} — \
                 SIGTERM прилетит посреди ожидания, метка needs-human не будет поставлена",
                self.app_wait_min, wait_sec, timeout_sec
            ));
        } else if timeout_sec - wait_sec < 30 * 60 {
            report.warnings.push(format!(
                "C9: запас между APP_WAIT_MIN ({} с) и TimeoutStartSec ({} с) меньше 30 минут",
                wait_sec, timeout_sec
            ));
        }
    }
}

/// `TimeoutStartSec` юнита — но только той области, где юнит реально
/// загружен. Для незнакомого юнита `systemctl show` молча отдаёт значения
/// по умолчанию (90 с), и проверка C9 давала ложную ошибку на сервере,
/// где юниты пользовательские.
async fn unit_timeout_sec(unit: &str) -> Option<u64> {
    for user_scope in [false, true] {
        let out = systemctl(user_scope)
            .args(["show", unit, "-p", "LoadState", "-p", "TimeoutStartUSec"])
            .output()
            .await;
        let Ok(out) = out else { continue };
        if !out.ok() {
            continue;
        }
        let props = parse_env_str(out.trimmed());
        if props.get("LoadState").map(|s| s.as_str()) != Some("loaded") {
            continue;
        }
        if let Some(value) = props.get("TimeoutStartUSec") {
            if let Some(secs) = parse_systemd_duration(value) {
                return Some(secs);
            }
        }
    }
    None
}

/// systemd печатает `4h`, `1min 30s`, `infinity`, иногда `4h 0min`.
pub fn parse_systemd_duration(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value == "infinity" {
        return Some(u64::MAX);
    }
    let mut total: u64 = 0;
    let mut seen = false;
    let mut digits = String::new();
    let mut unit = String::new();

    let flush =
        |digits: &mut String, unit: &mut String, total: &mut u64, seen: &mut bool| -> bool {
            if digits.is_empty() {
                return true;
            }
            let n: u64 = match digits.parse() {
                Ok(n) => n,
                Err(_) => return false,
            };
            let mult = match unit.as_str() {
                "us" | "usec" => {
                    *total += n / 1_000_000;
                    *seen = true;
                    digits.clear();
                    unit.clear();
                    return true;
                }
                "ms" | "msec" => {
                    *total += n / 1_000;
                    *seen = true;
                    digits.clear();
                    unit.clear();
                    return true;
                }
                "" | "s" | "sec" | "seconds" => 1,
                "min" | "m" => 60,
                "h" | "hr" => 3600,
                "d" => 86400,
                "w" => 604800,
                _ => return false,
            };
            *total += n * mult;
            *seen = true;
            digits.clear();
            unit.clear();
            true
        };

    for ch in value.chars() {
        if ch.is_ascii_digit() {
            if !unit.is_empty() && !flush(&mut digits, &mut unit, &mut total, &mut seen) {
                return None;
            }
            digits.push(ch);
        } else if ch.is_ascii_alphabetic() {
            unit.push(ch);
        } else if ch.is_whitespace() {
            if !flush(&mut digits, &mut unit, &mut total, &mut seen) {
                return None;
            }
        } else {
            return None;
        }
    }
    if !flush(&mut digits, &mut unit, &mut total, &mut seen) {
        return None;
    }
    if seen {
        Some(total)
    } else {
        None
    }
}

/// Где искать `ai-dev-<instance>.env`.
///
/// Первый путь — как в DEPLOY.md. Остальные нужны rootless-установке, где
/// у пользователя нет пароля sudo и `/etc` ему недоступен.
pub fn candidate_files(instance: &str) -> Vec<PathBuf> {
    let name = format!("ai-dev-{instance}.env");
    let mut out = Vec::new();
    if let Some(dir) = std::env::var_os("AI_DEV_CONFIG_DIR") {
        out.push(PathBuf::from(dir).join(&name));
    }
    out.push(PathBuf::from("/etc").join(&name));
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        out.push(PathBuf::from(dir).join(&name));
    }
    if let Some(home) = std::env::var_os("HOME") {
        out.push(PathBuf::from(home).join(".config").join(&name));
    }
    out
}

fn find_instance_file(instance: &str) -> Option<PathBuf> {
    candidate_files(instance).into_iter().find(|p| p.is_file())
}

/// Каталоги, где ищутся конфиги инстансов, — в порядке приоритета.
pub fn instance_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("AI_DEV_CONFIG_DIR") {
        dirs.push(PathBuf::from(d));
    }
    dirs.push(PathBuf::from("/etc"));
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME") {
        dirs.push(PathBuf::from(d));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".config"));
    }
    dirs
}

/// Какие инстансы есть на этой машине: `ai-dev-<имя>.env` в каталогах
/// поиска. Нужно и `doctor`, и подсказке «укажите --instance».
pub fn known_instances() -> Vec<(String, PathBuf)> {
    known_instances_in(&instance_dirs())
}

pub fn known_instances_in(dirs: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(rest) = name.strip_prefix("ai-dev-") else {
                continue;
            };
            let Some(instance) = rest.strip_suffix(".env") else {
                continue;
            };
            if instance.is_empty() || found.iter().any(|(i, _)| i == instance) {
                continue;
            }
            found.push((instance.to_string(), entry.path()));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Разбор `KEY=value` в том объёме, в каком его понимает systemd
/// `EnvironmentFile`: комментарии, пустые строки, необязательные кавычки.
pub fn parse_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("не прочитать {}", path.display()))?;
    Ok(parse_env_str(&raw))
}

pub fn parse_env_str(raw: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let value = value.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        out.insert(key.to_string(), value.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_like_systemd() {
        let vars = parse_env_str(
            "# комментарий\n\nREPO_DIR=/home/aidev/backend\n\
             ALLOWED_AUTHORS=jack5505 app/claude\n\
             export DEV_MODE=github-app\n\
             QUOTED=\"значение\"\n\
             #TELEGRAM_CHAT_ID=57825643\n\
             TELEGRAM_CHAT_ID=1\n",
        );
        assert_eq!(vars["REPO_DIR"], "/home/aidev/backend");
        assert_eq!(vars["ALLOWED_AUTHORS"], "jack5505 app/claude");
        assert_eq!(vars["DEV_MODE"], "github-app");
        assert_eq!(vars["QUOTED"], "значение");
        // Закомментированная строка не должна побеждать настоящую.
        assert_eq!(vars["TELEGRAM_CHAT_ID"], "1");
    }

    #[test]
    fn instances_are_found_by_config_name() {
        let dir = std::env::temp_dir().join(format!("ai-dev-inst-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["ai-dev-backend.env", "ai-dev-android.env", "прочее.env"] {
            std::fs::write(dir.join(name), b"REPO_DIR=/tmp\n").unwrap();
        }
        let found = known_instances_in(std::slice::from_ref(&dir));
        let names: Vec<&str> = found.iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(names, vec!["android", "backend"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn systemd_durations() {
        assert_eq!(parse_systemd_duration("4h"), Some(14400));
        assert_eq!(parse_systemd_duration("4h 0min"), Some(14400));
        assert_eq!(parse_systemd_duration("1min 30s"), Some(90));
        assert_eq!(parse_systemd_duration("90"), Some(90));
        assert_eq!(parse_systemd_duration("14400000000us"), Some(14400));
        assert_eq!(parse_systemd_duration("infinity"), Some(u64::MAX));
        assert_eq!(parse_systemd_duration(""), None);
    }
}
