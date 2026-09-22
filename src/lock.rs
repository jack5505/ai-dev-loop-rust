//! Замок итерации: `flock -n` на `LOCK_FILE`, общий у оркестратора и
//! смотрителя бэклога (**C10**). Занято — это не ошибка: код возврата 0.
//!
//! Используем тот же системный вызов, что и bash (`flock(2)`, LOCK_EX |
//! LOCK_NB), поэтому боевой bash и Rust-бинарник видят замок друг друга —
//! это обязательное условие для недели работы «в тени».

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result};

/// RAII-гвард: замок держится, пока жив объект.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

/// `Ok(None)` — замок занят другим процессом.
pub fn try_acquire(path: &Path) -> Result<Option<Lock>> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("не открыть файл замка {}", path.display()))?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(Lock { _file: file }));
    }

    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Ok(None),
        _ => Err(anyhow::Error::new(err).context(format!("flock на {} не удался", path.display()))),
    }
}

/// Кто-то держит замок? Ответ без его захвата — для `ai-dev status`.
pub fn is_busy(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    Ok(try_acquire(path)?.is_none())
}
