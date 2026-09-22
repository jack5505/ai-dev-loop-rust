//! Telegram-уведомления. Ошибка отправки никогда не роняет итерацию —
//! это поведение bash (`curl … || true`) и оно важно: алерт вторичен по
//! отношению к работе цикла.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

#[async_trait]
pub trait Notifier: Send + Sync {
    async fn send(&self, text: &str);
}

/// Telegram не настроен — молча ничего не делаем.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullNotifier;

#[async_trait]
impl Notifier for NullNotifier {
    async fn send(&self, _text: &str) {}
}

pub struct Telegram {
    client: reqwest::Client,
    token: String,
    chat_id: String,
}

impl Telegram {
    pub fn new(token: impl Into<String>, chat_id: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            token: token.into(),
            chat_id: chat_id.into(),
        }
    }
}

#[async_trait]
impl Notifier for Telegram {
    async fn send(&self, text: &str) {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let res = self
            .client
            .post(url)
            .form(&[("chat_id", self.chat_id.as_str()), ("text", text)])
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => tracing::info!("⚠️ Telegram ответил {}", r.status()),
            Err(e) => tracing::info!("⚠️ Telegram недоступен: {e}"),
        }
    }
}

/// Пишет уведомления в лог вместо сети: `--dry-run` и тесты.
#[derive(Debug, Default, Clone)]
pub struct RecordingNotifier(Arc<Mutex<Vec<String>>>);

impl RecordingNotifier {
    pub fn messages(&self) -> Vec<String> {
        self.0.lock().expect("журнал уведомлений").clone()
    }
}

#[async_trait]
impl Notifier for RecordingNotifier {
    async fn send(&self, text: &str) {
        tracing::info!("(dry-run) telegram: {}", text.lines().next().unwrap_or(""));
        self.0
            .lock()
            .expect("журнал уведомлений")
            .push(text.to_string());
    }
}
