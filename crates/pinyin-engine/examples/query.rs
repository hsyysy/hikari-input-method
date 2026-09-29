//! 词典查询 CLI —— 用来人工验证引擎输出。
//!
//! ```sh
//! cargo run --release --example query -- data/dict/pinyin.bin nihao xian
//! ```
//!
//! 不带输入时进入交互模式（从 stdin 逐行读）。

use pinyin_engine::{Engine, DEFAULT_TOP_N};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("用法: query <词典.bin> [拼音...]");
        std::process::exit(1);
    }

    let t0 = std::time::Instant::now();
    let engine = match Engine::open(&args[1]) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("✖ {e}");
            std::process::exit(1);
        }
    };
    let load = t0.elapsed();

    let d = engine.dict();
    println!(
        "词典: {} 音节 / {} 节点 / {} 条目，加载耗时 {:?}",
        d.n_syllables(),
        d.n_nodes(),
        d.n_entries(),
        load
    );
    println!();

    let inputs: Vec<String> = args[2..].to_vec();
    if !inputs.is_empty() {
        for q in &inputs {
            show(&engine, q);
        }
        return;
    }

    // 交互模式
    use std::io::BufRead;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let q = line.trim();
        if q.is_empty() {
            continue;
        }
        show(&engine, q);
    }
}

fn show(engine: &Engine, input: &str) {
    let t0 = std::time::Instant::now();
    let cands = engine.candidates(input, DEFAULT_TOP_N);
    let dt = t0.elapsed();

    print!("{input:16} → ");
    if cands.is_empty() {
        println!("(无候选)   [{dt:?}]");
        return;
    }
    let s: Vec<String> = cands
        .iter()
        .map(|c| {
            // 不满整个输入的候选会被**分步选词**定成前缀（而不是上屏），
            // 所以把它的 `consumed` 标出来：`这[12.3/3]` = 只吃掉 3 个字母。
            if c.consumed < input.len() {
                format!("{}[{:.1}/{}]", c.word, c.score, c.consumed)
            } else {
                format!("{}[{:.1}]", c.word, c.score)
            }
        })
        .collect();
    println!("{}   [{dt:?}]", s.join(" "));
}
