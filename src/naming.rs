//! 命名模板引擎：编译、渲染、锚点解析、目录扫描排序（设计文档 §5）

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::Error;

/// 已知变量锚点（值来自 Config，解析时作字面量锚点）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Anchor {
    Dir,
    Name,
    Sep,
    Suffix,
}

/// 模板槽位：编译产物（渲染与解析共用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Slot {
    /// 字面量字符（如模板中 {dir} 之后的 "/"）
    Literal(String),
    /// 已知变量锚点
    Known(Anchor),
    /// {date} 运行时变量
    Date,
    /// {counter} 运行时变量
    Counter,
}

/// 渲染 / 解析共用的已知变量值
#[derive(Debug, Clone)]
pub(crate) struct KnownValues {
    pub dir: String,
    pub name: String,
    pub sep: String,
    pub suffix: String,
}

impl KnownValues {
    fn value(&self, anchor: Anchor) -> &str {
        match anchor {
            Anchor::Dir => &self.dir,
            Anchor::Name => &self.name,
            Anchor::Sep => &self.sep,
            Anchor::Suffix => &self.suffix,
        }
    }

    /// 当前写入文件名（固定规则，不参与模板）：{name}{sep}{suffix}；suffix 空则为 {name}
    pub(crate) fn active_file_name(&self) -> String {
        if self.suffix.is_empty() {
            self.name.clone()
        } else {
            format!("{}{}{}", self.name, self.sep, self.suffix)
        }
    }

    /// 当前写入文件完整路径
    pub(crate) fn active_path(&self) -> PathBuf {
        Path::new(&self.dir).join(self.active_file_name())
    }
}

/// 解析产物：一个轮转文件的识别结果
#[derive(Debug, Clone)]
pub(crate) struct ParsedName {
    /// {date}{sep}{counter} 组合（去重与排除刚轮转文件的 key）
    pub rotation_key: String,
    pub date: String,
    pub counter: Option<u32>,
    /// 文件名是否带 .gz
    pub compressed: bool,
}

/// 扫描产物：目录中识别到的一个轮转文件（去重合并后）
#[derive(Debug, Clone)]
pub(crate) struct RotatedFile {
    pub parsed: ParsedName,
    pub mtime: SystemTime,
}

/// 编译后的命名模板
#[derive(Debug, Clone)]
pub(crate) struct Template {
    slots: Vec<Slot>,
    date_format: String,
}

impl Template {
    /// 编译模板：占位符扫描 + 6 条校验规则（设计文档 §5.2 / §5.4）
    pub(crate) fn compile(template: &str, date_format: &str) -> Result<Self, Error> {
        let mut slots: Vec<Slot> = Vec::new();
        let mut rest = template;
        while let Some(i) = rest.find('{') {
            if i > 0 {
                push_literal(&mut slots, &rest[..i]);
            }
            let after = &rest[i + 1..];
            let j = after
                .find('}')
                .ok_or_else(|| Error::Template("占位符未闭合".to_string()))?;
            let key = &after[..j];
            let slot = match key {
                "dir" => Slot::Known(Anchor::Dir),
                "name" => Slot::Known(Anchor::Name),
                "sep" => Slot::Known(Anchor::Sep),
                "suffix" => Slot::Known(Anchor::Suffix),
                "date" => Slot::Date,
                "counter" => Slot::Counter,
                _ => {
                    return Err(Error::Template(format!("未知占位符 {{{key}}}")));
                }
            };
            // 规则 4：{date} 与 {counter} 不得直接相邻（防解析歧义）
            if let Some(last) = slots.last()
                && matches!(
                    (last, &slot),
                    (Slot::Date, Slot::Counter) | (Slot::Counter, Slot::Date)
                )
            {
                return Err(Error::Template(
                    "{date} 与 {counter} 不能直接相邻，中间需 {sep} 或字面量".to_string(),
                ));
            }
            slots.push(slot);
            rest = &after[j + 1..];
        }
        if !rest.is_empty() {
            push_literal(&mut slots, rest);
        }
        // 规则 2：{dir} 必须位于模板开头
        if slots.first() != Some(&Slot::Known(Anchor::Dir)) {
            return Err(Error::Template("模板必须以 {dir} 开头".to_string()));
        }
        // 规则 2.5：{dir} 之后必须紧跟以路径分隔符开头的字面量（如 {dir}/{name}）。
        // 缺失时 {dir} 与后续槽位直接拼接（"logs" + "app..." = "logsapp..."），
        // 轮转文件落到日志目录之外的兄弟路径：scan_dir 扫不到，max_backups 清理
        // 与 compress 全部静默失效。解析侧用同一谓词跳过该字面量，两侧必须一致。
        if !matches!(slots.get(1), Some(Slot::Literal(t)) if is_path_prefix(t)) {
            return Err(Error::Template(
                "{dir} 之后必须紧跟路径分隔符（如 {dir}/{name}...）".to_string(),
            ));
        }
        // 规则 3：{name} 与 {date} 必须出现
        if !slots.contains(&Slot::Known(Anchor::Name)) {
            return Err(Error::Template("模板必须包含 {name}".to_string()));
        }
        if !slots.contains(&Slot::Date) {
            return Err(Error::Template("模板必须包含 {date}".to_string()));
        }
        Ok(Template {
            slots,
            date_format: date_format.to_string(),
        })
    }

