//! 非阻塞运行时：channel、背压双阈值、worker 定时轮转、优雅关闭（设计文档 §8）

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::{NonBlockingConfig, OverflowStrategy};
use crate::engine::Engine;
use crate::Error;

/// channel 通信消息
pub(crate) enum Message {
    /// 日志数据（一次 write 调用 = 一条消息）
    Data(Vec<u8>),
    /// 同步 flush 请求（带应答通道）
    Flush(SyncSender<io::Result<()>>),
    /// 关闭信号（worker 收到后排空 channel 再退出）
    Close,
}

/// 业务侧共享状态（Arc 共享，多线程克隆 AsyncWriter 并发写）
pub(crate) struct WriterShared {
    pub(crate) tx: SyncSender<Message>,
    /// 待写字节配额（CAS 占用 / worker 消费时释放）
    pub(crate) pending_bytes: AtomicUsize,
    /// worker 是否已退出（Block 等待循环感知，防永久阻塞）
    pub(crate) closed: AtomicBool,
    /// 在途发送者计数（R4-1 关闭排空协议）
    pub(crate) inflight_senders: AtomicUsize,
    pub(crate) dropped: AtomicU64,
    pub(crate) overflow: OverflowStrategy,
    pub(crate) max_pending_bytes: usize,
}

/// 非阻塞写入器：可克隆，多线程并发写
pub struct AsyncWriter {
    pub(crate) shared: Arc<WriterShared>,
}

impl Clone for AsyncWriter {
    fn clone(&self) -> Self {
        AsyncWriter {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl AsyncWriter {
    pub(crate) fn new(
        engine: Engine,
        config: &NonBlockingConfig,
    ) -> Result<(Self, WorkerGuard), Error> {
        let (tx, rx) = sync_channel(config.channel_capacity);
        let shared = Arc::new(WriterShared {
            tx: tx.clone(),
            pending_bytes: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            inflight_senders: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            overflow: config.overflow,
            max_pending_bytes: config.max_pending_bytes,
        });
        let worker_shared = Arc::clone(&shared);
        // spawn 失败时 engine 随闭包丢弃（Drop 触发 flush + join 压缩线程）
        let handle = thread::Builder::new()
            .name("rotate-rs-writer".to_string())
            .spawn(move || worker_loop(engine, rx, worker_shared))
            .map_err(Error::Io)?;
        Ok((
            AsyncWriter { shared },
            WorkerGuard {
                shutdown_started: AtomicBool::new(false),
                tx,
                handle: Some(handle),
            },
        ))
    }

    pub(crate) fn dropped_count(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// 带超时的同步 flush：超时返回 TimedOut，数据不丢失
    /// （Flush 消息与之前的 Data 仍留在 channel 由 worker 继续处理）。
    pub fn flush_with_timeout(&self, timeout: Duration) -> io::Result<()> {
        // R4-2：checked_add 防溢出——`Instant + Duration` 在 timeout 巨大
        // （如 Duration::MAX）时溢出 panic；None 表示近似无限等待
        let deadline = Instant::now().checked_add(timeout);
        let rx_reply = self.enqueue_flush(deadline)?;
        match deadline {
            Some(d) => {
                let remaining = d.saturating_duration_since(Instant::now());
                match rx_reply.recv_timeout(remaining) {
                    Ok(result) => result,
                    Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "flush 超时")),
                }
            }
            // 近似无限：阻塞等待应答（worker 退出时 send 端断开，recv 报错）
            None => rx_reply
                .recv()
                .unwrap_or_else(|_| Err(io::Error::other("worker 已退出"))),
        }
    }

    /// Flush 消息入队：try_send + deadline 轮询（R3-8：send 阶段纳入超时保护）
    /// deadline = None：近似无限轮询直至入队成功或 worker 退出（R4-2）
    fn enqueue_flush(&self, deadline: Option<Instant>) -> io::Result<Receiver<io::Result<()>>> {
        let (tx_reply, rx_reply) = sync_channel(1);
        let mut msg = Message::Flush(tx_reply);
        loop {
            match self.shared.tx.try_send(msg) {
                Ok(()) => return Ok(rx_reply),
                Err(TrySendError::Full(m)) => match deadline {
                    Some(d) if Instant::now() >= d => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "flush 入队超时（channel 满）",
                        ));
                    }
                    _ => {
                        msg = m;
                        thread::sleep(Duration::from_millis(1));
                    }
                },
                Err(TrySendError::Disconnected(_)) => {
                    return Err(io::Error::other("worker 已退出"));
                }
            }
        }
    }
}

