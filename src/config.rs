//! 唯一配置项：Config 定义、默认常量、校验归一化（设计文档 §4.1 / §4.2 / §4.8）

use std::sync::Arc;
use std::time::Duration;

use crate::naming::{KnownValues, Template};
use crate::rotation::Rotation;
use crate::Error;

// ── 默认常量表（设计文档 §4.1） ─────────────────────────────

/// 默认日志目录
pub(crate) const DEFAULT_DIR: &str = "logs";
/// 默认文件基础名
pub(crate) const DEFAULT_NAME: &str = "app";
/// 默认文件后缀
pub(crate) const DEFAULT_SUFFIX: &str = "log";
/// 默认命名分隔符
pub(crate) const DEFAULT_SEP: &str = ".";
/// 默认日期格式
pub(crate) const DEFAULT_DATE_FORMAT: &str = "%Y-%m-%d-%H-%M-%S";
/// 默认命名模板
pub(crate) const DEFAULT_TEMPLATE: &str = "{dir}/{name}{sep}{date}{sep}{counter}{sep}{suffix}";
/// Hybrid 策略默认大小分量（100 MB）
pub(crate) const DEFAULT_MAX_SIZE: usize = 100 * 1024 * 1024;
/// Hybrid 策略默认时间分量（24h）
pub(crate) const DEFAULT_INTERVAL: Duration = Duration::from_secs(86400);
/// 默认保留轮转文件数
pub(crate) const DEFAULT_MAX_BACKUPS: usize = 30;
/// 非阻塞模式默认 channel 条数容量
pub(crate) const DEFAULT_CHANNEL_CAPACITY: usize = 128 * 1024;
/// 非阻塞模式默认待写字节配额（256 MB）
pub(crate) const DEFAULT_MAX_PENDING_BYTES: usize = 256 * 1024 * 1024;
/// 异步压缩队列容量
pub(crate) const COMPRESS_QUEUE_CAPACITY: usize = 64;
/// 轮转命名冲突探测上限（防异常死循环）
pub(crate) const MAX_ROTATION_CANDIDATES: u32 = 100_000;
/// stderr 错误节流间隔（连续错误每 N 次打印一次）
pub(crate) const ERROR_REPORT_INTERVAL: u32 = 100;

/// 内部错误回调：替代默认的 stderr 输出。
/// 参数为（阶段上下文, 错误）：context 标识出错阶段
/// （"write" / "rotate" / "rotate-retry" / "compress"），便于定位问题来源
pub type ErrorHandler = Arc<dyn Fn(&str, &Error) + Send + Sync>;

/// 唯一配置项：库的全部行为由它决定（全字段默认值，零配置可用）
#[derive(Clone)]
pub struct Config {
    // ── 文件命名 ──
    /// 日志目录（相对当前工作目录或绝对路径），不存在自动创建
    pub dir: String,
    /// 文件基础名
    pub name: String,
    /// 文件后缀（自动过滤前导 "."，如 ".log" -> "log"；空字符串 = 无后缀）
    pub suffix: String,
    /// 命名分隔符（模板中 {sep} 的值），不能包含 '/' 或 '\'
    pub sep: String,
    /// {date} 的时间格式（chrono 本地时区）
    pub date_format: String,
    /// 轮转文件命名模板，见设计文档 §5
    pub template: String,

    // ── 轮转策略 ──
    pub rotation: Rotation,

    // ── 生命周期 ──
    /// 保留的轮转文件数量上限（含刚轮转的最新一个）；None = 不清理
    pub max_backups: Option<usize>,
    /// 是否 gzip 压缩轮转文件（独立线程异步完成）
    pub compress: bool,

    // ── 写入模式 ──
    pub mode: Mode,

    // ── 可观测性 ──
    /// 内部错误回调（轮转失败、写入失败等）；None = stderr 节流输出
    pub error_handler: Option<ErrorHandler>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("dir", &self.dir)
            .field("name", &self.name)
            .field("suffix", &self.suffix)
            .field("sep", &self.sep)
            .field("date_format", &self.date_format)
            .field("template", &self.template)
            .field("rotation", &self.rotation)
            .field("max_backups", &self.max_backups)
            .field("compress", &self.compress)
            .field("mode", &self.mode)
            .field(
                "error_handler",
                &self.error_handler.as_ref().map(|_| "<handler>"),
            )
            .finish()
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            dir: DEFAULT_DIR.to_string(),
            name: DEFAULT_NAME.to_string(),
            suffix: DEFAULT_SUFFIX.to_string(),
            sep: DEFAULT_SEP.to_string(),
            date_format: DEFAULT_DATE_FORMAT.to_string(),
            template: DEFAULT_TEMPLATE.to_string(),
            rotation: Rotation::default(),
            max_backups: Some(DEFAULT_MAX_BACKUPS),
            compress: false,
            mode: Mode::default(),
            error_handler: None,
        }
    }
}

/// 写入模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// 同步写入：impl Write，轮转检查内联在 write() 中
    #[default]
    Sync,
    /// 非阻塞写入：channel + 后台 worker，时间轮转由 worker 定时器驱动
    NonBlocking(NonBlockingConfig),
}

/// 非阻塞模式配置
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NonBlockingConfig {
    /// channel 条数容量
    pub channel_capacity: usize,
    /// 待写字节配额（channel 内存上界，与条数容量双阈值并行生效）
    pub max_pending_bytes: usize,
    /// 溢出策略
    pub overflow: OverflowStrategy,
}

