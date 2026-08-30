//! 库错误类型（设计文档 §4.6）

use std::io;

/// 库错误类型
#[derive(Debug)]
pub enum Error {
    /// 配置非法（如 sep 含路径分隔符、date_format 产生含字母时间串）
    Config(String),
    /// 命名模板非法（未知占位符、{dir} 不在开头、缺 {name}/{date}、
    /// {date} 与 {counter} 直接相邻、占位符未闭合）
    Template(String),
    /// 文件系统 I/O 错误
    Io(io::Error),
    /// 压缩失败（不影响数据完整性：原文件保留，由后续轮转择机补压重试）
    Compress(io::Error),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Config(s) => write!(f, "配置错误: {s}"),
            Error::Template(s) => write!(f, "模板错误: {s}"),
            Error::Io(e) => write!(f, "I/O 错误: {e}"),
            Error::Compress(e) => write!(f, "压缩错误: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) | Error::Compress(e) => Some(e),
            _ => None,
        }
    }
}
