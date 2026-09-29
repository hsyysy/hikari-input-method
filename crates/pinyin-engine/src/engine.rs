//! 引擎门面：输入串 → 排好序的候选词。
//!
//! 排序策略：
//! `score = ln(词频) + 覆盖度奖励 - 单字惩罚`
//!
//! **多切分的合并规则**（§6.2 明确要求，不是可选）：
//! 各切分**分别取 top-N**，再按分数**全局归并去重**，共享首屏配额。
//! 不让某个切分独占首屏，否则「西安 / 先」这类歧义的另一读法会彻底不可见。

use std::path::Path;

use crate::dict::{Candidate, Dict, DictError};
use crate::format::{encode_syllable, MAX_SYLLABLE_LEN};
use crate::segment::{Segmenter, MAX_INPUT_LEN};

/// 每种切分最多取多少条候选进入合并
const PER_SEGMENTATION_LIMIT: usize = 8;
/// 最多保留多少种切分（防止组合爆炸）
const SEGMENTATION_LIMIT: usize = 6;
/// 末尾不完整音节最多展开成多少个音节（`s` → ~35 个，留余量）
const MAX_EXPANSIONS: usize = 64;

pub struct Engine {
    dict: Dict,
    segmenter: Segmenter,
    /// 音节键 → id。建一次表，避免每个音节都线性扫 414 项还顺带分配一个 Vec。
    id_of: std::collections::HashMap<u64, u16>,
    /// 音节字符串（id 顺序）。缓存下来供「前缀展开」用，避免每次查都重新解析 blob。
    syllables: Vec<String>,
}

impl Engine {
    /// 打开词典文件
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DictError> {
        let dict = Dict::open(path)?;
        Ok(Self::new(dict))
    }

    /// 从内存字节构造（测试用）
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DictError> {
        let dict = Dict::from_bytes(bytes)?;
        Ok(Self::new(dict))
    }

    fn new(dict: Dict) -> Self {
        let syllables: Vec<String> = dict.syllables().iter().map(|s| s.to_string()).collect();
        let segmenter = Segmenter::new(syllables.iter().map(|s| s.as_str()));
        let id_of = syllables
            .iter()
            .enumerate()
            .filter_map(|(i, s)| encode_syllable(s).map(|k| (k, i as u16)))
            .collect();
        Self {
            dict,
            segmenter,
            id_of,
            syllables,
        }
    }

    pub fn dict(&self) -> &Dict {
        &self.dict
    }

    pub fn syllable_count(&self) -> usize {
        self.segmenter.syllable_count()
    }

    /// 把拼音串按音节切开，用于**显示**（`nihao` → `ni'hao`）。
    ///
    /// 用最长切分（确定且可复现），与候选查询用的多切分无关 ——
    /// 这只是给用户看"我把它切成了哪几个音节"。
    pub fn display_preedit(&self, input: &str) -> String {
        if input.is_empty() {
            return String::new();
        }
        let seg = if input.len() > MAX_INPUT_LEN {
            vec![self.segmenter.segment_longest(input)]
        } else {
            let s = self.segmenter.segment(input, 1);
            if s.is_empty() {
                vec![self.segmenter.segment_longest(input)]
            } else {
                s
            }
        };
        let Some(first) = seg.first() else {
            return input.to_string();
        };
        // 音节键 → 长度（字母数）：键的低 3 位存 len-1
        let mut out = String::with_capacity(input.len() + first.len());
        let mut pos = 0usize;
        for (i, key) in first.iter().enumerate() {
            let len = ((key & 0b111) + 1) as usize;
            if pos + len > input.len() {
                break;
            }
            if i > 0 {
                out.push('\'');
            }
            out.push_str(&input[pos..pos + len]);
            pos += len;
        }
        if pos < input.len() {
            out.push_str(&input[pos..]);
        }
        out
    }

