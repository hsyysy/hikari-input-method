//! 词典读取与查表。
//!
//! blob 经 `mmap` 加载，**零解析**直接按偏移访问。

use std::path::Path;

use memmap2::Mmap;

use crate::format::{encode_syllable, Entry, Header, Node, HEADER_SIZE, MAGIC, VERSION};

/// 一个候选词
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// 词条文本
    pub word: String,
    /// 原始词频
    pub weight: u32,
    /// 命中了几个音节
    pub matched: usize,
    /// 命中的音节**一共占掉多少个输入字母**
    ///
    /// 分步选词要按它把已选中的前缀从缓冲里切出去：`zheli` 的「这」是 3，
    /// 「这里」是 5。由 `Engine::collect` 填 —— [`Dict::walk`] 只认音节 id，
    /// 不知道每个音节有几个字母。
    pub consumed: usize,
    /// 排序分（越大越靠前）
    pub score: f32,
}

/// 打开词典时的错误
#[derive(Debug)]
pub enum DictError {
    Io(std::io::Error),
    BadMagic,
    BadVersion(u32),
    Truncated,
}

impl std::fmt::Display for DictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DictError::Io(e) => write!(f, "读取词典失败: {e}"),
            DictError::BadMagic => write!(f, "词典文件魔数不对（不是 dict-builder 的产物？）"),
            DictError::BadVersion(v) => write!(f, "词典版本 {v} 不受支持（本程序支持 {VERSION}）"),
            DictError::Truncated => write!(f, "词典文件被截断"),
        }
    }
}

impl std::error::Error for DictError {}

impl From<std::io::Error> for DictError {
    fn from(e: std::io::Error) -> Self {
        DictError::Io(e)
    }
}

/// mmap 加载的词典
pub struct Dict {
    map: Mmap,
    header: Header,
}

// 只读访问 mmap，跨线程安全
unsafe impl Send for Dict {}
unsafe impl Sync for Dict {}

impl Dict {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DictError> {
        let file = std::fs::File::open(path)?;
        // SAFETY: 只读映射；文件在映射期间不应被外部修改（词典是只读资源）
        let map = unsafe { Mmap::map(&file)? };
        Self::from_mmap(map)
    }

    fn from_mmap(map: Mmap) -> Result<Self, DictError> {
        if map.len() < HEADER_SIZE {
            return Err(DictError::Truncated);
        }
        let header = read_header(&map)?;
        if &header.magic != MAGIC {
            return Err(DictError::BadMagic);
        }
        if header.version != VERSION {
            return Err(DictError::BadVersion(header.version));
        }
        Ok(Self { map, header })
    }

    /// 供测试用：直接从字节构造（走匿名映射，复用同一套解析路径）
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DictError> {
        let mut m = memmap2::MmapMut::map_anon(bytes.len())?;
        m.copy_from_slice(bytes);
        Self::from_mmap(m.make_read_only()?)
    }

    pub fn n_syllables(&self) -> usize {
        self.header.n_syllables as usize
    }
    pub fn n_nodes(&self) -> usize {
        self.header.n_nodes as usize
    }
    pub fn n_entries(&self) -> usize {
        self.header.n_entries as usize
    }

    /// 全部音节字符串（顺序 = 音节 id 顺序）
    pub fn syllables(&self) -> Vec<&str> {
        let mut out = Vec::with_capacity(self.n_syllables());
        let base = self.header.syllables_off as usize;
        let mut p = base;
        for _ in 0..self.n_syllables() {
            let len = self.map[p] as usize;
            p += 1;
            out.push(std::str::from_utf8(&self.map[p..p + len]).unwrap_or(""));
            p += len;
        }
        out
    }

