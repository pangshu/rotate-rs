//! 同步运行时：write-time checking（设计文档 §7）

use std::io::{self, Write};

use crate::engine::Engine;

/// 同步写入器：write() 内联轮转检查
///
/// 注意（设计文档 §7.4）：同步模式的时间轮转依赖写入驱动，长期无写入不会触发轮转；
/// 需要空闲场景准时轮转请使用非阻塞模式。
pub struct SyncWriter {
    pub(crate) engine: Engine,
}

impl SyncWriter {
    pub(crate) fn new(engine: Engine) -> Self {
        SyncWriter { engine }
    }
}

impl Write for SyncWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // 1. 上次轮转失败则无条件重试（不阻断写入）
        if self.engine.has_pending_rotation()
            && let Err(e) = self.engine.rotate()
        {
            self.engine.reporter().report("rotate-retry", &e);
        }
        // 2. write-time checking：满足轮转条件即触发（失败不阻断写入）
        if self.engine.should_rotate_now()
            && let Err(e) = self.engine.rotate()
        {
            self.engine.reporter().report("rotate", &e);
        }
        // 3. 写入
        self.engine.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.engine.flush()
    }
}
