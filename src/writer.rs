use core::option::Option::{None, Some};
use core::result::Result::{Err, Ok};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::compression::{self, compress_file};
use crate::constants::normalize_suffix;
use crate::rotation::Rotation;
use crate::suffix::{self, SuffixScheme, generate_date_string, scan_rotated_files};

pub struct RotatingWriter {
    path: PathBuf,
    file_prefix: String,
    file_suffix: String,
    current_date: String,
    file: Option<File>,
    rotation: Rotation,
    suffix_scheme: SuffixScheme,
    compression: bool,
    current_size: u64,
    last_rotation: Instant,
    // 记录上次轮转失败的错误信息，用于在下次这与入时重试
    last_rotation_error: Option<String>,
}

impl RotatingWriter {
    // 创建写入
    pub fn new(
        path: impl AsRef<Path>,
        file_prefix: String,
        file_suffix: String,
        rotation: Rotation,
        suffix_scheme: SuffixScheme,
        compression: bool,
    ) -> io::Result<Self> {
        let file_suffix = normalize_suffix(&file_suffix);
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let current_size = file.metadata().map(|m| m.len()).unwrap_or(0);

        // 从路径中提取当前日期
        let current_date = extract_date_from_path(&path, &file_prefix, &file_suffix)
            .unwrap_or_else(|| generate_date_string(suffix_scheme.date_format()));

        Ok(Self {
            path,
            file_prefix,
            file_suffix,
            current_date,
            file: Some(file),
            rotation,
            suffix_scheme,
            compression,
            current_size,
            last_rotation: Instant::now(),
            last_rotation_error: None,
        })
    }

    // 构建文件路径： `{dir}/{prefix}.{date}{suffix}`
    fn build_path(&self, date: &str) -> PathBuf {
        let parent = self.path.parent().unwrap_or(Path::new(""));
        parent.join(format!("{}.{}{}", self.file_prefix, date, self.file_suffix))
    }

    // 构建轮转文件路径： `{dir}/{prefix}.{rotation_suffix}{suffix}{.gz}`
    fn rotated_path(&self, rotation_suffix: &str, compressed: bool) -> String {
        let parent = self.path.parent().unwrap_or(Path::new(""));
        let base = format!("{}.{}", self.file_prefix, rotation_suffix);
        if compressed {
            parent
                .join(format!("{}{}.gz", base, self.file_suffix))
                .display()
                .to_string()
        } else {
            parent
                .join(format!("{}{}", base, self.file_suffix))
                .display()
                .to_string()
        }
    }

    // 执行轮转
    // 统一方案： 当前文件`{prefix}{suffix}` -> 轮转后 `{prefix}.{date}{suffix}`
    // 日期冲突时追加 counter: `{prefix}.{date}.{counter}{suffix}`
    pub(crate) fn rotate(&mut self) -> io::Result<()> {
        // 先flush + sync 旧文件，但不take(保留句柄，失败时还能继续写)
        if let Some(ref mut file) = self.file {
            let _ = file.flush();
            let _ = file.sync_all();
        }

        let new_date = generate_date_string(self.suffix_scheme.date_format());
        let default_target = self.build_path(&new_date);

        //当前文件重命名后的目标路径
        let rotated_target = if Path::new(&default_target).exists() {
            let mut counter = 1;
            loop {
                let candidate = self.build_path(&format!("{}.{}", new_date, counter));
                if !Path::new(&candidate).exists() {
                    break candidate;
                }
                counter += 1;
            }
        } else {
            default_target
        };

        // 1、重命名当前文件（此时文件句柄仍有效）
        std::fs::rename(&self.path, &rotated_target)?;

        // 2、rename 成功后，才丢弃旧文件句柄
        self.file = None;

        // 3、立即压缩rename的文件(如果启用压缩)
        // 这确保风轮转的文件不会被 cleanup_and_compress 的 exclude_date 逻辑跳过
        if self.compression {
            let _ = compress_file(Path::new(&rotated_target));
        }

        self.current_date = new_date;

        // 4、清理和压缩其他旧文件（排除当前日期）
        self.cleanup_and_compress(self.suffix_scheme.max_backups(), &self.current_date);

        // 5、打开新文件
        match OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
        {
            Ok(new_file) => {
                self.file = Some(new_file);
                self.current_size = 0;
                self.last_rotation = Instant::now();
                Ok(())
            }
            Err(e) => {
                // open 新文件失败，尝试恢复，将已重命名的文件移回原位并重新打开
                eprint!("rotate-rs: failed to open new file after rotation: {}", e);
                if let Err(rename_back_err) = std::fs::rename(&rotated_target, &self.path) {
                    eprint!(
                        "rotate-rs: failed to restore original file: {}",
                        rename_back_err
                    );
                    return Err(e);
                }

                // 恢复成功，重新打开旧文件以保持Writer可用
                match OpenOptions::new().append(true).open(&self.path) {
                    Ok(recovered_file) => {
                        self.current_size = recovered_file.metadata().map(|m| m.len()).unwrap_or(0);
                        self.file = Some(recovered_file);
                        self.last_rotation = Instant::now();
                        eprint!(
                            "rotate-rs: recovered original file after totation failure, Writer remains usable"
                        );
                    }
                    Err(reopen_err) => {
                        eprint!(
                            "rotate-rs: failed to reopen original file after recovery: {}",
                            reopen_err
                        );
                        // 最后手段：尝试创建新文件，确保Writer不会永久不可用
                        // 注意：这会丢失旧文件中未flush的数据，但比完全瘫痪好
                        match OpenOptions::new()
                            .create(true)
                            .write(true)
                            .truncate(true)
                            .open(&self.path)
                        {
                            Ok(fallback_file) => {
                                self.file = Some(fallback_file);
                                self.current_size = 0;
                                self.last_rotation = Instant::now();
                                eprint!(
                                    "rotate-rs: created fallback file after recovery failure,Writer remains usable (some data may be lost)"
                                );
                            }
                            Err(fallback_err) => {
                                // 连fallback都失败，真的无能为力了
                                eprint!(
                                    "rotate-rs: failed to create fallback file: {}, Writer is now unusable",
                                    fallback_err
                                );
                            }
                        }
                    }
                }
                Err(e)
            }
        }
    }

