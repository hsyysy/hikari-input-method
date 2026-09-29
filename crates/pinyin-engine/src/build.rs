//! 词典构建 —— 从条目生成 [`crate::format`] 定义的 blob。
//!
//! 放在 engine crate 里而不是 CLI 里，是为了让**测试能用内存中的小词典**
//! 验证完整往返，而不必依赖 25 MB 的真实数据文件。
//!
//! `tools/dict-builder` 只是这个模块的 CLI 外壳。

use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::format::{encode_syllable, Entry, Node, HEADER_SIZE, MAGIC, VERSION};

/// 构建期的错误
#[derive(Debug)]
pub enum BuildError {
    /// 拼音里出现非法音节（含非 `[a-z]` 或过长）
    BadSyllable(String),
    /// 音节数超过 u16 上限
    TooManySyllables(usize),
    /// 空词条
    EmptyWord,
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::BadSyllable(s) => write!(f, "非法音节: {s:?}（只允许 [a-z]，最长 6 字母）"),
            BuildError::TooManySyllables(n) => write!(f, "音节数 {n} 超过 u16 上限"),
            BuildError::EmptyWord => write!(f, "词条为空"),
        }
    }
}

impl std::error::Error for BuildError {}

/// 构建统计
#[derive(Default, Debug)]
pub struct BuildStats {
    pub syllables: usize,
    pub entries: usize,
    pub nodes: usize,
    pub string_bytes: usize,
    /// 因重复（同词同拼音）被合并的条数
    pub duplicates: usize,
}

/// 条目收集器 → 二进制 blob
#[derive(Default)]
pub struct DictBuilder {
    /// (词, 音节序列, 权重)
    items: Vec<(String, Vec<String>, u32)>,
    pub duplicates: usize,
}

