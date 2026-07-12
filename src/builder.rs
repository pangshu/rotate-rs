use std::io::{self, Seek};
use std::path::PathBuf;
use std::time::Duration;

use crate::constants::{
    DEFAULT_CHANNEL_CAPACITY, DEFAULT_FILE_PREFIX, DEFAULT_FILE_SUFFIX, normalize_suffix,
};
use crate::non_blocking::{NonBlockingWriter, Worker};
use crate::rotation::Rotation;
use crate::suffix::{self, SuffixScheme};
use crate::writer::RotatingWriter;

/// 配置项
pub struct RotateBuilder { 
    /// 轮转策略
    rotation: Rotation,
    /// 文件路径
    directory: PathBuf,
    /// 文件前缀
    file_prefix: String,
    /// 文件后缀
    file_suffix: String,
    /// 轮转文件名后缀
    suffix: SuffixScheme,
    /// 是否启用压缩
    compression: bool,
    /// 通道容量
    channel_capacity: usize,
    /// 最大备份文件数
    /// 时间格式
    /// 压缩级别
    /// 队列容量
}

impl RotateBuilder {
    pub fn new(
        directory: impl AsRef<std::path::Path>,
        file_prefix: impl AsRef<str>,
        file_suffix: impl AsRef<str>,
    ) -> Self {
        RotateBuilder {
            rotation: Rotation::default(),
            directory: directory.as_ref().to_path_buf(),
            file_prefix: file_prefix.as_ref().to_string(),
            file_suffix: normalize_suffix(file_suffix.as_ref()),
            suffix: SuffixScheme::default(),
            compression: false,
            channel_capacity: crate::constants::DEFAULT_CHANNEL_CAPACITY,
        }
    }

    pub fn with_directory(mut self, directory: impl AsRef<std::path::Path>) -> Self {
        self.directory = directory.as_ref().to_path_buf();
        self
    }

    pub fn with_prefix(mut self, prefix: impl AsRef<str>) -> Self {
        self.file_prefix = prefix.as_ref().to_string();
        self
    }

    pub fn with_suffix(mut self, suffix: impl AsRef<str>) -> Self {
        self.file_suffix = normalize_suffix(suffix.as_ref());
        self
    }

    fn path(&self) -> PathBuf {
        self.directory
            .join(format("{}{}", self.file_prefix, self.file_suffix))
    }

    pub fn with_mode(mut self, mode: &str, size: usize, time: Duration) -> Self {
        match mode {
            "time" => self.rotation = Rotation::time(time),
            "size" => self.rotation = Rotation::size(size),
            _ => self.rotation = Rotation::hybrid(size, time),
        }
        self
    }

    pub fn with_time(mut self, time:Duration) -> Self {
        self.rotation = Rotation::time(time);
        self
    }

    pub fn with_size(mut self, size:usize) -> Self {
        self.rotation = Rotation::size(size);
        self
    }

    pub fn with_max_backups(mut self, max_backups: Option<usize>, format: &str) -> Self {
        self.suffix = SuffixScheme::with_format(max_backups, format);
        self
    }

    pub fn with_compress(mut self, enable: bool) -> Self {
        self.compression = enable;
        self
    }
    
    // 设置非阻塞写入channel容量, 仅对`build_non_blocking()`有效，默认：128*1024
    pub fn channel_capacity(mut self, capacity: usize) -> Self {
        self.channel_capacity = capacity;
        self
    }

    // 对空值/零值回退到默认值
    fn normalize(self) -> Self{
        RotateBuilder {
            file_prefix: if self.file_prefix.is_empty(){
                DEFAULT_FILE_PREFIX.to_string()
            } else {
                self.file_prefix
            },
            file_suffix: if self.file_suffix.is_empty() {
                normalize_suffix(DEFAULT_FILE_SUFFIX)
            } else {
                self.file_suffix
            },
            rotation: match self.rotation {
                // Size = 0，意味着每次写入都轮转，回退到默认值
                Rotation::Size(0) => Rotation::default(),
                Rotation::Time(Duration::ZERO) => Rotation::default(),
                Rotation::Hybrid {max_size, interval} => {
                    let max_size = if max_size == 0 {
                        100 * 1024 * 1024
                    } else {
                        max_size
                    };
                    let interval = if interval == Duration::ZERO {
                        Duration::from_secs(86400)
                    } else {
                        interval
                    };
                    Rotation::Hybrid {max_size, interval}
                }
                other => other,
            },
            channel_capacity: if self.channel_capacity == 0 {
                DEFAULT_CHANNEL_CAPACITY
            } else {
                self.channel_capacity
            },
            ..self
        }
    }

    // 构建同步写入器
    pub fn build(self) -> io::Result<RotatingWriter> {
        let builder = self.normalize();
        RotatingWriter::new(
            builder.path(),
            builder.file_prefix,
            builder.file_suffix,
            builder.rotation,
            builder.suffix,
            builder.compression,
        )
    }

    // 构建非阻塞写入器
    // 返回 `(NonBlockingWriter, Worker)`.`Worker`必须保活。
    pub fn build_non_blocking(self) -> io::Result<(NonBlockingWriter, Worker)> {
        let builder = self.normalize();
        let writer = RotatingWriter::new(
            builder.path(),
            builder.file_prefix,
            builder.file_suffix,
            builder.rotation,
            builder.suffix,
            builder.compression,
        )?;
        Ok(NonBlockingWriter::new(writer, builder.channel_capacity))
    }
}