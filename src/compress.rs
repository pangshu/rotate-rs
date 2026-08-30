//! gzip 单文件压缩：Windows 句柄兼容与失败残留清理（设计文档 §9）

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use flate2::write::GzEncoder;
use flate2::Compression;

use crate::engine::ErrorReporter;
use crate::Error;

/// 压缩单个文件为 `.gz` 并删除原文件
///
/// 三个关键防御（设计文档 §9.1 + R3-1）：
/// - Windows 兼容：作用域块限制输入/输出句柄生命周期，确保 `remove_file` 前锁已释放
///   （Windows 不允许删除打开中的文件）
/// - 失败清理：编码失败时删除残留 `.gz`，避免后续扫描误判为已压缩
/// - 防覆盖（R3-1）：目标 `.gz` 用 `create_new` 创建，已存在即失败保留原文件，
///   杜绝同名归档被静默截断；该失败不触发清理（不得误删既有归档）
pub(crate) fn compress_file(path: &Path) -> io::Result<()> {
    let gz_path = gz_path(path);
    // R3-1：create_new 防覆盖——目标 .gz 已存在时保留双方（原文件与既有归档），
    // 返回错误由调用方择机重试（如下次轮转补压）
    let output = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&gz_path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("压缩目标已存在，拒绝覆盖: {}", gz_path.display()),
            ));
        }
        Err(e) => return Err(e),
    };
    let result = (|| {
        // 作用域块：确保 input/output 句柄在块结束时释放（Windows 兼容）
        {
            let input = File::open(path)?;
            // CP-2：日志文本高重复，fast 级别压缩率损失小、CPU 开销降低数倍
            let mut encoder = GzEncoder::new(output, Compression::fast());
            io::copy(&mut BufReader::new(input), &mut encoder)?;
            encoder.finish()?; // 冲刷 gzip 尾部校验码
        } // input/output 句柄在此全部释放
        std::fs::remove_file(path)
    })();
    if result.is_err() {
        // 清理本次半成品 .gz（create_new 保证仅属于本次）
        let _ = std::fs::remove_file(&gz_path);
    }
    result
}

/// 压缩文件路径：原路径固定追加 ".gz"
pub(crate) fn gz_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".gz");
    PathBuf::from(s)
}

/// 压缩线程主体：串行处理压缩队列，单个失败不影响后续任务。
/// R4-3：失败经 ErrorReporter 上报（Error::Compress），不再静默吞掉；
/// 原文件保留，由后续轮转的 cleanup 补压重试
pub(crate) fn compress_worker(rx: Receiver<PathBuf>, reporter: Arc<ErrorReporter>) {
    while let Ok(path) = rx.recv() {
        if let Err(e) = compress_file(&path) {
            reporter.report("compress", &Error::Compress(e));
        }
    }
}