    /// 找**最长的可完全切分成音节的前缀**，返回 (该前缀的音节键, 剩余的不完整部分)。
    ///
    /// 例：`s` → `([], "s")`；`nih` → `([ni], "h")`；`nihao` → `([ni,hao], "")`。
    ///
    /// 为什么要单独找：用户从第一个字母开始就该看到候选（输入 `s` 就想看到「是/三/说」），
    /// 而 `s` 本身不是完整音节，必须按**音节前缀**去展开。
    fn split_partial<'a>(&self, input: &'a str) -> (Vec<u64>, &'a str) {
        let n = input.len();
        // 可达性 DP：reachable[i] = 前 i 个字节能否完全切成音节
        let mut reachable = vec![false; n + 1];
        reachable[0] = true;
        for i in 0..n {
            if !reachable[i] {
                continue;
            }
            let hi = (i + MAX_SYLLABLE_LEN).min(n);
            for end in i + 1..=hi {
                if self.segmenter.is_syllable(&input[i..end]) {
                    reachable[end] = true;
                }
            }
        }
        // 最长的可达位置
        let best = (0..=n).rev().find(|&i| reachable[i]).unwrap_or(0);
        let (complete, partial) = input.split_at(best);
        if complete.is_empty() {
            return (Vec::new(), partial);
        }
        // 完整部分用最长切分给出音节序列
        let keys = self
            .segmenter
            .segment_longest(complete)
            .into_iter()
            .collect();
        (keys, partial)
    }

    /// 所有以 `prefix` 开头的音节 id。
    ///
    /// 例：`"s"` → sa/sai/san/…/shi/si/song/su… 的 id。
    fn syllables_with_prefix(&self, prefix: &str) -> Vec<u16> {
        self.syllables
            .iter()
            .enumerate()
            .filter(|(_, s)| s.starts_with(prefix))
            .map(|(i, _)| i as u16)
            .collect()
    }

    /// 主入口：输入拼音串，返回排好序的候选。
    ///
    /// `input` 应已过滤为 `[a-z]`；含其它字符会返回空。
    ///
    /// 分两条路：
    /// - **能完全切成音节** → 走多切分逻辑（`xian` 的两种读法都要出）
    /// - **末尾有不完整片段** → 按**音节前缀展开**（输入 `s` 就出「是/三/说」）
    pub fn candidates(&self, input: &str, top_n: usize) -> Vec<Candidate> {
        if input.is_empty() {
            return Vec::new();
        }

        let (complete_keys, partial) = self.split_partial(input);
        let base: Vec<u16> = complete_keys
            .iter()
            .filter_map(|k| self.id_of.get(k).copied())
            .collect();

        let mut raw: Vec<Candidate> = Vec::new();

        if partial.is_empty() {
            // ── 完全切分：保留全部合法切分 ──
            // 超限时降级为最长切分（§6.2 的显式上限）
            let segmentations = if input.len() > MAX_INPUT_LEN {
                vec![self.segmenter.segment_longest(input)]
            } else {
                let s = self.segmenter.segment(input, SEGMENTATION_LIMIT);
                if s.is_empty() {
                    vec![self.segmenter.segment_longest(input)]
                } else {
                    s
                }
            };
            for seg in &segmentations {
                let Some(ids) = seg
                    .iter()
                    .map(|k| self.id_of.get(k).copied())
                    .collect::<Option<Vec<u16>>>()
                else {
                    continue;
                };
                collect(
                    &self.dict,
                    &self.syllables,
                    &ids,
                    input.len(),
                    ids.len(),
                    PER_SEGMENTATION_LIMIT,
                    &mut raw,
                );
            }
        } else {
            // ── 末尾不完整：按音节前缀展开 ──
            //
            // 展开后音节数 = 已完整部分 + 1（那个不完整的音节）。
            // 这个 total 用于打分：完整覆盖它的候选才会拿到 complete 奖励。
            let total = base.len() + 1;

            // 已完整部分自身的候选（如 `nih` 的「你」）—— 覆盖不全，分数自然靠后
            if !base.is_empty() {
                collect(
                    &self.dict,
                    &self.syllables,
                    &base,
                    input.len(),
                    total,
                    PER_SEGMENTATION_LIMIT,
                    &mut raw,
                );
            }

            // 逐个展开音节：`s` → sa/sai/san/…/shi/si/song/su…
            let expansions = self.syllables_with_prefix(partial);
            for sid in expansions.into_iter().take(MAX_EXPANSIONS) {
                let mut ids = base.clone();
                ids.push(sid);
                collect(
                    &self.dict,
                    &self.syllables,
                    &ids,
                    input.len(),
                    total,
                    PER_SEGMENTATION_LIMIT,
                    &mut raw,
                );
            }
        }

        merge(raw, top_n)
    }
}

/// 走一遍 trie、打分、取前 N 条塞进 `out`。
///
/// `total` **必须由调用方给出**，不能默认取 `ids.len()` ——
/// 末尾有未完成音节时，「已完整部分」单独走查的音节数更少，
/// 若让它按自己的长度算就会被误判成「完整覆盖」而拿到不该拿的奖励。
/// 实测：`nih` 的「你」曾因此排在「你好」前面。
///
/// `input_len` 是用户**实际敲的**字母数，用于夹住 `consumed`（见下）。
fn collect(
    dict: &Dict,
    syllables: &[String],
    ids: &[u16],
    input_len: usize,
    total: usize,
    limit: usize,
    out: &mut Vec<Candidate>,
) {
    let mut hits = Vec::new();
    dict.walk(ids, limit, &mut hits);
    // 填 `consumed`：命中音节的字母数之和。
    //
    // ⚠️ 必须夹到 `input_len`。末尾音节不完整时会走**前缀展开**
    // （`zhel` 的 `l` 展开成 `li`），展开出的音节比用户实际敲的长：
    // 不夹的话「这里」在 `zhel` 下算出来是 5 > 4，会被当成"只覆盖了一部分"
    // 而拒绝上屏 —— 明明它把整个输入都吃下了。
    for h in hits.iter_mut() {
        h.consumed = ids[..h.matched]
            .iter()
            .map(|&i| syllables[i as usize].len())
            .sum::<usize>()
            .min(input_len);
    }
    rank(&mut hits, total);
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits.truncate(limit);
    out.append(&mut hits);
}

