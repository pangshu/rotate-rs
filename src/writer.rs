//! 统一 Writer 门面与唯一入口 open()（设计文档 §4.5 / §4.7）

use std::io;
use std::time::Duration;

use crate::channel::AsyncWriter;
use crate::config::{Config, Mode};
use crate::engine::Engine;
use crate::error::Error;
use crate::sync::SyncWriter;

/// 统一门面：两种模式都实现 `Write`，用户无需 match（设计文档 §4.5）
///
/// 变体大小差异是门面类型的可接受代价：用户持有单个实例，不频繁按值移动。
#[allow(clippy::large_enum_variant)]
pub enum Writer {
    Sync(SyncWriter),
    /// WorkerGuard 收纳在此变体内，writer 存活即 worker 存活
    NonBlocking(AsyncWriter, crate::channel::WorkerGuard),
}

impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Writer::Sync(_) => f.write_str("Writer::Sync(..)"),
            Writer::NonBlocking(..) => f.write_str("Writer::NonBlocking(..)"),
        }
    }
}

impl io::Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Writer::Sync(w) => w.write(buf),
            Writer::NonBlocking(w, _) => w.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Writer::Sync(w) => w.flush(),
            Writer::NonBlocking(w, _) => w.flush(),
        }
    }
}

impl Writer {
    /// 非阻塞模式的带超时 flush；同步模式恒 Ok（数据已在调用线程直接落盘）
    pub fn flush_with_timeout(&self, timeout: Duration) -> io::Result<()> {
        match self {
            Writer::Sync(_) => Ok(()),
            Writer::NonBlocking(w, _) => w.flush_with_timeout(timeout),
        }
    }

    /// DropNew 策略下累计丢弃的消息数；同步模式恒为 0
    pub fn dropped_count(&self) -> u64 {
        match self {
            Writer::Sync(_) => 0,
            Writer::NonBlocking(w, _) => w.dropped_count(),
        }
    }

    /// 显式优雅关闭；同步模式无后台线程，为 no-op
    pub fn shutdown(&mut self) {
        match self {
            Writer::Sync(_) => {}
            Writer::NonBlocking(_, guard) => guard.shutdown(),
        }
    }
}

/// 唯一入口：校验归一化 -> 构造引擎 -> 按模式分发运行时
pub fn open(config: Config) -> Result<Writer, Error> {
    let valid = config.validate()?;
    let mode = valid.mode;
    let engine = Engine::new(valid)?;
    match mode {
        Mode::Sync => Ok(Writer::Sync(SyncWriter::new(engine))),
        Mode::NonBlocking(nb) => {
            let (writer, guard) = AsyncWriter::new(engine, &nb)?;
            Ok(Writer::NonBlocking(writer, guard))
        }
    }
}
