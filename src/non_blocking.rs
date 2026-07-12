use core::option::Option::{None, Some};
use core::result;
use core::result::Result::{Err, Ok};
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};

use crate::non_blocking;
use crate::writer::{self, RotatingWriter};

enum Message {
    Data(Vec<u8>),
    Flush(std::sync::mpsc::SyncSender<io::Result<()>>),
    Close,
}

pub struct NonBlockingWriter {
    sender: SyncSender<Message>,
}

impl NonBlockingWriter {
    // 创建非阻塞写入器
    pub(crate) fn new(writer: RotatingWriter, capacity: usize) -> (Self, Worker) {
        let (sender, receiver) = sync_channel(capacity);
        let closed = Arc::new(AtomicBool::new(false));

        let handle = thread::Builder::new()
            .name("rotate-rs-writer".to_string())
            .spawn(move || {
                worker_loop(writer, receiver);
            })
            .expect("failed to spawn rotate-rs writer thread");

        let worker = Worker {
            handle: Some(handle),
            sender: Some(sender.clone()),
            closed,
        };

        let non_blocking = NonBlockingWriter { sender };

        (non_blocking, worker)
    }
}

impl Clone for NonBlockingWriter {
    fn clone(&self) -> Self {
        NonBlockingWriter {
            sender: self.sender.clone(),
        }
    }
}

impl Write for NonBlockingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sender
            .send(Message::Data(buf.to_vec()))
            .map_err(|_| io::Error::other("rotate-rs: channel closed"))?;
        Ok(buf.len())
    }

    // 刷新缓冲区
    fn flush(&mut self) -> io::Result<()> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.sender
            .send(Message::Flush(tx))
            .map_err(|_| io::Error::other("rotate-rs: channel closed"))?;

        //等待worker完成刷新并返回结果
        rx.recv()
            .map_err(|_| io::Error::other("rotate-rs: worker thread closed"))?
    }
}

// Worker守卫
pub struct Worker {
    handle: Option<JoinHandle<()>>,
    sender: Option<SyncSender<Message>>,
    closed: Arc<AtomicBool>,
}

impl Worker {
    // 显式关闭，等待所有待写数据写入完成
    pub fn shutdown(&mut self) {
        // 使用原子标志避免重复关闭
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }

        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Message::Close);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// 后台写入线程主循环
fn worker_loop(mut writer: RotatingWriter, receiver: Receiver<Message>) {
    // 记录连续写入失败次数，用于避免日志风暴
    let mut consecutive_errors = 0u64;

    // 获取轮转间隔（如果是Time 或 Hybrid 模式）
    let rotation_interval = writer.rotation_interval();

    loop {
        // 根据是否有时间轮转配置选择接收方式
        let msg = if let Some(interval) = rotation_interval {
            // 带超时的接收： 超时即触发轮转
            match receiver.recv_timeout(interval) {
                Ok(msg) => Some(msg),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // 超时触发轮转
                    if let Err(e) = writer.rotate() {
                        eprintln!("rotate-rs: timed rotation failed:L {}", e);
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
            }
        } else {
            // 纯 size 模式，阻塞等待
            receiver.recv().ok()
            //Ok(receiver.recv())
        };

        match msg {
            Some(Message::Data(data)) => {
                if let Err(e) = writer.write_all(&data) {
                    consecutive_errors += 1;
                    // 只在首次错误或每100次错误时输出，避免日志风暴
                    if consecutive_errors == 1 || consecutive_errors.is_multiple_of(100) {
                        eprintln!(
                            "rotate-rs: worker write failed (consecutive errors: {}); {}",
                            consecutive_errors, e
                        );
                    }
                } else {
                    // 写入成功， 重置计数器
                    consecutive_errors = 0;
                }
            }
            Some(Message::Flush(response_tx)) => {
                // 执行刷新并将结束结果返回给调用者
                let result = writer.flush();
                let is_ok = result.is_ok();
                let _ = response_tx.send(result);
                // flush 成功后也重置错误计数
                if is_ok {
                    consecutive_errors = 0;
                }
            }
            Some(Message::Close) | None => break,
        }
    }
    // 退出前最后刷新一次
    let _ = writer.flush();
}