    // 清理旧文件并压缩未压缩的文件
    // 合并清理和压缩操作，避免重复扫描目录
    fn cleanup_and_compress(&self, max_files: Option<usize>, exclude_date: &str) {
        let files = scan_rotated_files(
            &self.path,
            &self.file_prefix,
            &self.file_suffix,
            exclude_date,
        );

        // 如果需要限制文件数量，先清理超出部分
        if let Some(max) = max_files {
            for (suffix, _is_compressed) in files.iter().skip(max.saturating_sub(1)) {
                let path = self.rotated_path(suffix, false);
                let _ = std::fs::remove_file(&path);
                let path_gz = self.rotated_path(suffix, true);
                let _ = std::fs::remove_file(&path_gz);
            }
        }

        // 压缩未压缩的文件（只处理保留范围内的文件）
        if self.compression {
            let files_to_compress = if let Some(max) = max_files {
                &files[..std::cmp::min(max.saturating_sub(1), files.len())]
            } else {
                &files[..]
            };

            for (suffix, is_compressed) in files_to_compress.iter() {
                if !is_compressed {
                    let path = self.rotated_path(suffix, false);
                    let _ = compress_file(Path::new(&path));
                }
            }
        }
    }

    // 返回时间轮转间隔（如果有）
    pub(crate) fn rotation_interval(&self) -> Option<std::time::Duration> {
        self.rotation.time_interval()
    }
}

impl Write for RotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // 检查是否需要轮转（同步Writer使用write-time checking）
        let elapsed = self.last_rotation.elapsed();
        let should_rotate = self.rotation.verify_rotate(self.current_size, elapsed);

        // 如果上次轮转失败，或者本次需要轮转，则尝试轮转
        if should_rotate || self.last_rotation_error.is_some() {
            match self.rotate() {
                Ok(()) => {
                    // 轮转成功，清除错误信息
                    self.last_rotation_error = None;
                }
                Err(e) => {
                    // 记录错误，但继续写入（避免数据丢失）
                    let error_msg = format!("{}", e);
                    eprintln!("rotate-rs: rotation failed: {}", error_msg);
                    self.last_rotation_error = Some(error_msg);
                }
            }
        }

        // 写入数据
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("no file open"))?;
        let written = file.write(buf)?;
        self.current_size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(ref mut file) = self.file {
            file.flush()?;
        }
        Ok(())
    }
}

impl Drop for RotatingWriter {
    fn drop(&mut self) {
        if let Some(ref mut file) = self.file {
            let _ = file.flush();
            let _ = file.sync_all();
        }
    }
}

// 从路径中提取日期部分
// 路径格式`{dir}/{prefix}.{date}{suffix}`
fn extract_date_from_path(path: &Path, prefix: &str, suffix: &str) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let prefix_dot = format!("{}.", prefix);
    if !name.starts_with(&prefix_dot) {
        return None;
    }
    let rest = &name[prefix_dot.len()..];
    if rest.ends_with(suffix) && rest.len() > suffix.len() {
        Some(rest[..rest.len() - suffix.len()].to_string())
    } else {
        None
    }
}
