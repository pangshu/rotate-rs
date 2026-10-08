//! 轮转引擎：文件状态机、原子轮转流程、失败恢复、清理与压缩调度（设计文档 §6 / §12.1）

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::compress::{compress_file, compress_worker, gz_path};
use crate::config::{
    COMPRESS_QUEUE_CAPACITY, ERROR_REPORT_INTERVAL, ErrorHandler, MAX_ROTATION_CANDIDATES,
    ValidatedConfig,
};
use crate::naming::{KnownValues, ParsedName, RotatedFile, Template, scan_dir};
use crate::rotation::Rotation;
use crate::Error;

/// 错误上报器：用户回调优先，否则 stderr 节流输出（设计文档 §12.1）
pub(crate) struct ErrorReporter {
    handler: Option<ErrorHandler>,
    /// 连续错误计数；写入/轮转成功时清零
    consecutive_errors: AtomicU32,
}

impl ErrorReporter {
    pub(crate) fn new(handler: Option<ErrorHandler>) -> Self {
        ErrorReporter {
            handler,
            consecutive_errors: AtomicU32::new(0),
        }
    }

    pub(crate) fn report(&self, context: &str, err: &Error) {
        match &self.handler {
            Some(h) => {
                // R3-2：用户回调 panic 防护——unwind 穿透会终止 worker/打断业务
                // 线程。捕获后丢弃（panic 位置与消息由默认 panic hook 呈现），
                // 库继续运行
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h(context, err)));
            }
            None => {
                // 节流：仅首次及每 ERROR_REPORT_INTERVAL 次连续错误打印
                let n = self.consecutive_errors.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 || n.is_multiple_of(ERROR_REPORT_INTERVAL) {
                    eprintln!("rotate-rs: {context}: {err}");
                }
            }
        }
    }

    /// 成功即清零连续错误计数
    pub(crate) fn reset(&self) {
        self.consecutive_errors.store(0, Ordering::Relaxed);
    }
}

/// 文件状态机：唯一持有并操作活跃文件句柄的组件。
/// 同步模式由业务线程经由 SyncWriter 串行访问；非阻塞模式仅 worker 线程访问——
/// 两种模式下同一时刻都只有一个线程操作 Engine。
pub(crate) struct Engine {
    /// 当前活跃文件路径（固定规则渲染：{dir}/{name}{sep}{suffix}）
    active_path: PathBuf,
    /// 当前文件句柄；None = 不可用（rotate 三级恢复全部失败）
    file: Option<File>,
    /// 当前文件已写字节数（重启时从文件 metadata 恢复，支持续写触发轮转）
    current_size: u64,
    /// 上次轮转时刻（构造时 = now；Time/Hybrid 的时间基准）
    last_rotation: Instant,

    rotation: Rotation,
    template: Template,
    known: KnownValues,
    max_backups: Option<usize>,
    compression: bool,

    /// 刚轮转文件的 rotation_key（scan_dir 排除用）
    last_rotation_key: Option<String>,
    /// 上次轮转失败的错误描述；存在时下次 write 无条件重试轮转
    last_rotation_error: Option<String>,

    /// 异步压缩任务队列与线程（仅启用压缩时存在）
    compress_tx: Option<SyncSender<PathBuf>>,
    compress_handle: Option<JoinHandle<()>>,

    reporter: Arc<ErrorReporter>,
}

impl Engine {
    /// 构造流程（设计文档 §6.2）
    pub(crate) fn new(valid: ValidatedConfig) -> io::Result<Self> {
        let active_path = valid.known.active_path();
        if let Some(parent) = active_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&active_path)?;
        // 重启恢复：从已有文件 metadata 恢复 current_size
        let current_size = file.metadata().map(|m| m.len()).unwrap_or(0);

        let reporter = Arc::new(ErrorReporter::new(valid.error_handler));

        // 启用压缩时启动独立压缩线程；失败退化同步压缩
        let (compress_tx, compress_handle) = if valid.compress {
            let (tx, rx) = sync_channel(COMPRESS_QUEUE_CAPACITY);
            // R4-3：压缩线程共享错误通道，压缩失败（Error::Compress）可观测
            let worker_reporter = Arc::clone(&reporter);
            match thread::Builder::new()
                .name("rotate-rs-compressor".to_string())
                .spawn(move || compress_worker(rx, worker_reporter))
            {
                Ok(handle) => (Some(tx), Some(handle)),
                Err(_) => (None, None),
            }
        } else {
            (None, None)
        };

