//! 回归测试（2026-10-08 排查的两个轮转缺陷）：
//! 1. 应用空闲时，到点的定时轮转不得产出空归档；
//! 2. 时间轮转的实际周期应与配置一致（不得因定时基准漂移翻倍）。

use std::io::Write;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use rotate_rs::{Config, Mode, NonBlockingConfig, Rotation, open};

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
