//! 拼音转换引擎 —— **纯逻辑，零 wayland 依赖**。
//!
//! 这样设计是为了能脱离图形环境快速迭代：单元测试与 bench 都不需要合成器。
//!
//! # 数据流
//!
//! ```text
//! "nihao"
//!   ├─ segment.rs   DP 切分 → [[ni, hao]]        （多切分全部保留）
//!   ├─ dict.rs      trie 查表 → 你好(9.8) 你号(2.1) …
//!   ├─ 单字回退     [ni]→你 尼 泥…  [hao]→好 号 浩…
//!   └─ engine.rs    合并排序 → 前 N 个候选
//! ```
//!
//! # 词典来源
//!
//! `tools/dict-builder` 把 RIME 格式的词库编译成 [`format`] 定义的二进制 blob，
//! 运行时经 `mmap` **零解析**加载。

pub mod build;
pub mod dict;
pub mod engine;
pub mod format;
pub mod segment;

pub use build::{build_dict, BuildError, BuildStats, DictBuilder};
pub use dict::{Candidate, Dict, DictError};
pub use engine::Engine;
pub use segment::{Segmenter, MAX_INPUT_LEN};

/// 默认候选数（首屏）
pub const DEFAULT_TOP_N: usize = 9;