impl Default for NonBlockingConfig {
    fn default() -> Self {
        NonBlockingConfig {
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            max_pending_bytes: DEFAULT_MAX_PENDING_BYTES,
            overflow: OverflowStrategy::default(),
        }
    }
}

/// 溢出策略（额度满 / channel 满时的行为）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverflowStrategy {
    /// 额度满时业务线程短暂等待（不丢数据，业务被背压）
    #[default]
    Block,
    /// 额度满即丢弃新消息（write 永不阻塞，dropped_count() 可观测）
    DropNew,
}

/// 校验归一化产物：模板已编译、零值已回退（内部类型）
#[derive(Clone)]
pub(crate) struct ValidatedConfig {
    pub known: KnownValues,
    pub template: Template,
    pub rotation: Rotation,
    pub max_backups: Option<usize>,
    pub compress: bool,
    pub mode: Mode,
    pub error_handler: Option<ErrorHandler>,
}

impl Config {
    /// 校验与归一化（设计文档 §4.8）：
    /// 空值回退默认 -> sep 约束 -> suffix 归一化 -> 轮转/非阻塞零值回退 -> 模板编译
    pub(crate) fn validate(self) -> Result<ValidatedConfig, Error> {
        // 1. 空值回退（安全默认）
        let dir = if self.dir.is_empty() {
            DEFAULT_DIR.to_string()
        } else {
            self.dir
        };
        let name = if self.name.is_empty() {
            DEFAULT_NAME.to_string()
        } else {
            self.name
        };
        let sep = if self.sep.is_empty() {
            DEFAULT_SEP.to_string()
        } else {
            self.sep
        };
        let date_format = if self.date_format.is_empty() {
            DEFAULT_DATE_FORMAT.to_string()
        } else {
            self.date_format
        };
        let template_str = if self.template.is_empty() {
            DEFAULT_TEMPLATE.to_string()
        } else {
            self.template
        };

        // 2. 路径分隔符约束：sep/name/suffix 均为文件名组件，含分隔符会
        //    导致子目录静默创建、轮转目标名分裂（R3-6）
        for (field, value) in [("sep", &sep), ("name", &name), ("suffix", &self.suffix)] {
            if value.contains('/') || value.contains('\\') {
                return Err(Error::Config(format!("{field} 不能包含路径分隔符")));
            }
        }

        // 3. suffix 归一化（空 = 显式无后缀，是合法语义，不回退）
        let suffix = self.suffix.trim_start_matches('.').to_string();

        // 3.5 date_format 兼容性预检：渲染样例时间，若含 ASCII 字母则拒绝。
        // （parse 后置校验拒绝含字母的 date 段，若不拦截，清理/补压会静默失效）
        let sample = chrono::Local::now().format(&date_format).to_string();
        if sample.bytes().any(|b| b.is_ascii_alphabetic()) {
            return Err(Error::Config(format!(
                "date_format {:?} 会产生含字母的时间串（如 {sample:?}），\
                 与文件名解析冲突（轮转文件无法被识别，max_backups/compress 将失效）",
                date_format
            )));
        }
        // R3-6：渲染结果会成为文件路径组件，含路径分隔符时轮转目标名分裂为子路径
        if sample.contains('/') || sample.contains('\\') {
            return Err(Error::Config(format!(
                "date_format {:?} 会产生含路径分隔符的时间串（如 {sample:?}），轮转文件路径非法",
                date_format
            )));
        }

        // 4. 轮转策略零值回退
        let rotation = match self.rotation {
            Rotation::Size(0) | Rotation::Time(Duration::ZERO) => Rotation::default(),
            Rotation::Hybrid { max_size, interval } => Rotation::Hybrid {
                max_size: if max_size == 0 {
                    DEFAULT_MAX_SIZE
                } else {
                    max_size
                },
                interval: if interval.is_zero() {
                    DEFAULT_INTERVAL
                } else {
                    interval
                },
            },
            other => other,
        };

        // 4.5 max_backups=Some(0) 归一化（R3-10）：0 与 1 行为等价
        // （刚轮转文件必保留），统一为 Some(1) 消除歧义
        let max_backups = match self.max_backups {
            Some(0) => Some(1),
            other => other,
        };

        // 5. 非阻塞配置零值回退
        let mode = match self.mode {
            Mode::NonBlocking(nb) => Mode::NonBlocking(NonBlockingConfig {
                channel_capacity: if nb.channel_capacity == 0 {
                    DEFAULT_CHANNEL_CAPACITY
                } else {
                    nb.channel_capacity
                },
                max_pending_bytes: if nb.max_pending_bytes == 0 {
                    DEFAULT_MAX_PENDING_BYTES
                } else {
                    nb.max_pending_bytes
                },
                overflow: nb.overflow,
            }),
            Mode::Sync => Mode::Sync,
        };

        // 6. 模板编译（含 5 条校验规则）
        let template = Template::compile(&template_str, &date_format)?;
        let known = KnownValues {
            dir,
            name,
            sep,
            suffix,
        };

        Ok(ValidatedConfig {
            known,
            template,
            rotation,
            max_backups,
            compress: self.compress,
            mode,
            error_handler: self.error_handler,
        })
    }
}
