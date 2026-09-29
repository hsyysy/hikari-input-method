//! 词典 blob 的二进制布局 —— **builder 与 engine 共用**，改这里两边一起改。
//!
//! 设计目标：**mmap 后零解析直接访问**。所有多字节整数小端。
//!
//! ```text
//! ┌──────────────┐ 0
//! │ Header (32B) │
//! ├──────────────┤ syllables_off
//! │ 音节表        │  每条: u8 长度 + 字节（ASCII）
//! ├──────────────┤ nodes_off
//! │ 节点数组      │  每节点 16B，见 Node
//! ├──────────────┤ entries_off
//! │ 条目数组      │  每条约 12B，见 Entry
//! ├──────────────┤ strings_off
//! │ 字符串池      │  词条的 UTF-8 字节，靠 (offset,len) 引用
//! └──────────────┘
//! ```

/// 文件头魔数
pub const MAGIC: &[u8; 8] = b"IMEPINY\x01";
/// 当前格式版本
pub const VERSION: u32 = 1;

// 8(magic) + 4(version) + 4+4+4(三个计数) + 4*4(四个段偏移) = 40
pub const HEADER_SIZE: usize = 40;

/// 文件头
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub magic: [u8; 8],
    pub version: u32,
    pub n_syllables: u32,
    pub n_nodes: u32,
    pub n_entries: u32,
    /// 各段起始偏移
    pub syllables_off: u32,
    pub nodes_off: u32,
    pub entries_off: u32,
    pub strings_off: u32,
}

/// trie 节点，16 字节。
///
/// 子节点在数组中是**连续且按 `syllable_id` 升序**的，
/// 所以查子节点用二分（`child_start .. child_start+child_count`）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Node {
    /// 本节点对应的音节 id（根节点无意义）
    pub syllable_id: u16,
    /// 子节点个数
    pub child_count: u16,
    /// 子节点在节点数组中的起始下标
    pub child_start: u32,
    /// 本节点命中的条目在条目数组中的起始下标
    pub entry_start: u32,
    /// 本节点命中的条目个数
    pub entry_count: u32,
}

/// 词条，12 字节。
///
/// **同一节点内的条目按 weight 降序排列**，所以取 top-N 只需读前 N 条。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    /// 在字符串池中的字节偏移
    pub str_off: u32,
    /// 词频（原始权重，≥1）
    pub weight: u32,
    /// 词条 UTF-8 字节数
    pub str_len: u16,
    pub _pad: u16,
}

/// 每个音节最多 6 个字母（"zhuang"、"shuang"…），只有 `[a-z]`，
/// 故可把整个音节编进一个整数：每字母 5 bit + 3 bit 长度。
pub const MAX_SYLLABLE_LEN: usize = 6;

/// 音节键。用 u64 而非 u32：6 字母 × 5 bit = 30 bit，再加 3 bit 长度 = 33 bit，
/// **放不进 u32**（这点写错过一次）。
pub type SyllableKey = u64;

/// 把 `[a-z]` 音节编码成键；非法字符返回 `None`。
#[inline]
pub fn encode_syllable(s: &str) -> Option<SyllableKey> {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > MAX_SYLLABLE_LEN {
        return None;
    }
    let mut key = 0u64;
    for &c in b {
        if !c.is_ascii_lowercase() {
            return None;
        }
        key = (key << 5) | (c - b'a' + 1) as u64;
    }
    // 长度单独占 3 bit（1..=6 → 0..=5），避免 "a" 与其它前缀混淆
    Some((key << 3) | (b.len() as u64 - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_is_injective() {
        // 不同音节必须得到不同键
        let mut seen = std::collections::HashMap::new();
        for s in [
            "a", "ai", "an", "ang", "ao", "ba", "zhuang", "chuan", "nv", "nve", "shuang",
        ] {
            let k = encode_syllable(s).expect(s);
            if let Some(prev) = seen.insert(k, s) {
                panic!("碰撞: {s} 与 {prev} 编出同一个键");
            }
        }
    }

    #[test]
    fn encode_rejects_bad() {
        assert!(encode_syllable("").is_none());
        assert!(encode_syllable("ABC").is_none());
        assert!(encode_syllable("ni3").is_none());
        assert!(encode_syllable("toolong").is_none());
        assert!(encode_syllable("nü").is_none());
    }

    #[test]
    fn longest_syllable_fits() {
        // 6 字母是最长情形，键必须放得进 u64 且不溢出
        let k = encode_syllable("shuang").unwrap();
        assert!(k < u64::MAX / 8);
    }
}
