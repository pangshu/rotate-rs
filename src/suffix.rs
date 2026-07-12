use core::{convert::Into, option::Option::Some, result::Result::Ok};
use std::path::{Component::Prefix, Path};

#[derive(Debug, Clone)]
pub struct SuffixScheme {
    // 最大保留的轮转文件数量，None 表示不限制
    max_backups: Option<usize>,
    // 日期格式字符串，默认"%Y-%m-%d_%H-%M-%S"
    format: String,
}

impl SuffixScheme {
    // 创建后缀方案
    pub fn new(max_backups: Option<usize>) -> Self {
        SuffixScheme {
            max_backups,
            format: crate::constants::DEFAULT_DATE_FORMAT.to_string(),
        }
    }

    // 自定义日期格式
    pub fn with_format(max_backups: Option<usize>, format: &str) -> Self {
        SuffixScheme {
            max_backups,
            format: format.to_string(),
        }
    }

    // 获取最大备份数量
    pub fn max_backups(&self) -> Option<usize> {
        self.max_backups
    }

    // 获取日期格式字符串
    pub fn date_format(&self) -> &str {
        &self.format
    }

    // 设置日期格式字符串
    pub fn set_date_format(&mut self, format: impl Into<String>) {
        self.format = format.into();
    }
}

impl Default for SuffixScheme {
    fn default() -> Self {
        SuffixScheme::new(Some(30))
    }
}

pub(crate) fn scan_rotated_files(
    base_path: &Path,
    file_prefix: &str,
    file_suffix: &str,
    current_date: &str,
) -> Vec<(String, bool)> {
    let parent = match base_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => return Vec::new(),
    };

    let prefix = format!("{}.", file_prefix);
    let mut files = Vec::new();

    // 遍历父目录下的所有条目
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            // 检查文件名是否以prefix开头，且长度大于prefix(有后缀部分)
            if let Some(name) = entry.file_name().to_str()
                && name.starts_with(&prefix)
                && name.len() > prefix.len()
            {
                // 取 prefix 之后的部分，如 "2026-01-01_10-01-01.log" 或 "2026-01-01_10-01-01.log.gz"
                let rest = &name[prefix.len()..];
                // 判断是否被gzip压缩
                let is_compressed = rest.ends_with(".gz");
                // 去掉.gz后缀得到内部部分
                let inner = if is_compressed {
                    &rest[..rest.len() - 3]
                } else {
                    rest
                };

                // 内部部分需要以file_suffix结尾
                if inner.ends_with(file_suffix) && inner.len() > file_suffix.len() {
                    // 去掉 file_suffix 得到轮转后缀(日期部分)
                    let rotation_suffix = &inner[..inner.len() - file_suffix.len()];
                    // 排除当前文件本身(rotation_suffix 与 current_date 一致)
                    if rotation_suffix == current_date {
                        continue;
                    }
                    // 非空后缀才加入结果列表
                    if !rotation_suffix.is_empty() {
                        files.push((rotation_suffix.to_string(), is_compressed));
                    }
                }
            }
        }
    }

    // 降序排列（最新在前）
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files
}

pub(crate) fn generate_date_string(format: &str) -> String {
    let now = chrono::Local::now();
    now.format(format).to_string()
}
