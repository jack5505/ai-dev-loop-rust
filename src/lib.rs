//! AI dev loop — перенос `orchestrator-ci.sh` и `backlog-keeper.sh` на Rust.
//!
//! Внешние миры вынесены в трейты (`gh::GitHub`, `agent::Agent`,
//! `notify::Notifier`, `clock::Clock`) — отсюда тесты без сети и `--dry-run`
//! как декорирующая реализация, а не `if` по всему коду.

pub mod agent;
pub mod clock;
pub mod config;
pub mod exec;
pub mod gh;
pub mod git;
pub mod keeper;
pub mod lock;
pub mod logging;
pub mod markers;
pub mod notify;
pub mod orchestrator;
pub mod prompts;
pub mod queue;
pub mod shutdown;
pub mod state;

/// Коды возврата процесса (§6 спецификации).
pub mod exit {
    /// Штатное завершение: в том числе «очередь пуста», «замок занят»,
    /// «ушло человеку» — всё это не аварии.
    pub const OK: i32 = 0;
    /// Авария: оркестратор упал.
    pub const CRASH: i32 = 1;
    /// Ошибка конфигурации — не повод для алерта «оркестратор упал».
    pub const CONFIG: i32 = 2;
}
