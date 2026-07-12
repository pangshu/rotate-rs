// 默认容量
pub(crate) const DEFAULT_CHANNEL_CAPACITY: usize = 128 * 1024;

// 默认文件前缀
pub(crate) const DEFAULT_FILE_PREFIX: &str = "app";

// 默认文件后缀
pub(crate) const DEFAULT_FILE_SUFFIX: &str = "log";

// 默认日期格式: yyyy-MM-dd_HH-mm-ss
pub const DEFAULT_DATE_FORMAT: &str = "%Y-%m-%d_%H-%M-%S";

// 规范文件后缀
pub(crate) fn normalize_suffix(suffix: &str) -> String {
    let trimmed = suffix.trim_start_matches(',');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(".{}", trimmed)
    }
}
