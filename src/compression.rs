use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

pub(crate) fn compress_file(path: &Path) -> io::Result<PathBuf> {
    let gz_path: PathBuf = format!("{}.gz", path.display()).into();
    // 使用作用域块限制input/reader的生命周期，确保在remove_file前释放文件锁
    // 这对Windows兼容性至关重要，Windows不允许删除仍被打开的文件
    {
        let input = std::fs::File::open(path)?;
        let mut reader = BufReader::new(input);

        let output = std::fs::File::create(&gz_path)?;
        let mut writer = flate2::write::GzEncoder::new(output, flate2::Compression::default());

        //压缩数据并完成编码； 失败时清理残留.gz文件，避免后续误判为已压缩
        let result = io::copy(&mut reader, &mut writer).and_then(|_| writer.finish());
        if result.is_err() {
            let _ = std::fs::remove_file(&gz_path);
        }
        result?;
    }

    // 压缩成功后删除原始文件（此时文件已关闭，Windows上也允许删除）
    std::fs::remove_file(path)?;

    Ok(gz_path)
}
