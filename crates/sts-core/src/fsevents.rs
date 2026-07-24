//! FSEvents 实时文件监听（notify 8）。
//! 监听常用目录，文件变更时置位 `force_update`，由既有索引循环拾取并增量重建。
//! 监听是「加速层」：初始化失败仅告警，索引仍有 1h 兜底全量扫描。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

/// 防抖窗口：短时间内大量事件（如批量复制）只触发一次重建
const DEBOUNCE_SECS: u64 = 5;

/// 启动后台监听线程。watcher 需常驻，故在独立线程中 park 保活。
/// `paths` 为要递归监听的目录；`force_update` 与索引循环共享。
pub fn start_watching(paths: Vec<String>, force_update: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        // 上次触发时间，用于防抖（初值置于过去，保证首个事件立即生效）
        let last_trigger = Arc::new(Mutex::new(
            Instant::now()
                .checked_sub(Duration::from_secs(DEBOUNCE_SECS + 1))
                .unwrap_or_else(Instant::now),
        ));
        let force = force_update.clone();
        let lt = last_trigger.clone();

        let handler = move |res: Result<Event, notify::Error>| {
            if res.is_ok() {
                let mut guard = lt.lock().unwrap();
                if guard.elapsed() >= Duration::from_secs(DEBOUNCE_SECS) {
                    *guard = Instant::now();
                    force.store(true, Ordering::SeqCst);
                }
            }
        };

        let mut watcher: RecommendedWatcher = match notify::recommended_watcher(handler) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[sts] FSEvents 初始化失败，退回定时全量扫描: {}", e);
                return;
            }
        };

        let mut watched = 0usize;
        for p in &paths {
            if Path::new(p).exists() {
                match watcher.watch(Path::new(p), RecursiveMode::Recursive) {
                    Ok(_) => watched += 1,
                    Err(e) => eprintln!("[sts] 监听 {} 失败: {}", p, e),
                }
            }
        }
        eprintln!("[sts] FSEvents 实时监听已启动，覆盖 {} 个目录", watched);

        // watcher 一旦 drop 即停止监听，故 park 线程保活
        loop {
            std::thread::park();
        }
    });
}
