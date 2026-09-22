//! Отмена по сигналу — главное, что даёт перенос.
//!
//! Bash умирал от SIGTERM посреди `sleep 60`: метки не проставлялись,
//! задача возвращалась в очередь и держала всё за собой. Здесь ожидания
//! идут под `tokio::select!` вместе с сигналом, и по SIGTERM итерация не
//! умирает, а доводит уборку до конца.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

/// Итерацию попросили остановиться.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("получен сигнал остановки")]
pub struct Cancelled;

#[derive(Debug, Clone)]
pub struct Shutdown {
    /// Отправитель держим при себе: если канал закроется, `wait_for`
    /// вернёт ошибку немедленно, и любое ожидание будет выглядеть как
    /// отмена — итерация обрывалась бы на первой же паузе.
    tx: Arc<watch::Sender<bool>>,
    rx: watch::Receiver<bool>,
}

impl Shutdown {
    /// Слушать SIGTERM (`systemctl stop`) и SIGINT (Ctrl-C).
    pub fn listen() -> Self {
        let (tx, rx) = watch::channel(false);
        let tx = Arc::new(tx);
        let signal_tx = tx.clone();
        tokio::spawn(async move {
            let mut term =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::info!("⚠️ Не удалось подписаться на SIGTERM: {e}");
                        return;
                    }
                };
            tokio::select! {
                _ = term.recv() => tracing::info!("Получен SIGTERM — сворачиваю итерацию аккуратно."),
                _ = tokio::signal::ctrl_c() => tracing::info!("Получен SIGINT — сворачиваю итерацию аккуратно."),
            }
            let _ = signal_tx.send(true);
        });
        Self { tx, rx }
    }

    /// Заглушка: сигнала не будет.
    pub fn never() -> Self {
        let (tx, rx) = watch::channel(false);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    /// Попросить итерацию остановиться. Нужно тестам и ручной остановке
    /// изнутри процесса.
    pub fn trigger(&self) {
        let _ = self.tx.send(true);
    }

    /// Уже попросили остановиться?
    pub fn triggered(&self) -> bool {
        *self.rx.borrow()
    }

    /// Ждать сигнала. Если он уже был — возвращается сразу.
    pub async fn wait(&self) {
        let mut rx = self.rx.clone();
        if *rx.borrow() {
            return;
        }
        if rx.wait_for(|v| *v).await.is_err() {
            // Канал закрыт: сигнала уже никогда не будет, и «ждать» здесь
            // честнее, чем сообщить о ложной отмене.
            std::future::pending::<()>().await;
        }
    }

    /// Пауза, прерываемая сигналом.
    pub async fn sleep(
        &self,
        clock: &dyn crate::clock::Clock,
        dur: Duration,
    ) -> Result<(), Cancelled> {
        if self.triggered() {
            return Err(Cancelled);
        }
        tokio::select! {
            biased;
            _ = self.wait() => Err(Cancelled),
            _ = clock.sleep(dur) => Ok(()),
        }
    }

    /// Выполнить операцию, прервав её по сигналу.
    pub async fn guard<F, T>(&self, fut: F) -> Result<T, Cancelled>
    where
        F: std::future::Future<Output = T>,
    {
        if self.triggered() {
            return Err(Cancelled);
        }
        tokio::select! {
            biased;
            _ = self.wait() => Err(Cancelled),
            v = fut => Ok(v),
        }
    }
}
