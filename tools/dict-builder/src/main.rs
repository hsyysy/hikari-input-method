//! RIME 词库 → 二进制 blob 的 CLI 外壳。
//!
//! 建表逻辑在 `pinyin-engine::build` 里（这样测试能用内存小词典跑往返验证）；
//! 本程序只负责解析 RIME 的 `.dict.yaml` 并落盘。
//!
//! # 输入格式
//!
//! RIME `.dict.yaml`，数据段（`...` 之后）每行 `汉字<TAB>拼音<TAB>权重`：
//!
//! ```text
//! 你好	ni hao	9820
//! ```
//!
//! 拼音是**空格分隔的无调音节**（`[a-z]`，`v` 代表 ü）。`#` 开头为注释。
//!
//! # 用法
//!
//! ```sh
//! dict-builder out.bin 8105.dict.yaml base.dict.yaml
//! ```

use std::io::{BufRead, BufReader};

use pinyin_engine::DictBuilder;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("用法: dict-builder <输出.bin> <输入1.dict.yaml> [输入2.dict.yaml ...]");
        std::process::exit(1);
    }
    match run(&args[1], &args[2..]) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("✖ {e}");
            std::process::exit(1);
        }
    }
}

fn run(out_path: &str, inputs: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut b = DictBuilder::new();
    let (mut lines, mut malformed) = (0u64, 0u64);

    for path in inputs {
        let f = std::fs::File::open(path).map_err(|e| format!("打开 {path} 失败: {e}"))?;
        let reader = BufReader::with_capacity(1 << 20, f);
        let mut in_data = false;
        for line in reader.lines() {
            let line = line?;
            if !in_data {
                if line.trim_end() == "..." {
                    in_data = true;
                }
                continue;
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split('\t');
            let (Some(word), Some(py), Some(w)) = (it.next(), it.next(), it.next()) else {
                malformed += 1;
                continue;
            };
            let Ok(weight) = w.trim().parse::<u32>() else {
                malformed += 1;
                continue;
            };
            if b.add(word, py, weight)? {
                lines += 1;
            } else {
                malformed += 1;
            }
        }
    }

    let (bytes, stats) = b.build()?;
    std::fs::write(out_path, &bytes)?;

    println!("✅ 写出 {out_path}");
    println!("   读入 {lines} 行，跳过格式异常 {malformed} 行");
    println!("   音节 {} 个", stats.syllables);
    println!("   条目 {} 条（去重 {} 条）", stats.entries, stats.duplicates);
    println!("   trie 节点 {} 个", stats.nodes);
    println!("   字符串池 {} 字节", stats.string_bytes);
    println!(
        "   文件 {} 字节（{:.1} MB）",
        bytes.len(),
        bytes.len() as f64 / 1e6
    );
    Ok(())
}
