//! 输入串 → 音节序列的切分。
//!
//! 拼音输入是一串连续字母（`nihao`），必须切成音节（`[ni, hao]`）才能查表。
//! **同一串常有多种合法切分**（`xian` → `[xian]` 或 `[xi, an]`），
//! 两种都要保留 —— 这决定了「西安 / 先」这类歧义输入能否正确出词。

use crate::format::{encode_syllable, MAX_SYLLABLE_LEN, SyllableKey};
use std::collections::HashSet;

/// 拼音缓冲的**显式上限**。
///
/// 切分数随长度呈斐波那契式增长，而 `should_consume` 会吃掉所有裸字母，
/// 长按字母会放大计算量与内存。超限时降级为**最长切分**。
pub const MAX_INPUT_LEN: usize = 32;

/// 一种切分：音节串（已编码）
pub type Segmentation = Vec<SyllableKey>;

/// 切分器
pub struct Segmenter {
    /// 合法音节的编码集合
    syllables: HashSet<SyllableKey>,
}

impl Segmenter {
    pub fn new<'a>(syllables: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            syllables: syllables
                .into_iter()
                .filter_map(encode_syllable)
                .collect(),
        }
    }

    pub fn syllable_count(&self) -> usize {
        self.syllables.len()
    }

    /// 这个串是不是一个合法音节（engine 做前缀展开的可达性 DP 时要用）
    #[inline]
    pub fn is_syllable(&self, s: &str) -> bool {
        encode_syllable(s).is_some_and(|k| self.syllables.contains(&k))
    }

    /// 求**全部**合法切分。
    ///
    /// DP：`ways[i]` = 切完前 `i` 个字节的所有方案（存前驱下标 + 音节键）。
    /// 为避免组合爆炸，每种切分最多保留到 `limit` 种；`limit` 之外截断。
    ///
    /// 输入含非 `[a-z]` 字节时返回空（调用方应在此之前过滤）。
    pub fn segment(&self, input: &str, limit: usize) -> Vec<Segmentation> {
        let bytes = input.as_bytes();
        let n = bytes.len();
        if n == 0 || n > MAX_INPUT_LEN || !bytes.iter().all(|b| b.is_ascii_lowercase()) {
            return Vec::new();
        }

        // back[i] = 到达位置 i 的方案：(前一位置, 音节键)
        let mut back: Vec<Vec<(usize, SyllableKey)>> = vec![Vec::new(); n + 1];
        back[0].push((usize::MAX, 0)); // 起点哨兵

        for i in 1..=n {
            for l in 1..=MAX_SYLLABLE_LEN.min(i) {
                let start = i - l;
                if back[start].is_empty() {
                    continue;
                }
                // SAFETY: start..i 落在 input 内，且已确认全是 ASCII
                let piece = &input[start..i];
                if let Some(key) = encode_syllable(piece).filter(|k| self.syllables.contains(k)) {
                    back[i].push((start, key));
                }
            }
            // 截断：防止组合爆炸
            if back[i].len() > limit {
                back[i].truncate(limit);
            }
        }

        if back[n].is_empty() {
            return Vec::new();
        }
        // 回溯重建
        let mut out = Vec::new();
        let mut stack = vec![(n, Vec::new())];
        while let Some((pos, acc)) = stack.pop() {
            if pos == 0 {
                let mut s = acc;
                s.reverse();
                out.push(s);
                if out.len() >= limit {
                    break;
                }
                continue;
            }
            for &(prev, key) in &back[pos] {
                if prev == usize::MAX {
                    continue;
                }
                let mut next = acc.clone();
                next.push(key);
                stack.push((prev, next));
            }
        }
        out
    }

    /// 降级切分：只求**最长音节优先**的一种切分（用于超限或兜底）。
    ///
    /// 贪心从右往左，保证切完；切不动时按单字母硬切。
    pub fn segment_longest(&self, input: &str) -> Segmentation {
        let bytes = input.as_bytes();
        let n = bytes.len();
        let mut out = Vec::new();
        let mut i = 0;
        while i < n {
            let mut matched = None;
            let hi = (i + MAX_SYLLABLE_LEN).min(n);
            // 从长到短试，取最长匹配
            for end in (i + 1..=hi).rev() {
                if let Some(key) =
                    encode_syllable(&input[i..end]).filter(|k| self.syllables.contains(k))
                {
                    matched = Some((end, key));
                    break;
                }
            }
            match matched {
                Some((end, key)) => {
                    out.push(key);
                    i = end;
                }
                None => {
                    // 硬切一个字母（理论上不该发生，因为单字母音节基本都存在）
                    i += 1;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的音节集（真实数据里是 414 个，这里取够用的子集）
    const SYL: &[&str] = &[
        "a", "ai", "an", "ang", "ao", "ba", "chang", "chi", "de", "di", "e", "en", "er", "ha",
        "hao", "he", "hi", "hua", "huan", "ji", "jia", "jian", "jiao", "jin", "jing", "jiu", "le",
        "li", "lian", "liang", "ma", "man", "mei", "men", "mi", "mian", "ming", "mo", "mu", "na",
        "nai", "nan", "ne", "nei", "ni", "nian", "niang", "nin", "ning", "niu", "nv", "nve", "o",
        "ou", "qi", "qian", "qiang", "qie", "qin", "qing", "qiu", "qu", "quan", "que", "ren",
        "ri", "rong", "rou", "ru", "ruan", "rui", "sa", "sai", "san", "shang", "she", "shen",
        "sheng", "shi", "shou", "shu", "shua", "shuai", "shuan", "shuang", "shui", "shun", "shuo",
        "si", "song", "sou", "su", "suan", "sui", "sun", "suo", "ta", "tai", "tan", "tang", "te",
        "ti", "tian", "tiao", "tie", "ting", "tong", "tou", "tu", "tuan", "tui", "tun", "tuo",
        "wa", "wai", "wan", "wang", "wei", "wen", "weng", "wo", "wu", "xi", "xia", "xian",
        "xiang", "xiao", "xie", "xin", "xing", "xiong", "xiu", "xu", "xuan", "xue", "xun", "ya",
        "yan", "yang", "yao", "ye", "yi", "yin", "ying", "yo", "yong", "you", "yu", "yuan", "yue",
        "yun", "zha", "zhai", "zhan", "zhang", "zhao", "zhe", "zhen", "zheng", "zhi", "zhong",
        "zhou", "zhu", "zhua", "zhuai", "zhuan", "zhuang", "zhui", "zhun", "zhuo", "zi", "zong",
        "zou", "zu", "zuan", "zui", "zun", "zuo",
    ];

    fn seg() -> Segmenter {
        Segmenter::new(SYL.iter().copied())
    }

    fn decode(s: &Segmentation, _seg: &Segmenter) -> Vec<String> {
        // 用编码反查（测试辅助）：从音节集里找匹配的
        s.iter()
            .map(|k| {
                SYL.iter()
                    .find(|x| encode_syllable(x) == Some(*k))
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| format!("?{k}"))
            })
            .collect()
    }

    #[test]
    fn simple() {
        let s = seg();
        let r = s.segment("nihao", 8);
        let got: Vec<Vec<String>> = r.iter().map(|x| decode(x, &s)).collect();
        // 注意：nihao 并非只有一种切分 —— [ni,ha,o] 也合法（ha=哈、o=哦）。
        // 这正说明「多切分都要保留」是必需的，而不是可选优化。
        assert!(
            got.contains(&vec!["ni".to_string(), "hao".to_string()]),
            "缺 [ni, hao]: {got:?}"
        );
        assert!(
            got.contains(&vec!["ni".to_string(), "ha".to_string(), "o".to_string()]),
            "缺 [ni, ha, o]: {got:?}"
        );
    }

    #[test]
    fn ambiguous_keeps_both() {
        // xian 的两种切分都要保留
        let s = seg();
        let r = s.segment("xian", 8);
        let mut got: Vec<Vec<String>> = r.iter().map(|x| decode(x, &s)).collect();
        got.sort();
        assert!(got.contains(&vec!["xian".to_string()]), "缺 [xian]: {got:?}");
        assert!(
            got.contains(&vec!["xi".to_string(), "an".to_string()]),
            "缺 [xi, an]: {got:?}"
        );
    }

    #[test]
    fn no_segmentation_for_garbage() {
        let s = seg();
        assert!(s.segment("zzzz", 8).is_empty());
        assert!(s.segment("", 8).is_empty());
        assert!(s.segment("NI", 8).is_empty(), "大写应被拒");
        assert!(s.segment("ni3", 8).is_empty(), "非字母应被拒");
    }

    #[test]
    fn overlong_input_rejected() {
        let s = seg();
        let long = "a".repeat(MAX_INPUT_LEN + 1);
        assert!(s.segment(&long, 8).is_empty(), "超限应返回空，由调用方降级");
    }

    #[test]
    fn longest_fallback_terminates() {
        let s = seg();
        // 即使切不动也要能返回，不能死循环
        let r = s.segment_longest("zzzz");
        assert!(r.len() <= 4);
        let r2 = s.segment_longest("nihaoxian");
        assert!(!r2.is_empty());
    }

    #[test]
    fn segmentation_count_bounded_by_limit() {
        let s = seg();
        // "xianxianxian" 这类多歧义串的切分数应被 limit 截断
        let r = s.segment("xianxianxian", 4);
        assert!(r.len() <= 4, "切分数应被 limit 截断，实际 {}", r.len());
    }
}
