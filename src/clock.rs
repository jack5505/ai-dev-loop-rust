//! Время — тоже внешний мир: подменяемый `Clock` даёт сценарные тесты без
//! реальных пауз в 60 секунд.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};

#[async_trait]
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    async fn sleep(&self, dur: Duration);
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    async fn sleep(&self, dur: Duration) {
        tokio::time::sleep(dur).await;
    }
}

/// Часы для тестов: `sleep` не ждёт, а проматывает время вперёд.
#[derive(Debug, Clone)]
pub struct TestClock {
    epoch_secs: Arc<AtomicI64>,
}

impl TestClock {
    pub fn new(start_secs: i64) -> Self {
        Self {
            epoch_secs: Arc::new(AtomicI64::new(start_secs)),
        }
    }

    pub fn advance(&self, secs: i64) {
        self.epoch_secs.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new(1_700_000_000)
    }
}

#[async_trait]
impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        Utc.timestamp_opt(self.epoch_secs.load(Ordering::SeqCst), 0)
            .single()
            .expect("корректная метка времени")
    }

    async fn sleep(&self, dur: Duration) {
        self.advance(dur.as_secs() as i64);
        tokio::task::yield_now().await;
    }
}
