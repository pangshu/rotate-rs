//! 回归测试（2026-10-08 排查的三个轮转缺陷）：
//! 1. 应用空闲时，到点的定时轮转不得产出空归档；
//! 2. 时间轮转的实际周期应与配置一致（不得因定时基准漂移翻倍）；
//! 3. 运行期活跃文件被外部删除后，轮转不得永久失败（应重建活跃文件）。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use rotate_rs::{Config, Error, ErrorHandler, Mode, NonBlockingConfig, Rotation, open};

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rotate-rs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 目录内的归档文件名（排除活跃文件 app.log）
fn archives(dir: &PathBuf) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "app.log")
        .collect();
    v.sort();
    v
}

fn cfg(dir: &PathBuf, interval: Duration) -> Config {
    Config {
        dir: dir.to_string_lossy().into_owned(),
        rotation: Rotation::Hybrid {
            max_size: 1 << 30, // 1GB：测试窗口内不会触发 size 轮转
            interval,
        },
        mode: Mode::NonBlocking(NonBlockingConfig::default()),
        max_backups: None,
        ..Default::default()
    }
}

/// 空闲（无写入）时，到点的定时轮转必须跳过 0 字节的活跃文件。
#[test]
fn idle_rotation_skips_empty_active_file() {
    let dir = tmp_dir("idle");
    let writer = open(cfg(&dir, Duration::from_secs(1))).unwrap();
    sleep(Duration::from_millis(3200)); // 覆盖 2~3 个轮转周期
    let files = archives(&dir);
    assert!(files.is_empty(), "空闲不应产生归档，实际: {files:?}");
    drop(writer);
}

/// 时间轮转的实际周期应与配置一致（修复前因定时基准漂移为 2 倍）。
///
/// 依赖真实时间：断言留有余量（6s / interval=1s 期望 ≥4 个归档；
/// 修复前只有 3 个，若 CI 负载极高可能误报，必要时可调大窗口）。
#[test]
fn time_rotation_period_matches_interval() {
    let dir = tmp_dir("cadence");
    let mut writer = open(cfg(&dir, Duration::from_secs(1))).unwrap();
    for _ in 0..120 {
        // 持续写入约 6s，保证每个周期都有内容可轮转
        writer.write_all(b"line\n").unwrap();
        sleep(Duration::from_millis(50));
    }
    writer.flush().unwrap();
    let files = archives(&dir);
    assert!(
        files.len() >= 4,
        "6s / interval=1s 期望至少 4 个归档，实际 {} 个: {files:?}",
        files.len()
    );
    drop(writer);
}

// ── 缺陷 3：运行期活跃文件被外部删除 ──────────────────────────────

/// 同步模式 + 小 size 阈值 + 收集错误回调上下文的配置。
/// 同步模式轮转检查内联在 `write()` 中，触发时机确定，便于断言。
fn cfg_sync_size(dir: &Path, max_size: usize) -> (Config, Arc<Mutex<Vec<String>>>) {
    let contexts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&contexts);
    let handler: ErrorHandler = Arc::new(move |ctx: &str, _e: &Error| {
        sink.lock().unwrap().push(ctx.to_string());
    });
    let config = Config {
        dir: dir.to_string_lossy().into_owned(),
        rotation: Rotation::Size(max_size),
        max_backups: None,
        mode: Mode::Sync,
        error_handler: Some(handler),
        ..Default::default()
    };
    (config, contexts)
}

/// 运行期活跃文件被外部删除后：轮转应"无物可归档 → 重建活跃文件"，
/// 而不是 rename 源不存在导致每次 write 报 `rotate-retry`（永久卡死）。
#[test]
fn deleted_active_file_is_recreated_on_rotation() {
    let dir = tmp_dir("deleted");
    let (config, contexts) = cfg_sync_size(&dir, 64);
    let mut writer = open(config).unwrap();

    // 累积到超过阈值（本函数轮转判定发生在写入之前，故此处尚未轮转）
    writer.write_all(&[b'a'; 100]).unwrap();
    writer.flush().unwrap();

    // 运行期删除活跃文件：句柄仍有效但目录项已消失（Windows 下句柄为
    // FILE_SHARE_DELETE，删除成功后 rename 的源即不存在）
    std::fs::remove_file(dir.join("app.log")).unwrap();
    assert!(!dir.join("app.log").exists(), "前置条件：活跃文件已被删除");

    // 本次 write 先触发轮转：应走缺失重建而非 rename 失败
    writer.write_all(b"after-delete\n").unwrap();
    writer.flush().unwrap();

    assert!(dir.join("app.log").exists(), "活跃文件应被重建");
    let content = std::fs::read(dir.join("app.log")).unwrap();
    assert_eq!(
        content, b"after-delete\n",
        "删除后的写入应落入重建的活跃文件"
    );

    let seen = contexts.lock().unwrap().clone();
    assert!(seen.is_empty(), "恢复过程不应上报错误，实际: {seen:?}");
    drop(writer);
}

/// 恢复之后轮转能力应保持正常：再次累积到阈值仍能产出归档（状态机未被破坏）。
#[test]
fn rotation_still_works_after_recovery() {
    let dir = tmp_dir("recover-then-rotate");
    let (config, _contexts) = cfg_sync_size(&dir, 64);
    let mut writer = open(config).unwrap();

    writer.write_all(&[b'a'; 100]).unwrap();
    std::fs::remove_file(dir.join("app.log")).unwrap();

    // 触发缺失恢复（重建活跃文件，不产出归档）
    writer.write_all(b"x").unwrap();
    assert!(dir.join("app.log").exists(), "活跃文件应被重建");
    assert!(archives(&dir).is_empty(), "恢复本身不应产出归档");

    // 恢复后正常累积并轮转，应产出 1 个归档
    writer.write_all(&[b'b'; 100]).unwrap();
    writer.write_all(b"y").unwrap();
    writer.flush().unwrap();

    let files = archives(&dir);
    assert_eq!(files.len(), 1, "恢复后应能正常产出归档，实际: {files:?}");
    drop(writer);
}
