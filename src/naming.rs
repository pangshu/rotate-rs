//! 命名模板引擎：编译、渲染、锚点解析、目录扫描排序（设计文档 §5）

use std::borrow::Cow;
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
    /// "缺省 {counter}"形态的槽序列（`counter_optional_variant` 的产物），
    /// 用于识别 `render(.., None)` 产出的无序号归档名；无 {counter} 模板为 None
    slots_optional: Option<Vec<Slot>>,
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
            slots_optional: counter_optional_variant(&slots),
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

    /// 渲染：占位符替换（生成方向，设计文档 §5.5）。
    ///
    /// `counter` 为 `None` 表示本次轮转不带序号段：`{counter}` 渲染为空，
    /// 且若其紧邻的前一个槽位是 `{sep}`，该分隔符一并省略 —— 否则会渲染出
    /// `app.<date>..log` 这样的双点名。解析侧的 `slots_optional` 是同一规则的
    /// 逆运算（见 `counter_optional_variant`），二者必须同时存在。
    pub(crate) fn render(&self, known: &KnownValues, date: &str, counter: Option<u32>) -> PathBuf {
        let mut s = String::new();
        for (i, slot) in self.slots.iter().enumerate() {
            match slot {
                Slot::Literal(t) => s.push_str(t),
                Slot::Known(a) => s.push_str(known.value(*a)),
                Slot::Date => s.push_str(date),
                Slot::Counter => match counter {
                    Some(c) => s.push_str(&c.to_string()),
                    None => {
                        if i > 0
                            && self.slots[i - 1] == Slot::Known(Anchor::Sep)
                            && !known.sep.is_empty()
                            && s.ends_with(known.sep.as_str())
                        {
                            s.truncate(s.len() - known.sep.len());
                        }
                    }
                },
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
    ///
    /// 先按"带 {counter}"形态匹配（更具体，优先保证 `app.<date>.1.log` 的
    /// date/counter 切分正确），失败再按"缺省 {counter}"形态匹配 —— 后者对应
    /// `render(.., None)` 产出的 `app.<date>.log` 这类无序号归档名。
    /// 缺了第二种形态，无序号归档会被本函数漏识别 → `scan_dir` 跳过 →
    /// `max_backups` 清理与 `compress` 补压全部静默失效。
    pub(crate) fn parse(&self, known: &KnownValues, file_name: &str) -> Option<ParsedName> {
        // 0. 压缩标记
        let (name, compressed) = match file_name.strip_suffix(".gz") {
            Some(n) => (n, true),
            None => (file_name, false),
        };
        // 1. 跳过头部 {dir} + "/" 槽位（两种形态共用同一谓词，见 strip_head）
        // 2. 回溯匹配
        let (date, counter) = self
            .match_slots(known, name, strip_head(&self.slots))
            .or_else(|| {
                self.slots_optional
                    .as_deref()
                    .and_then(|slots| self.match_slots(known, name, strip_head(slots)))
            })?;
        // 3. 组合 key（去重与排除刚轮转文件用）：无 counter 时即 date 本身
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

    /// 用给定槽序列匹配文件名，返回 `(date, counter)`。
    ///
    /// 后置校验（date 非空且不含 ASCII 字母，date_format 默认为数字+分隔符）
    /// 在此完成：不满足的匹配按失败处理，让调用方能继续尝试另一种形态。
    fn match_slots(
        &self,
        known: &KnownValues,
        name: &str,
        slots: &[Slot],
    ) -> Option<(String, Option<u32>)> {
        let mut candidates: Vec<Cow<'_, str>> = vec![Cow::Borrowed(name)];
        // suffix 为空：补回末尾分隔符重试（对应 render 的省略逻辑；
        // 同时兼容历史遗留的带尾分隔符文件）
        if known.suffix.is_empty() {
            candidates.push(Cow::Owned(format!("{name}{}", known.sep)));
        }
        for cand in candidates {
            let mut date = String::new();
            let mut counter: Option<u32> = None;
            if self.backtrack(known, &cand, slots, 0, &mut date, &mut counter)
                && !date.is_empty()
                && !date.bytes().any(|b| b.is_ascii_alphabetic())
            {
                return Some((date, counter));
            }
        }
        None
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

/// 跳过槽序列头部的 `{dir}` 与紧随其后的路径分隔符字面量。
/// 解析的两种形态（带 / 不带 {counter}）共用，保证两侧跳过逻辑一致。
fn strip_head(slots: &[Slot]) -> &[Slot] {
    let mut s = slots;
    if s.first() == Some(&Slot::Known(Anchor::Dir)) {
        s = &s[1..];
    }
    if let Some(Slot::Literal(t)) = s.first()
        && is_path_prefix(t)
    {
        s = &s[1..];
    }
    s
}

/// 构造"缺省 {counter}"形态的槽序列：移除首个 `{counter}` 槽；若其紧邻的
/// 前一个槽位是 `{sep}`，一并移除。与 `render` 中 `counter == None` 时的省略
/// 规则成对（缺一侧就会出现"渲染出的名字解析不回来"的静默失效）。
/// 模板不含 `{counter}` 时返回 `None`。
fn counter_optional_variant(slots: &[Slot]) -> Option<Vec<Slot>> {
    let idx = slots.iter().position(|s| *s == Slot::Counter)?;
    let sep_idx = (idx > 0 && slots[idx - 1] == Slot::Known(Anchor::Sep)).then(|| idx - 1);
    Some(
        slots
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != idx && Some(*i) != sep_idx)
            .map(|(_, s)| s.clone())
            .collect(),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_TPL: &str = "{dir}/{name}{sep}{date}{sep}{counter}{sep}{suffix}";
    const DATE: &str = "2026-10-11-14-15-03";

    fn known() -> KnownValues {
        KnownValues {
            dir: "logs".to_string(),
            name: "app".to_string(),
            sep: ".".to_string(),
            suffix: "log".to_string(),
        }
    }

    fn tpl(template: &str) -> Template {
        Template::compile(template, "%Y-%m-%d-%H-%M-%S").unwrap()
    }

    /// counter 为 None 时：序号段连同其前的 {sep} 一并省略，不得出现双点
    #[test]
    fn render_without_counter_omits_sep_and_counter() {
        let (k, t) = (known(), tpl(DEFAULT_TPL));
        assert_eq!(
            t.render(&k, DATE, None),
            PathBuf::from("logs/app.2026-10-11-14-15-03.log")
        );
        assert_eq!(
            t.render(&k, DATE, Some(1)),
            PathBuf::from("logs/app.2026-10-11-14-15-03.1.log")
        );
    }

    /// 无序号归档名必须能被解析回来（否则 scan_dir 跳过 → 清理/压缩静默失效）
    #[test]
    fn parse_recognizes_both_forms() {
        let (k, t) = (known(), tpl(DEFAULT_TPL));

        let plain = t.parse(&k, "app.2026-10-11-14-15-03.log").unwrap();
        assert_eq!(plain.date, DATE);
        assert_eq!(plain.counter, None);
        assert_eq!(plain.rotation_key, DATE);

        let numbered = t.parse(&k, "app.2026-10-11-14-15-03.1.log").unwrap();
        assert_eq!(numbered.date, DATE);
        assert_eq!(numbered.counter, Some(1));
        assert_eq!(numbered.rotation_key, "2026-10-11-14-15-03.1");

        let gz = t.parse(&k, "app.2026-10-11-14-15-03.log.gz").unwrap();
        assert!(gz.compressed);
        assert_eq!(gz.counter, None);
    }

    /// 渲染 → 解析 → 再渲染 应完全稳定（无序号 / 有序号 / 多位数序号）
    #[test]
    fn render_parse_round_trip_is_stable() {
        let (k, t) = (known(), tpl(DEFAULT_TPL));
        for counter in [None, Some(1), Some(7), Some(123)] {
            let path = t.render(&k, DATE, counter);
            let file_name = path.file_name().unwrap().to_str().unwrap();
            let parsed = t.parse(&k, file_name).unwrap();
            assert_eq!(parsed.counter, counter, "counter 往返不一致: {file_name}");
            assert_eq!(parsed.date, DATE);
            assert_eq!(t.render(&k, &parsed.date, parsed.counter), path);
        }
    }

    /// 非本库管理的文件不得被误识别
    #[test]
    fn unrelated_files_are_rejected() {
        let (k, t) = (known(), tpl(DEFAULT_TPL));
        assert!(t.parse(&k, "app.log").is_none());
        assert!(t.parse(&k, "other.2026-10-11-14-15-03.log").is_none());
        assert!(t.parse(&k, "app.2026-10-11-14-15-03.txt").is_none());
    }

    /// 模板不含 {counter}：没有"缺省形态"，渲染结果与无序号形态一致
    #[test]
    fn template_without_counter_has_no_optional_variant() {
        let (k, t) = (known(), tpl("{dir}/{name}{sep}{date}{sep}{suffix}"));
        assert!(!t.has_counter());
        assert!(t.slots_optional.is_none());
        assert_eq!(
            t.render(&k, DATE, None),
            PathBuf::from("logs/app.2026-10-11-14-15-03.log")
        );
        assert!(t.parse(&k, "app.2026-10-11-14-15-03.log").is_some());
    }

    /// suffix 为空：省略尾分隔符，且无序号形态仍能往返
    #[test]
    fn empty_suffix_round_trips_without_counter() {
        let k = KnownValues {
            suffix: String::new(),
            ..known()
        };
        let t = tpl(DEFAULT_TPL);
        let path = t.render(&k, DATE, None);
        assert_eq!(path, PathBuf::from("logs/app.2026-10-11-14-15-03"));
        let file_name = path.file_name().unwrap().to_str().unwrap();
        let parsed = t.parse(&k, file_name).unwrap();
        assert_eq!(parsed.counter, None);
        assert_eq!(parsed.date, DATE);
    }

    /// {counter} 前的槽位不是 {sep}（如字面量）时只省略 counter 本身，
    /// 且该形态同样可被解析回来
    #[test]
    fn literal_before_counter_is_kept_and_parseable() {
        let template = "{dir}/{name}{sep}{date}_{counter}{sep}{suffix}";
        let (k, t) = (known(), tpl(template));
        let path = t.render(&k, DATE, None);
        assert_eq!(path, PathBuf::from("logs/app.2026-10-11-14-15-03_.log"));
        let file_name = path.file_name().unwrap().to_str().unwrap();
        let parsed = t.parse(&k, file_name).unwrap();
        assert_eq!(parsed.counter, None);
        assert_eq!(parsed.date, DATE);
    }
}
