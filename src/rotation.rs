//! 轮转策略：触发条件判定（设计文档 §4.3 / §6.3）

use std::time::Duration;

use crate::config::{DEFAULT_INTERVAL, DEFAULT_MAX_SIZE};

/// 轮转策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// 按大小轮转（字节）
    Size(usize),
    /// 按固定时间间隔轮转（从活跃文件创建/上次轮转起算，不对齐时钟边界）
    Time(Duration),
    /// 任一条件满足即轮转（默认）
    Hybrid { max_size: usize, interval: Duration },
}

impl Default for Rotation {
    fn default() -> Self {
        Rotation::Hybrid {
            max_size: DEFAULT_MAX_SIZE,
            interval: DEFAULT_INTERVAL,
        }
    }
}

impl Rotation {
    /// 轮转条件判定（唯一入口）
    pub fn should_rotate(&self, current_size: u64, elapsed: Duration) -> bool {
        match self {
            Rotation::Size(max) => current_size >= *max as u64,
            Rotation::Time(interval) => elapsed >= *interval,
            Rotation::Hybrid { max_size, interval } => {
                current_size >= *max_size as u64 || elapsed >= *interval
            }
        }
    }

    /// 仅大小分量判定（不含时间）。
    /// 供非阻塞 worker 在处理 Data 后驱动 size 轮转（大小只能随写入感知）；
    /// Time 策略恒 false——时间轮转仍由定时器唯一负责，与写入路径解耦。
    pub fn should_rotate_by_size(&self, current_size: u64) -> bool {
        match self {
            Rotation::Size(max) => current_size >= *max as u64,
            Rotation::Time(_) => false,
            Rotation::Hybrid { max_size, .. } => current_size >= *max_size as u64,
        }
    }

    /// 时间分量；纯 Size 策略返回 None（worker 不挂定时器）
    pub fn interval(&self) -> Option<Duration> {
        match self {
            Rotation::Size(_) => None,
            Rotation::Time(d) | Rotation::Hybrid { interval: d, .. } => Some(*d),
        }
    }
}
