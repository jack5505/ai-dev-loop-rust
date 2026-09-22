//! Формат лога сохраняем дословно от bash: `[%F %T] сообщение`.
//! Рецепты grep по journalctl продолжают работать, поэтому ни уровень,
//! ни target в строку не попадают.

use std::fmt;

use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

struct BashTime;

impl FormatTime for BashTime {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        write!(w, "[{}]", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"))
    }
}

/// Инициализация подписчика. Вызывается один раз из `main`.
pub fn init() {
    let _ = tracing_subscriber::fmt()
        .with_timer(BashTime)
        .with_target(false)
        .with_level(false)
        .with_ansi(false)
        .try_init();
}