    #[inline]
    fn node(&self, idx: u32) -> Option<Node> {
        let off = self.header.nodes_off as usize + idx as usize * std::mem::size_of::<Node>();
        let bytes = self.map.get(off..off + std::mem::size_of::<Node>())?;
        Some(Node {
            syllable_id: u16::from_le_bytes([bytes[0], bytes[1]]),
            child_count: u16::from_le_bytes([bytes[2], bytes[3]]),
            child_start: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            entry_start: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            entry_count: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        })
    }

    #[inline]
    fn entry(&self, idx: u32) -> Option<Entry> {
        let off = self.header.entries_off as usize + idx as usize * std::mem::size_of::<Entry>();
        let b = self.map.get(off..off + std::mem::size_of::<Entry>())?;
        Some(Entry {
            str_off: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            weight: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            str_len: u16::from_le_bytes([b[8], b[9]]),
            _pad: 0,
        })
    }

    #[inline]
    fn string(&self, off: u32, len: u16) -> &str {
        let s = self.header.strings_off as usize + off as usize;
        std::str::from_utf8(self.map.get(s..s + len as usize).unwrap_or(b"")).unwrap_or("")
    }

    /// 在节点的子节点里二分找指定音节
    #[inline]
    fn child(&self, node: &Node, syllable_id: u16) -> Option<u32> {
        let (mut lo, mut hi) = (
            node.child_start as usize,
            node.child_start as usize + node.child_count as usize,
        );
        while lo < hi {
            let mid = (lo + hi) / 2;
            let n = self.node(mid as u32)?;
            match n.syllable_id.cmp(&syllable_id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid as u32),
            }
        }
        None
    }

    /// 沿音节序列走 trie，收集沿途**所有前缀命中**的条目。
    ///
    /// `max_per_node` 限制每个节点取多少条（条目已按 weight 降序，取前 N 即可）。
    pub fn walk(&self, syllables: &[u16], max_per_node: usize, out: &mut Vec<Candidate>) {
        let mut cur = 0u32; // 根节点
        for (depth, &sid) in syllables.iter().enumerate() {
            let Some(node) = self.node(cur) else { return };
            let Some(next) = self.child(&node, sid) else {
                return; // 走不下去了
            };
            cur = next;
            let Some(node) = self.node(cur) else { return };

            let n = (node.entry_count as usize).min(max_per_node);
            for k in 0..n {
                let Some(e) = self.entry(node.entry_start + k as u32) else {
                    break;
                };
                let word = self.string(e.str_off, e.str_len);
                if word.is_empty() {
                    continue;
                }
                out.push(Candidate {
                    word: word.to_string(),
                    weight: e.weight,
                    matched: depth + 1,
                    consumed: 0, // 由 Engine::collect 填（这里不知道音节字母数）
                    score: 0.0,  // 由 rank 填
                });
            }
        }
    }

    /// 音节字符串 → id（线性扫，仅用于建表后自检；热路径不用）
    pub fn syllable_id(&self, s: &str) -> Option<u16> {
        let want = encode_syllable(s)?;
        self.syllables()
            .iter()
            .position(|x| encode_syllable(x) == Some(want))
            .map(|i| i as u16)
    }
}

fn read_header(map: &[u8]) -> Result<Header, DictError> {
    let b = map.get(..HEADER_SIZE).ok_or(DictError::Truncated)?;
    let mut magic = [0u8; 8];
    magic.copy_from_slice(&b[0..8]);
    Ok(Header {
        magic,
        version: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
        n_syllables: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
        n_nodes: u32::from_le_bytes([b[16], b[17], b[18], b[19]]),
        n_entries: u32::from_le_bytes([b[20], b[21], b[22], b[23]]),
        syllables_off: u32::from_le_bytes([b[24], b[25], b[26], b[27]]),
        nodes_off: u32::from_le_bytes([b[28], b[29], b[30], b[31]]),
        entries_off: u32::from_le_bytes([b[32], b[33], b[34], b[35]]),
        strings_off: u32::from_le_bytes([b[36], b[37], b[38], b[39]]),
    })
}