    pub(crate) fn date_format(&self) -> &str {
        &self.date_format
    }

    /// 模板是否包含 {counter} 槽位（无 counter 模板同 date 冲突直接失败，不递增）
    pub(crate) fn has_counter(&self) -> bool {
        self.slots.contains(&Slot::Counter)
    }

    /// 渲染：占位符替换（生成方向，设计文档 §5.5）
    pub(crate) fn render(&self, known: &KnownValues, date: &str, counter: Option<u32>) -> PathBuf {
        let mut s = String::new();
        for slot in &self.slots {
            match slot {
                Slot::Literal(t) => s.push_str(t),
                Slot::Known(a) => s.push_str(known.value(*a)),
                Slot::Date => s.push_str(date),
                Slot::Counter => {
                    if let Some(c) = counter {
                        s.push_str(&c.to_string());
                    }
                }
            }
        }
        // suffix 为空时省略末尾分隔符：模板多以 {sep}{suffix} 结尾，suffix 为空会
        // 渲染出以分隔符收尾的文件名（如 "app.<date>.1."）。Windows 会自动剥离文件名
        // 尾部的点/空格，落盘名因此与模板不匹配 —— parse 无法识别，max_backups 清理
        // 与 compress 全部失效（实测：max_backups=Some(2) 下归档无限累积）。
        // 与 parse 侧"补回分隔符重试"成对，二者必须同时存在。
        if known.suffix.is_empty()
            && let Some(stripped) = s.strip_suffix(known.sep.as_str())
        {
            s = stripped.to_string();
        }
        PathBuf::from(s)
    }

    /// 解析文件名：贪婪优先 + 回溯匹配（扫描方向，设计文档 §5.6）。
    /// 输入是目录内文件名（不含 {dir}/ 前缀），跳过头部 Dir 与路径分隔符字面量。
    pub(crate) fn parse(&self, known: &KnownValues, file_name: &str) -> Option<ParsedName> {
        // 0. 压缩标记
        let (name, compressed) = match file_name.strip_suffix(".gz") {
            Some(n) => (n, true),
            None => (file_name, false),
        };
        // 1. 跳过头部 {dir} + "/" 槽位
        let mut slots: &[Slot] = &self.slots;
        if slots.first() == Some(&Slot::Known(Anchor::Dir)) {
            slots = &slots[1..];
        }
        // 与编译规则 2.5 同一谓词
        if let Some(Slot::Literal(t)) = slots.first()
            && is_path_prefix(t)
        {
            slots = &slots[1..];
        }
        // 2. 回溯匹配
        let mut date = String::new();
        let mut counter: Option<u32> = None;
        let mut matched = self.backtrack(known, name, slots, 0, &mut date, &mut counter);
        // 失败且 suffix 为空：补回末尾分隔符重试（对应 render 的省略逻辑；
        // 同时兼容历史遗留的带尾分隔符文件）。backtrack 失败时不写入 date/counter，
        // 故重试前无需重置
        if !matched && known.suffix.is_empty() {
            let with_sep = format!("{name}{}", known.sep);
            matched = self.backtrack(known, &with_sep, slots, 0, &mut date, &mut counter);
        }
        if !matched {
            return None;
        }
        // 3. 后置校验：date 非空且不含 ASCII 字母（date_format 默认为数字+分隔符）
        if date.is_empty() || date.bytes().any(|b| b.is_ascii_alphabetic()) {
            return None;
        }
        let rotation_key = match counter {
            Some(c) => format!("{}{}{}", date, known.sep, c),
            None => date.clone(),
        };
        Some(ParsedName {
            rotation_key,
            date,
            counter,
            compressed,
        })
    }