/// 字节额度占用守卫（R3-5）：武装状态下 Drop 自动回滚额度，
/// 防 panic/提前返回路径泄漏；投递成功后 disarm 将额度责任转移给 worker
struct QuotaGuard<'a> {
    shared: &'a WriterShared,
    bytes: usize,
    armed: bool,
}

impl<'a> QuotaGuard<'a> {
    fn armed(shared: &'a WriterShared, bytes: usize) -> Self {
        QuotaGuard {
            shared,
            bytes,
            armed: true,
        }
    }

    /// 投递成功：额度责任转移给 worker（消费时释放），本守卫不再回滚
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for QuotaGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            release_bytes(self.shared, self.bytes);
        }
    }
}

/// 在途发送者登记守卫（R4-1）：创建即登记，Drop 注销（含 panic 路径）。
/// 与 worker 收尾排空配合的 SeqCst 协议：发送侧"先登记、后复查 closed"，
/// worker 侧"先置位 closed、后轮询 inflight"——先登记者必被等待到，
/// 看到 closed 者必不投递，因此 write 返回 Ok ⇒ 消息必被 worker 消费
struct InflightGuard<'a> {
    shared: &'a WriterShared,
}

impl<'a> InflightGuard<'a> {
    fn register(shared: &'a WriterShared) -> Self {
        shared.inflight_senders.fetch_add(1, Ordering::SeqCst);
        InflightGuard { shared }
    }
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.shared.inflight_senders.fetch_sub(1, Ordering::SeqCst);
    }
}