/// 全局归并去重（同词保留分最高的那次），取前 `top_n`
fn merge(mut raw: Vec<Candidate>, top_n: usize) -> Vec<Candidate> {
    raw.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(top_n);
    for c in raw {
        if seen.insert(c.word.clone()) {
            out.push(c);
            if out.len() >= top_n {
                break;
            }
        }
    }
    out
}

/// 填 `score`。`total` 是本次输入的音节总数。
pub fn rank(cands: &mut [Candidate], total: usize) {
    for c in cands.iter_mut() {
        c.score = score(c.weight, c.matched, total);
    }
}

/// 打分函数。
///
/// # 三个成分
///
/// 1. **`ln(weight)`** —— 词频。用对数是为了不让超高频词压死一切。
/// 2. **完整覆盖奖励** —— 匹配的音节数 == 输入音节数时给一个大奖励。
///    这一项是必需的：单字「是」的词频比词「世界」高两个数量级，
///    若只按词频排，输入 `shijie` 会把「是」排在「世界」前面，明显不对。
/// 3. **部分覆盖的小奖励** —— 没有完整匹配时（如 `nihaoshijie` 查不到
///    「你好世界」），按覆盖比例排序，让「你好」优先于「你」。
///
/// **注意**：不能用 `matched/total` 这种比值来区分完整与否 ——
/// 分母 `total` 是**该切分自己**的音节数，`[xian]` 和 `[xi,an]` 各自算出来
/// 都是 1.0，区分不了。必须用「是否等于本次输入的音节数」来判断。
#[inline]
fn score(weight: u32, matched: usize, total: usize) -> f32 {
    let freq = (weight.max(1) as f32).ln();
    let coverage = if total == 0 {
        0.0
    } else {
        matched as f32 / total as f32
    };
    let complete = matched == total && total > 0;
    let complete_bonus = if complete { COMPLETE_BONUS } else { 0.0 };
    let partial_bonus = PARTIAL_BONUS * coverage;
    freq + complete_bonus + partial_bonus
}

/// 完整覆盖输入时的奖励。要大到让**任何**完整匹配都胜过部分匹配。
const COMPLETE_BONUS: f32 = 15.0;
/// 部分覆盖时的比例奖励
const PARTIAL_BONUS: f32 = 8.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_prefers_longer_match() {
        let total = 2;
        assert!(
            score(1000, 2, total) > score(1000, 1, total),
            "同词频下整词匹配应优于单字"
        );
    }

    #[test]
    fn score_prefers_higher_weight() {
        assert!(score(10000, 2, 2) > score(100, 2, 2));
    }

    /// 这是实测发现的真问题：`shijie` 曾把「是」排在「世界」前面。
    /// 单字词频可以比词高两个数量级，所以完整覆盖的奖励必须压过词频差。
    #[test]
    fn complete_match_beats_high_freq_partial() {
        // 模拟真实量级：「是」权重 ~3.3e7，「世界」~4.8e5，输入 2 音节
        let shi = score(33_000_000, 1, 2); // 部分覆盖
        let shijie = score(480_000, 2, 2); // 完整覆盖
        assert!(
            shijie > shi,
            "完整覆盖的「世界」({shijie:.1}) 应胜过部分覆盖的「是」({shi:.1})"
        );
    }

    #[test]
    fn among_complete_matches_frequency_decides() {
        // 同为完整覆盖时，词频高的排前
        assert!(score(100_000, 2, 2) > score(1_000, 2, 2));
    }

    #[test]
    fn partial_ranks_by_coverage() {
        // 都没有完整匹配时，覆盖多的排前（nihaoshijie 的场景）
        let two_of_four = score(1000, 2, 4);
        let one_of_four = score(1000, 1, 4);
        assert!(two_of_four > one_of_four);
    }

    #[test]
    fn single_syllable_input_is_complete() {
        // 输入只有一个音节时，单字本身就是完整覆盖，不该被惩罚
        assert_eq!(score(100, 1, 1), (100f32).ln() + COMPLETE_BONUS + PARTIAL_BONUS);
    }
}
