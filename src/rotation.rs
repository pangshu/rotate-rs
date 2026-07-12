use std::time::Duration;

#[derive(Debug, Clone)]
pub enum Rotation {
    // 按大小轮转（字节）
    Size(usize),
    // 按时间轮转（固定间隔）
    Time(Duration),
    // 混合轮转: 大小与时间同时启用
    Hybrid { max_size: usize, interval: Duration },
}

impl Rotation {
    pub fn size(size: usize) -> Self {
        if size == 0 {
            return Rotation::default();
        }
        Rotation::Size(size)
    }

    pub fn time(interval: Duration) -> Self {
        if interval == Duration::ZERO {
            return Rotation::default();
        }
        Rotation::Time(interval)
    }

    pub fn hybrid(max_size: usize, interval: Duration) -> Self {
        if max_size == 0 || interval == Duration::ZERO {
            return Rotation::default();
        }
        Rotation::Hybrid { max_size, interval }
    }

    // 返回时间轮转间隔（如果有）
    pub(crate) fn time_interval(&self) -> Option<Duration> {
        match self {
            Rotation::Time(interval) => Some(*interval),
            Rotation::Hybrid { interval, .. } => Some(*interval),
            Rotation::Size(_) => None,
        }
    }

    // 验证是否需要轮转
    pub(crate) fn verify_rotate(&self, size: u64, elapsed: Duration) -> bool {
        match self {
            Rotation::Size(max_size) => size >= *max_size as u64,
            Rotation::Time(interval) => elapsed >= *interval,
            Rotation::Hybrid { max_size, interval } => {
                size >= *max_size as u64 || elapsed >= *interval
            }
        }
    }
}

impl Default for Rotation {
    fn default() -> Self {
        Rotation::Hybrid {
            // 默认100MB
            max_size: 100 * 1024 * 1024,
            interval: Duration::from_secs(86400),
        }
    }
}
