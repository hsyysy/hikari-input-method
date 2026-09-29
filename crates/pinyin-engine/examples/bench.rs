//! 性能实测。
//!
//! ```sh
//! cargo run --release --example bench -- data/dict/pinyin.bin
//! ```

use pinyin_engine::{Engine, DEFAULT_TOP_N};
use std::time::Instant;

/// 覆盖不同长度与歧义程度的典型输入
const CASES: &[&str] = &[
    // 单字母 —— 最坏情形：末尾前缀要展开成 30+ 个音节
    "s", "z", "n", "w", "x", "a",
    // 不完整的音节前缀
    "sh", "zh", "nih", "zhongg",
    // 单音节
    "wo", "ni", "de", "shi",
    // 双音节（含歧义）
    "nihao", "shijie", "beijing", "xian", "woshi", "zhongguo",
    // 三音节
    "zhongguoren", "wanshanghao", "jintiantianqi",
    // 四音节
    "nihaoshijie", "zhonghuarenmin", "woshizhongguoren",
    // 长串（触发多切分）
    "zhonghuarenmingongheguo",
    // 最坏情况：连续歧义音节
    "xianxianxianxian",
];

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "data/dict/pinyin.bin".to_string());

    let t0 = Instant::now();
    let engine = Engine::open(&path).expect("打开词典失败");
    let load = t0.elapsed();
    let d = engine.dict();

    println!("词典: {} 音节 / {} 节点 / {} 条目", d.n_syllables(), d.n_nodes(), d.n_entries());
    println!("文件: {} 字节 ({:.1} MB)", std::fs::metadata(&path).unwrap().len() as f64 as usize,
             std::fs::metadata(&path).unwrap().len() as f64 / 1e6);
    println!("加载耗时（mmap，仅建音节表）: {load:?}");
    println!();

    // 预热（让 mmap 页进来）
    for c in CASES {
        engine.candidates(c, DEFAULT_TOP_N);
    }

    let mut worst = 0u128;
    let mut total = 0u128;
    let mut n = 0u128;

    println!("{:<26} {:>10} {:>8}", "输入", "单次耗时", "候选数");
    println!("{}", "-".repeat(48));
    for c in CASES {
        const REPS: u32 = 200;
        let t = Instant::now();
        let mut cands = 0;
        for _ in 0..REPS {
            cands = engine.candidates(c, DEFAULT_TOP_N).len();
        }
        let per = t.elapsed().as_nanos() / REPS as u128;
        worst = worst.max(per);
        total += per;
        n += 1;
        println!("{:<26} {:>8} µs {:>8}", c, per as f64 / 1000.0, cands);
    }

    println!("{}", "-".repeat(48));
    println!("平均 {:.1} µs / 最差 {:.1} µs", total as f64 / n as f64 / 1000.0, worst as f64 / 1000.0);
    println!();
    println!("目标: < 2 ms（2000 µs）");
    if worst < 2_000_000 {
        println!("✅ 达标（最差 {:.1} µs，余量 {:.0}x）", worst as f64 / 1000.0, 2_000_000.0 / worst as f64);
    } else {
        println!("❌ 超限：最差 {:.1} µs", worst as f64 / 1000.0);
    }
}