        let engine = Engine {
            active_path,
            file: Some(file),
            current_size,
            last_rotation: Instant::now(),
            rotation: valid.rotation,
            template: valid.template,
            known: valid.known,
            max_backups: valid.max_backups,
            compression: valid.compress,
            last_rotation_key: None,
            last_rotation_error: None,
            compress_tx,
            compress_handle,
            reporter,
        };
        // 补压历史遗留的未压缩文件
        if engine.compression {
            let files = engine.scan(None);
            for f in &files {
                if !f.parsed.compressed {
                    let path = engine.render_rotated(&f.parsed);
                    engine.enqueue_compress(path);
                }
            }
        }
        Ok(engine)
    }

    pub(crate) fn reporter(&self) -> &Arc<ErrorReporter> {
        &self.reporter
    }

    /// 轮转条件判定（含 elapsed 计算）
    pub(crate) fn should_rotate_now(&self) -> bool {
        let elapsed = self.last_rotation.elapsed();
        self.rotation.should_rotate(self.current_size, elapsed)
    }

    /// 仅大小分量判定（写入路径专用：时间轮转由 worker 定时器唯一负责）
    pub(crate) fn should_rotate_by_size_now(&self) -> bool {
        self.rotation.should_rotate_by_size(self.current_size)
    }

    /// 时间分量（worker 定时器用）
    pub(crate) fn interval(&self) -> Option<std::time::Duration> {
        self.rotation.interval()
    }

    /// 下次时间轮转应触发的绝对时刻 = `last_rotation + interval`。
    ///
    /// 时间轮转的基准是"上次轮转完成时刻"（`finish_rotation` 置位；设计文档 §4.3
    /// "从活跃文件创建/上次轮转起算，不对齐时钟边界"），因此定时器必须锚定它。
    /// 若沿用固定网格推进（旧 deadline 反复 `+= interval`），醒来时刻会早于
    /// `last_rotation + interval`，使 `should_rotate_now()` 因 `elapsed < interval`
    /// 判为 false 而整周期被跳过 —— 实测 interval=5s 时实际周期为 10s。
    ///
    /// 轮转失败时 `last_rotation` 不推进，返回值可能已过期；调用方需按 interval
    /// 兜底推进到未来，避免立即再次超时形成忙等（见 worker_loop 超时分支）。
    pub(crate) fn next_time_deadline(&self) -> Option<Instant> {
        self.rotation
            .interval()
            .and_then(|iv| self.last_rotation.checked_add(iv))
    }

    /// 上次轮转是否失败（同步运行时 write 重试用）
    pub(crate) fn has_pending_rotation(&self) -> bool {
        self.last_rotation_error.is_some()
    }

    /// 写入并维护 current_size
    pub(crate) fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        let f = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("writer 不可用"))?;
        f.write_all(buf)?;
        self.current_size += buf.len() as u64;
        Ok(())
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some(f) => f.flush(),
            None => Err(io::Error::other("writer 不可用")),
        }
    }

    /// 核心轮转流程（失败可恢复，设计文档 §6.4）。
    /// 失败时记录 last_rotation_error，驱动下次 write 重试。
    pub(crate) fn rotate(&mut self) -> Result<(), Error> {
        let result = self.rotate_inner();
        if let Err(e) = &result {
            self.last_rotation_error = Some(e.to_string());
        }
        result
    }

    fn rotate_inner(&mut self) -> Result<(), Error> {
        // ── 活跃文件缺失检测与重建（外部删除恢复）──
        // 活跃文件可能被外部删除（日志目录被清理、运维手动 rm、容器挂载变化等）。
        // 此时旧句柄指向已 unlink 的 inode：rename 的源不存在，轮转将永久失败
        // （每次 write 重试仍 NotFound），后续写入也静默落入已删除 inode（数据丢失）。
        // 文件已消失 = 无物可归档，直接重建活跃文件即可。
        //
        // 必须置于零字节跳过之前：空文件被删（current_size == 0）时，零字节跳过会
        // 先返回而不重建，旧句柄将继续被使用。
        if self.active_file_missing() {
            return self.reopen_active().map_err(Error::Io);
        }

        // ── 零字节跳过 ──
        // 活跃文件为空（应用空闲、无日志写入）时不产生归档：定时轮转在空闲期照常
        // 到点，若不跳过会把 0 字节文件反复归档成空文件（实测空闲 + 小间隔时同一秒
        // 就能产出 .1/.2/.3 多个空文件）。finish_rotation 重置时间基准，使下一个
        // 周期从本次到点重新起算。
        // `file.is_some()` 不可省：本函数兼作"上次轮转失败后的恢复重试"，file == None
        // 表示 writer 不可用，必须继续走下方三级恢复链；若在此短路，finish_rotation
        // 会清掉 last_rotation_error，令重试状态假性消失。
        if self.current_size == 0 && self.file.is_some() {
            self.finish_rotation();
            return Ok(());
        }

        // ── 第一阶段：固化旧文件（失败时状态不变，下次重试） ──
        if let Some(f) = &mut self.file {
            f.flush()?;
            f.sync_all()?;
        }

        // ── 第二阶段：原子占位探测目标名 ──
        // date 取轮转触发时刻（非写入时刻）
        let date = chrono::Local::now()
            .format(self.template.date_format())
            .to_string();
        let has_counter = self.template.has_counter();
        let mut counter: u32 = 1;
        let target = loop {
            if counter > MAX_ROTATION_CANDIDATES {
                return Err(Error::Io(io::Error::other(
                    "轮转命名冲突探测超过 100000 次",
                )));
            }
            // 无 counter 模板：counter 递增不改变渲染结果，同 date 冲突直接失败（规则 5）
            if counter > 1 && !has_counter {
                return Err(Error::Io(io::Error::other(
                    "模板无 {counter}，同 date 轮转目标名冲突",
                )));
            }
            let candidate = self.template.render(&self.known, &date, Some(counter));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(placeholder) => {
                    // R3-1：原 .log 可能已被压缩删除只剩 .gz——名字仍被占用
                    // （后续压缩会覆盖旧归档），视为冲突递增重试
                    if gz_path(&candidate).exists() {
                        drop(placeholder);
                        let _ = fs::remove_file(&candidate); // 清理本次占位
                        counter += 1;
                        continue;
                    }
                    break candidate; // 原子占位成功，目标名唯一（.log 与 .gz 均不存在）
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    counter += 1; // 同 date 冲突，递增重试
                }
                Err(e) => return Err(Error::Io(e)), // 权限等真实错误，直接失败
            }
        };

        // ── 第三阶段：rename（失败时旧文件数据完好，继续写，下次重试） ──
        if let Err(e) = fs::rename(&self.active_path, &target) {
            let _ = fs::remove_file(&target); // 清理占位空文件（勿留 0 字节伪归档）
            // TOCTOU：入口探测通过后、rename 之前源文件被外部删除。与入口探测同一
            // 语义——无物可归档，直接重建活跃文件，而非把 NotFound 当作可重试错误
            // （否则每次 write 重试都失败，形成永久 rotate-retry）。
            if e.kind() == io::ErrorKind::NotFound && self.active_file_missing() {
                return self.reopen_active().map_err(Error::Io);
            }
            return Err(Error::Io(e));
        }

        // ── 第四阶段：状态推进 ──
        self.file = None; // 丢弃旧句柄
        self.last_rotation_key = Some(if has_counter {
            format!("{}{}{}", date, self.known.sep, counter)
        } else {
            date
        });
        if self.compression {
            self.enqueue_compress(target.clone());
        }
        self.cleanup();

        // ── 第五阶段：打开新活跃文件（三级恢复链） ──
        match File::create(&self.active_path) {
            Ok(f) => {
                self.file = Some(f);
                self.current_size = 0;
                self.finish_rotation();
                Ok(())
            }
            Err(first_err) => {
                // 一级恢复：rename 回原位 + append 重开（数据无损）
                if fs::rename(&target, &self.active_path).is_ok()
                    && let Ok(f) = OpenOptions::new().append(true).open(&self.active_path)
                {
                    self.current_size = f.metadata().map(|m| m.len()).unwrap_or(0);
                    self.file = Some(f);
                    self.finish_rotation();
                    return Ok(());
                }
                // 二级恢复：truncate 新建 fallback（旧数据丢失，Writer 可用）
                if let Ok(f) = File::create(&self.active_path) {
                    self.file = Some(f);
                    self.current_size = 0;
                    self.finish_rotation();
                    self.reporter.report(
                        "rotate",
                        &Error::Io(io::Error::other("新文件打开失败，已 fallback 新建")),
                    );
                    return Ok(());
                }
                // 三级：全部失败，Writer 不可用
                self.file = None;
                self.reporter.report(
                    "rotate",
                    &Error::Io(io::Error::other("三级恢复全部失败，Writer 不可用")),
                );
                Err(Error::Io(first_err))
            }
        }
    }

    fn finish_rotation(&mut self) {
        self.last_rotation = Instant::now();
        self.last_rotation_error = None;
    }

    /// 活跃文件是否已从磁盘消失（外部删除探测）。
    /// `try_exists` 的 `Err`（权限等）按"存在"处理：交回正常流程暴露真实错误，
    /// 避免把"无法判断"误判为"已删除"而跳过归档、静默丢弃轮转。
    fn active_file_missing(&self) -> bool {
        !self.active_path.try_exists().unwrap_or(true)
    }

    /// 活跃文件被外部删除后的重建：丢弃失效句柄，按 `Engine::new` 的语义重新打开
    /// （create + append，不 truncate 外部可能已重建的同名文件），并从 metadata
    /// 恢复 `current_size`；最后推进轮转基准并清除失败标志，解除 write 路径重试。
    ///
    /// 不产生任何归档、不触发压缩/清理。失败时 `file` 保持 `None`，由 `rotate()`
    /// 记录 `last_rotation_error` 驱动下次 write 重试（真实错误：权限/磁盘满等）。
    fn reopen_active(&mut self) -> io::Result<()> {
        // 先丢弃旧句柄：它指向已删除的 inode，继续持有只会把后续写入导向无处可寻的数据
        self.file = None;
        if let Some(parent) = self.active_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.active_path)?;
        self.current_size = f.metadata().map(|m| m.len()).unwrap_or(0);
        self.file = Some(f);
        self.finish_rotation();
        Ok(())
    }

    /// 清理与补压（设计文档 §6.5）
    fn cleanup(&mut self) {
        if self.max_backups.is_none() && !self.compression {
            return;
        }
        let exclude = self.last_rotation_key.as_deref();
        let files = self.scan(exclude);
        // 补压历史：启用压缩时投递未压缩的历史文件
        if self.compression {
            for f in &files {
                if !f.parsed.compressed {
                    let path = self.render_rotated(&f.parsed);
                    self.enqueue_compress(path);
                }
            }
        }
        // 清理超量：跳过最新 max-1 个历史文件，删除其余（含 .gz）
        if let Some(max) = self.max_backups {
            for f in files.iter().skip(max.saturating_sub(1)) {
                let plain = self.render_rotated(&f.parsed);
                let gz = gz_path(&plain);
                let _ = fs::remove_file(&plain);
                let _ = fs::remove_file(&gz);
            }
        }
    }

    fn scan(&self, exclude: Option<&str>) -> Vec<RotatedFile> {
        scan_dir(&self.known, &self.template, exclude)
    }

    fn render_rotated(&self, parsed: &ParsedName) -> PathBuf {
        self.template
            .render(&self.known, &parsed.date, parsed.counter)
    }

    /// 投递压缩任务：try_send 投递（R3-7：队列满/线程退出均不阻塞调用方），
    /// 失败取回 path 退化同步压缩（设计文档 §9.2 语义）
    fn enqueue_compress(&self, path: PathBuf) {
        if !self.compression {
            return;
        }
        match &self.compress_tx {
            Some(tx) => {
                // 队列满/线程退出：退化同步压缩（取回 path）
                if let Err(e) = tx.try_send(path) {
                    let path = match e {
                        TrySendError::Full(p) | TrySendError::Disconnected(p) => p,
                    };
                    self.compress_sync(&path);
                }
            }
            None => {
                self.compress_sync(&path);
            }
        }
    }

    /// 同步压缩并上报失败（R4-3：压缩错误纳入错误通道 Error::Compress，
    /// 不再静默吞掉；AlreadyExists 防覆盖拒绝同样可观测）
    fn compress_sync(&self, path: &Path) {
        if let Err(e) = compress_file(path) {
            self.reporter.report("compress", &Error::Compress(e));
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // 进程退出前：flush + sync_all，并等待压缩任务全部完成
        if let Some(f) = &mut self.file {
            let _ = f.flush();
            let _ = f.sync_all();
        }
        self.compress_tx = None; // 关闭队列，通知压缩线程终结
        if let Some(h) = self.compress_handle.take() {
            let _ = h.join();
        }
    }
}