    /// 回溯匹配：贪婪优先（Date / Counter 均从长到短枚举分割点）
    fn backtrack(
        &self,
        known: &KnownValues,
        name: &str,
        slots: &[Slot],
        pos: usize,
        date: &mut String,
        counter: &mut Option<u32>,
    ) -> bool {
        let Some(first) = slots.first() else {
            return pos == name.len();
        };
        match first {
            Slot::Literal(t) => {
                name[pos..].starts_with(t.as_str())
                    && self.backtrack(known, name, &slots[1..], pos + t.len(), date, counter)
            }
            Slot::Known(a) => {
                let v = known.value(*a);
                name[pos..].starts_with(v)
                    && self.backtrack(known, name, &slots[1..], pos + v.len(), date, counter)
            }
            Slot::Counter => {
                // pos 起连续数字段，从长到短枚举
                let bytes = name.as_bytes();
                let mut end = pos;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                if end == pos {
                    return false;
                }
                for e in (pos + 1..=end).rev() {
                    if let Ok(num) = name[pos..e].parse::<u32>()
                        && self.backtrack(known, name, &slots[1..], e, date, counter)
                    {
                        *counter = Some(num);
                        return true;
                    }
                }
                false
            }
            Slot::Date => {
                // 从长到短枚举分割点（贪婪优先，保证 date 取最长可匹配段）
                for e in (pos + 1..=name.len()).rev() {
                    if !name.is_char_boundary(e) {
                        continue;
                    }
                    if self.backtrack(known, name, &slots[1..], e, date, counter) {
                        *date = name[pos..e].to_string();
                        return true;
                    }
                }
                false
            }
        }
    }
}

/// 是否为目录前缀字面量：{dir} 之后必须以路径分隔符开头，否则目录与文件名会
/// 直接拼接成兄弟路径。编译规则 2.5 与解析侧的头部跳过共用同一谓词，
/// 保证渲染出的路径能被原样解析回来
fn is_path_prefix(text: &str) -> bool {
    text.starts_with('/') || text.starts_with('\\')
}

/// 相邻字面量合并
fn push_literal(slots: &mut Vec<Slot>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Slot::Literal(last)) = slots.last_mut() {
        last.push_str(text);
    } else {
        slots.push(Slot::Literal(text.to_string()));
    }
}

/// 目录扫描与排序（设计文档 §5.7）：
/// mtime 降序（最新在前）为主排序；mtime 相同按 rotation_key 自然序兜底。
/// 同一 rotation_key 的未压缩与压缩版本去重为一条。
pub(crate) fn scan_dir(
    known: &KnownValues,
    template: &Template,
    exclude_key: Option<&str>,
) -> Vec<RotatedFile> {
    let mut map: HashMap<String, (ParsedName, SystemTime)> = HashMap::new();
    let active = known.active_file_name();
    let Ok(entries) = std::fs::read_dir(&known.dir) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // 跳过活跃文件
        if file_name == active {
            continue;
        }
        // 非本库管理的文件
        let Some(parsed) = template.parse(known, &file_name) else {
            continue;
        };
        // 排除刚轮转文件
        if Some(parsed.rotation_key.as_str()) == exclude_key {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        map.entry(parsed.rotation_key.clone())
            .and_modify(|(p, t)| {
                p.compressed |= parsed.compressed;
                *t = (*t).max(mtime);
            })
            .or_insert((parsed, mtime));
    }
    let mut files: Vec<RotatedFile> = map
        .into_iter()
        .map(|(_, (parsed, mtime))| RotatedFile { parsed, mtime })
        .collect();
    files.sort_by(|a, b| {
        b.mtime
            .cmp(&a.mtime)
            .then_with(|| natural_cmp(&b.parsed.rotation_key, &a.parsed.rotation_key))
    });
    files
}

/// 自然序比较：连续数字段按数值比较，其余按字节序（设计文档 §5.8）。
/// 保证 `"log.9" < "log.10"`、`"007" == "7"`。
pub(crate) fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < x.len() && j < y.len() {
        if x[i].is_ascii_digit() && y[j].is_ascii_digit() {
            let (si, sj) = (i, j);
            while i < x.len() && x[i].is_ascii_digit() {
                i += 1;
            }
            while j < y.len() && y[j].is_ascii_digit() {
                j += 1;
            }
            // 去前导零后先比长度再比字节序，避免数值溢出
            let na = &x[si..i];
            let nb = &y[sj..j];
            let za = na.iter().take_while(|&&c| c == b'0').count();
            let zb = nb.iter().take_while(|&&c| c == b'0').count();
            let va = &na[za..];
            let vb = &nb[zb..];
            let ord = va.len().cmp(&vb.len()).then_with(|| va.cmp(vb));
            if ord != Ordering::Equal {
                return ord;
            }
        } else {
            let ord = x[i].cmp(&y[j]);
            if ord != Ordering::Equal {
                return ord;
            }
            i += 1;
            j += 1;
        }
    }
    (x.len() - i).cmp(&(y.len() - j))
}