impl io::Write for AsyncWriter {
    /// 背压双阈值 + 溢出策略（设计文档 §8.2）
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len();
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(io::Error::other("worker 已退出"));
        }
        // R3-3/R3-5：额度占用统一为记账式——普通消息额度内占用、额度满按策略处理；
        // 超大消息（>= 配额）绕过拒绝但记账（内存上界 = 配额 + 在途超大消息，有界）。
        // 占用成功即武装 QuotaGuard：投递成功前任何提前返回/panic 自动回滚额度。
        let quota = loop {
            let cur = self.shared.pending_bytes.load(Ordering::Relaxed);
            if n >= self.shared.max_pending_bytes || cur + n <= self.shared.max_pending_bytes {
                // 超大消息饱和记账防溢出；普通消息额度内加和
                let target = if n >= self.shared.max_pending_bytes {
                    cur.saturating_add(n)
                } else {
                    cur + n
                };
                match self.shared.pending_bytes.compare_exchange_weak(
                    cur,
                    target,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break QuotaGuard::armed(&self.shared, n),
                    Err(_) => continue,
                }
            }
            // 超限（仅普通消息到达此处）：按溢出策略处理
            match self.shared.overflow {
                OverflowStrategy::DropNew => {
                    self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                    // 尽力而为语义：Ok 但数据未入队
                    return Ok(n);
                }
                OverflowStrategy::Block => {
                    if self.shared.closed.load(Ordering::Acquire) {
                        return Err(io::Error::other("worker 已退出"));
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
        };
        // 投递（DropNew 用 try_send：channel 满即丢弃，write 永不阻塞）
        let msg = Message::Data(buf.to_vec());
        // R4-1：先登记在途、再复查 closed（SeqCst 协议见 InflightGuard 注释）；
        // 复查失败：额度由 quota drop 回滚，登记由 _inflight drop 注销
        let _inflight = InflightGuard::register(&self.shared);
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(io::Error::other("worker 已退出"));
        }
        let sent = match self.shared.overflow {
            OverflowStrategy::DropNew => match self.shared.tx.try_send(msg) {
                Ok(()) => true,
                // 丢弃（quota drop 回滚额度）
                Err(TrySendError::Full(_)) => false,
                Err(TrySendError::Disconnected(_)) => {
                    self.shared.closed.store(true, Ordering::Release);
                    return Err(io::Error::other("worker 已退出"));
                }
            },
            OverflowStrategy::Block => match self.shared.tx.send(msg) {
                Ok(()) => true,
                Err(_) => {
                    self.shared.closed.store(true, Ordering::Release);
                    return Err(io::Error::other("worker 已退出"));
                }
            },
        };
        if sent {
            // 额度责任转移给 worker（消费时释放）
            quota.disarm();
        } else {
            // DropNew channel 满：丢弃（quota drop 回滚额度）
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
        Ok(n)
    }

    /// 阻塞 flush：发送 Flush 消息并等待 worker 应答
    fn flush(&mut self) -> io::Result<()> {
        let (tx_reply, rx_reply) = sync_channel(1);
        self.shared
            .tx
            .send(Message::Flush(tx_reply))
            .map_err(|_| io::Error::other("worker 已退出"))?;
        rx_reply
            .recv()
            .unwrap_or_else(|_| Err(io::Error::other("worker 已退出")))
    }
}

/// 释放字节额度（saturating 防御下溢：超大消息的饱和记账可能超出实际占用）
fn release_bytes(shared: &WriterShared, n: usize) {
    let _ = shared.pending_bytes.fetch_update(
        Ordering::Relaxed,
        Ordering::Relaxed,
        |v| Some(v.saturating_sub(n)),
    );
}

/// 排空单条消息（关闭路径共用）。R5-1：数据写入失败经 reporter 上报，
/// 与主循环 Data 分支同语义——关闭窗口内的写入失败不再静默吞掉
fn drain_message(engine: &mut Engine, shared: &WriterShared, m: Message) {
    match m {
        Message::Data(buf) => {
            release_bytes(shared, buf.len());
            if let Err(e) = engine.write_all(&buf) {
                engine.reporter().report("write", &Error::Io(e));
            }
        }
        Message::Flush(r) => {
            let _ = r.send(engine.flush());
        }
        Message::Close => {}
    }
}

/// worker 关闭守卫（R3-2）：Drop 置位 closed（Release，R3-4 内存序统一），
/// 正常 return 与 panic unwind 均触发——worker 死亡后 Block 等待中的
/// 业务线程必然能感知退出，不再永久自旋
struct ClosedOnDrop(Arc<WriterShared>);

impl Drop for ClosedOnDrop {
    fn drop(&mut self) {
        self.0.closed.store(true, Ordering::Release);
    }
}

/// worker 守卫：drop 时触发优雅关闭（发送 Close -> worker 排空 -> join）
pub struct WorkerGuard {
    /// 防重入（显式 shutdown 与 Drop 竞争）
    shutdown_started: AtomicBool,
    tx: SyncSender<Message>,
    handle: Option<JoinHandle<()>>,
}

impl WorkerGuard {
    /// 显式关闭：与 Drop 等价，可重复调用（原子防重入）
    pub fn shutdown(&mut self) {
        if !self.shutdown_started.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(Message::Close);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// worker 主循环（设计文档 §8.3）：deadline 对齐定时轮转，与写入路径完全解耦。
/// 时间轮转唯一由此处 `recv_timeout` 超时驱动——业务零写入时轮转照常准时触发。
pub(crate) fn worker_loop(mut engine: Engine, rx: Receiver<Message>, shared: Arc<WriterShared>) {
    // R3-2：closed 置位与退出路径解耦（含 panic unwind），置于一切逻辑之前。
    // 声明顺序保证 drop 顺位在 rx/engine 之前：先置 closed，再断开 channel
    let _closed = ClosedOnDrop(Arc::clone(&shared));
    let interval = engine.interval();
    // checked_add 防溢出（R5-2）：Duration::MAX 等极端 interval 会使加法 panic；
    // 溢出时 None = 不挂定时器（时间轮转停用、size 轮转照常），与"MAX ≈ 永不"意图一致
    let mut next_deadline = interval.and_then(|d| Instant::now().checked_add(d));
    loop {
        let msg = match next_deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                rx.recv_timeout(remaining)
            }
            // 纯 Size 策略：无定时器，阻塞等
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match msg {
            Ok(Message::Data(buf)) => {
                // 先释放额度再写盘：让 Block 等待的业务线程尽快恢复
                release_bytes(&shared, buf.len());
                match engine.write_all(&buf) {
                    Ok(()) => engine.reporter().reset(),
                    Err(e) => engine.reporter().report("write", &Error::Io(e)),
                }
                // 上次轮转失败则重试：纯 Size 策略无定时器兑底，必须在此重试
                // （与同步模式语义对齐，不阻断后续写入）
                if engine.has_pending_rotation()
                    && let Err(e) = engine.rotate()
                {
                    engine.reporter().report("rotate-retry", &e);
                }
                // 大小分量判定：size 轮转由写入驱动（worker 线程内判定不违反
                // "时间轮转与写入路径解耦"——时间轮转仍由定时器唯一负责）
                if engine.should_rotate_by_size_now() {
                    match engine.rotate() {
                        Ok(()) => {
                            // size 轮转后同步重置 deadline：保持与 last_rotation 的
                            // 基准一致，避免时间轮转间隔被拉长（两套时钟必须同步）
                            if let Some(iv) = interval {
                                // R5-2：checked_add 溢出时停用时间轮转（同启动处语义）
                                next_deadline = Instant::now().checked_add(iv);
                            }
                        }
                        Err(e) => engine.reporter().report("rotate", &e),
                    }
                }
            }
            Ok(Message::Flush(reply)) => {
                let _ = reply.send(engine.flush());
            }
            Ok(Message::Close) => {
                // 排空剩余消息后退出：保证关闭前数据全部落盘
                while let Ok(m) = rx.try_recv() {
                    drain_message(&mut engine, &shared, m);
                }
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                if engine.should_rotate_now()
                    && let Err(e) = engine.rotate()
                {
                    engine.reporter().report("rotate", &e);
                }
                // 关键：从旧 deadline 推进而非 rotate 完成时刻，消除累计漂移；
                // 若处理耗时超过一个周期，快速补齐避免空转
                if let (Some(deadline), Some(iv)) = (next_deadline.as_mut(), interval) {
                    *deadline += iv;
                    while *deadline <= Instant::now() {
                        *deadline += iv;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = engine.flush();
    // R4-1：closed 必须先于收尾排空显式置位——否则排空释放额度会唤醒 Block
    // 等待中的业务线程，使其在 closed=false 期间成功入队注定滞留的消息
    // （write 返回 Ok 但数据永不落盘，静默丢失）。
    // ClosedOnDrop 仍保留为 panic unwind 路径的兜底（与此处幂等）。
    // SeqCst 与发送侧"登记后复查"配对：先登记的发送者必被下方在途等待等到。
    shared.closed.store(true, Ordering::SeqCst);
    // 收尾排空 + 在途等待（R4-1）：closed 置位前已登记的发送者可能尚未投递
    // 完成（额度占用到 send 之间的微小窗口），等待其完成并排空，直至 channel
    // 空且无在途发送者——闭合"排空完成到 rx 断开"窗口：write 返回 Ok ⇒
    // 数据必被消费落盘。发送者有限且投递必然快速完成，循环有界。
    loop {
        let inflight = shared.inflight_senders.load(Ordering::SeqCst);
        let mut received = false;
        while let Ok(m) = rx.try_recv() {
            received = true;
            drain_message(&mut engine, &shared, m);
        }
        // inflight==0 时登记过的发送必然完成，其消息已被上方排空收净
        if inflight == 0 && !received {
            break;
        }
        thread::yield_now();
    }
    let _ = engine.flush();
}