impl DictBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 加一条。拼音是**空格分隔的无调音节**（RIME 格式）。
    ///
    /// 非法音节会被跳过（返回 `Ok(false)`），而不是让整次构建失败 ——
    /// 真实词库里总有零星脏数据。
    pub fn add(&mut self, word: &str, pinyin: &str, weight: u32) -> Result<bool, BuildError> {
        if word.is_empty() {
            return Err(BuildError::EmptyWord);
        }
        let mut syls = Vec::new();
        for s in pinyin.split_whitespace() {
            if encode_syllable(s).is_none() {
                return Ok(false); // 脏数据，跳过
            }
            syls.push(s.to_string());
        }
        if syls.is_empty() {
            return Ok(false);
        }
        self.items.push((word.to_string(), syls, weight.max(1)));
        Ok(true)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 生成 blob
    pub fn build(self) -> Result<(Vec<u8>, BuildStats), BuildError> {
        let mut stats = BuildStats::default();

        // ── 音节表：按字符串排序 → id 稳定可复现 ──
        let mut set: BTreeMap<String, ()> = BTreeMap::new();
        for (_, syls, _) in &self.items {
            for s in syls {
                set.insert(s.clone(), ());
            }
        }
        let syllables: Vec<String> = set.into_keys().collect();
        if syllables.len() > u16::MAX as usize {
            return Err(BuildError::TooManySyllables(syllables.len()));
        }
        let sid: HashMap<&str, u16> = syllables
            .iter()
            .enumerate()
            .map(|(i, s)| (s.as_str(), i as u16))
            .collect();
        stats.syllables = syllables.len();

        // ── 去重：同词同拼音保留最大权重 ──
        let mut dedup: HashMap<(String, Vec<u16>), u32> = HashMap::new();
        for (word, syls, weight) in self.items {
            let ids: Vec<u16> = syls.iter().map(|s| sid[s.as_str()]).collect();
            let key = (word, ids);
            match dedup.get_mut(&key) {
                Some(w) => {
                    *w = (*w).max(weight);
                    stats.duplicates += 1;
                }
                None => {
                    dedup.insert(key, weight);
                }
            }
        }

        // 排序保证输出可复现：音节序列 → 权重降序 → 词
        let mut raws: Vec<(String, Vec<u16>, u32)> = dedup
            .into_iter()
            .map(|((w, s), wt)| (w, s, wt))
            .collect();
        raws.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then(b.2.cmp(&a.2))
                .then(a.0.cmp(&b.0))
        });
        stats.entries = raws.len();

        // ── 建 trie ──
        #[derive(Default)]
        struct BNode {
            children: BTreeMap<u16, usize>,
            entries: Vec<usize>,
        }
        let mut nodes: Vec<BNode> = vec![BNode::default()];
        for (i, (_, syls, _)) in raws.iter().enumerate() {
            let mut cur = 0usize;
            for &s in syls {
                let next = match nodes[cur].children.get(&s) {
                    Some(&n) => n,
                    None => {
                        nodes.push(BNode::default());
                        let n = nodes.len() - 1;
                        nodes[cur].children.insert(s, n);
                        n
                    }
                };
                cur = next;
            }
            nodes[cur].entries.push(i);
        }
        stats.nodes = nodes.len();

        // ── 重排下标，保证「同一父节点的子节点连续」──
        //
        // ⚠️ 普通 DFS 不满足这一点（子节点 A 的整棵子树会插在 A 与 B 之间），
        // 查表用的是 `child_start .. +child_count` 的二分，必须连续。
        // 用 BFS：处理一个节点时立刻给它的全部子节点分配连续下标。
        let mut node_index = vec![0u32; nodes.len()];
        let mut order: Vec<usize> = Vec::with_capacity(nodes.len());
        {
            let mut queue = VecDeque::new();
            order.push(0);
            queue.push_back(0usize);
            let mut next_free = 1u32;
            while let Some(n) = queue.pop_front() {
                let kids: Vec<usize> = nodes[n].children.values().copied().collect();
                if kids.is_empty() {
                    continue;
                }
                let start = next_free;
                for (i, &k) in kids.iter().enumerate() {
                    node_index[k] = start + i as u32;
                    order.push(k);
                    queue.push_back(k);
                }
                next_free += kids.len() as u32;
            }
        }
        debug_assert_eq!(order.len(), nodes.len(), "BFS 应覆盖全部节点");

        let mut flat_nodes = vec![
            Node {
                syllable_id: 0,
                child_count: 0,
                child_start: 0,
                entry_start: 0,
                entry_count: 0,
            };
            nodes.len()
        ];
        let mut flat_entries: Vec<Entry> = Vec::with_capacity(raws.len());
        let mut strings: Vec<u8> = Vec::new();
        let mut str_off_of: HashMap<&str, (u32, u16)> = HashMap::new();

        for &old in &order {
            let new_idx = node_index[old] as usize;
            let bn = &nodes[old];

            // 条目：组内按权重降序，walker 取前 N 条即 top-N
            let mut idxs = bn.entries.clone();
            idxs.sort_by(|&a, &b| raws[b].2.cmp(&raws[a].2).then(raws[a].0.cmp(&raws[b].0)));

            let entry_start = flat_entries.len() as u32;
            for &ri in &idxs {
                let (word, _, weight) = &raws[ri];
                let (off, len) = *str_off_of.entry(word.as_str()).or_insert_with(|| {
                    let off = strings.len() as u32;
                    strings.extend_from_slice(word.as_bytes());
                    (off, word.len() as u16)
                });
                flat_entries.push(Entry {
                    str_off: off,
                    weight: *weight,
                    str_len: len,
                    _pad: 0,
                });
            }

            let kids: Vec<u16> = bn.children.keys().copied().collect();
            let child_start = if kids.is_empty() {
                0
            } else {
                // ⚠️ 注意：要取的是**第一个子节点的旧下标**，再用 node_index 映射。
                // 别写成 `node_index[kids[0] as usize]` —— kids[0] 是**音节 id**，
                // 不是节点下标，那样会查到完全无关的节点。（此处曾写错一次，
                // 被内存小词典的往返测试抓到。）
                let first_child_old = *bn.children.get(&kids[0]).expect("刚取自 keys");
                node_index[first_child_old] as u32
            };
            flat_nodes[new_idx] = Node {
                syllable_id: 0, // 由第二遍回填
                child_count: kids.len() as u16,
                child_start,
                entry_start,
                entry_count: idxs.len() as u32,
            };
        }

        // 第二遍：回填子节点的 syllable_id。
        //
        // ⚠️ 必须独立成一遍。若并进上面的循环，会出现「父节点回填了子节点的 id
        // → 轮到子节点自己被处理时又把自己写成 0」的覆盖问题，
        // 结果是**所有节点的 syllable_id 全为 0**，查表必然失败。
        for &old in &order {
            for (&k, &child_old) in &nodes[old].children {
                let ci = node_index[child_old] as usize;
                flat_nodes[ci].syllable_id = k;
            }
        }

        stats.string_bytes = strings.len();

        // ── 序列化 ──
        let syl_bytes: usize = syllables.iter().map(|s| 1 + s.len()).sum();
        let syllables_off = HEADER_SIZE;
        let nodes_off = syllables_off + syl_bytes;
        let entries_off = nodes_off + flat_nodes.len() * std::mem::size_of::<Node>();
        let strings_off = entries_off + flat_entries.len() * std::mem::size_of::<Entry>();

        let mut out = Vec::with_capacity(strings_off + strings.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(syllables.len() as u32).to_le_bytes());
        out.extend_from_slice(&(flat_nodes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(flat_entries.len() as u32).to_le_bytes());
        out.extend_from_slice(&(syllables_off as u32).to_le_bytes());
        out.extend_from_slice(&(nodes_off as u32).to_le_bytes());
        out.extend_from_slice(&(entries_off as u32).to_le_bytes());
        out.extend_from_slice(&(strings_off as u32).to_le_bytes());
        debug_assert_eq!(out.len(), HEADER_SIZE);

        for s in &syllables {
            out.push(s.len() as u8);
            out.extend_from_slice(s.as_bytes());
        }
        for n in &flat_nodes {
            out.extend_from_slice(&n.syllable_id.to_le_bytes());
            out.extend_from_slice(&n.child_count.to_le_bytes());
            out.extend_from_slice(&n.child_start.to_le_bytes());
            out.extend_from_slice(&n.entry_start.to_le_bytes());
            out.extend_from_slice(&n.entry_count.to_le_bytes());
        }
        for e in &flat_entries {
            out.extend_from_slice(&e.str_off.to_le_bytes());
            out.extend_from_slice(&e.weight.to_le_bytes());
            out.extend_from_slice(&e.str_len.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
        }
        out.extend_from_slice(&strings);

        debug_assert_eq!(out.len(), strings_off + strings.len());
        Ok((out, stats))
    }
}

/// 便捷入口：一次性从 `(词, 拼音, 权重)` 列表建 blob
pub fn build_dict<'a>(
    items: impl IntoIterator<Item = (&'a str, &'a str, u32)>,
) -> Result<(Vec<u8>, BuildStats), BuildError> {
    let mut b = DictBuilder::new();
    for (w, p, wt) in items {
        b.add(w, p, wt)?;
    }
    b.build()
}
