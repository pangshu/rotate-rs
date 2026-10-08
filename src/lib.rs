//! # rotate-rs
//!
//! 独立的、可嵌入的第三方 Rust 日志文件轮转库。只专注轮转逻辑：
//! 何时切割、切割后文件叫什么、旧文件保留多少、是否压缩。
//!
//! ## 快速上手
//!
//! ```no_run
//! use std::io::Write;
//! use rotate_rs::{open, Config};
//!
//! # fn main() -> Result<(), rotate_rs::Error> {
//! // 零配置：写入 logs/app.log，Hybrid（100MB / 24h）轮转，保留 30 个备份
//! let mut writer = open(Config::default())?;
//! writer.write_all(b"hello rotate-rs\n")?;
//! writer.flush()?;
//! # Ok(())
//! # }
//! ```
//!
//! ## 使用约束
//!
//! 1. **单进程独占日志目录**：不支持多进程（或同进程多实例）写同一日志目录。
//!    轮转 rename 后其他进程持有的句柄会继续写入已归档文件；各实例清理逻辑会
//!    互相删除对方保留的文件。多实例请配置独立 `dir` 或独立 `name`。
//! 2. **写入原子单位是单次 `write` 调用**：一条日志应在一次 `write` 中完整写入；
//!    分片写入（如 `writeln!` 展开的多个 format 片段）在多线程下可能交错，
//!    且片段间可能触发轮转导致一条日志被拆到两个文件。
//! 3. **同步模式无空闲轮转**：时间轮转依赖写入触发检查；空闲准点轮转需使用
//!    `Mode::NonBlocking`（时间轮转由 worker 定时器驱动，与写入路径完全解耦）。
//! 4. **非阻塞模式 `WorkerGuard` 收纳在 `Writer` 内**：writer 存活即 worker 存活；
//!    多线程共享时从 `Writer::NonBlocking(w, _)` 取出 `w` 克隆。优雅关闭
//!    （`shutdown()` / Drop）时 worker 排空 channel 并等待在途发送者完成：
//!    `write` 返回 `Ok` 的数据必然落盘；关闭启动后的 `write` 显式报错。
//! 5. **`error_handler` 回调禁止日志递归**：回调签名为 `Fn(&str, &Error)`，
//!    第一个参数是阶段上下文（"write" / "rotate" / "compress"）。回调内
//!    不可调用会路由回本 writer 的 `tracing::error!` / `log::error!`
//!    （Block 下 worker 死锁、DropNew 下错误日志挤占 channel）；应走
//!    metrics / 告警旁路。回调 panic 已被库捕获（catch_unwind 防护，
//!    不会终止 worker）。
//! 6. **活跃文件被外部删除会自动重建**：运行期活跃文件（如 `logs/app.log`）被外部
//!    删除时，下次轮转发现源文件缺失会直接重建活跃文件并继续写入（无物可归档，
//!    故不产出归档、不上报错误），而非陷入 rename 失败的重试。删除前已写入但
//!    未落盘的数据无法找回。

mod channel;
mod compress;
mod config;
mod engine;
mod error;
mod naming;
mod rotation;
mod sync;
mod writer;

pub use channel::{AsyncWriter, WorkerGuard};
pub use config::{Config, ErrorHandler, Mode, NonBlockingConfig, OverflowStrategy};
pub use error::Error;
pub use rotation::Rotation;
pub use sync::SyncWriter;
pub use writer::{Writer, open};
