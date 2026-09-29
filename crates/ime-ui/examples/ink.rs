//! 离屏渲染诊断 —— 把文字画进内存缓冲，统计每个字的"墨量"。
//!
//! 用来回答「为什么某些字看起来更粗」这类问题，**不依赖合成器**，
//! 不会干扰正在使用的桌面。
//!
//! ```sh
//! cargo run --release -p ime-ui --example ink [字号]
//! ```

use ime_ui::{rgb, Canvas, TextRenderer, SUPERSAMPLE};

/// 待比较的字符：用户报告的「粗」组与「正常」组，外加几个参照
const GROUPS: &[(&str, &str)] = &[
    ("用户报告偏粗", "这里"),
    ("用户报告正常", "吃饭"),
    ("其它常用", "你好世界中国时间"),
    ("同字对比", "这这这吃吃吃"),
];

fn main() {
    let size: f32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(18.0);

    let mut text = TextRenderer::new();
    println!("字体族: {:?}", text.family());
    println!("逻辑字号: {size}  超采样: {SUPERSAMPLE}x");
    println!();

    let buf_w = 600u32 * SUPERSAMPLE;
    let buf_h = 60u32 * SUPERSAMPLE;
    let mut pixels = vec![0u32; (buf_w * buf_h) as usize];
    let bg = rgb(0, 0, 0);

    for (label, chars) in GROUPS {
        println!("── {label}: {chars} ──");
        for ch in chars.chars() {
            // 每个字单独画在一个干净的缓冲里，避免相邻字影响统计
            pixels.fill(bg);
            let s = ch.to_string();
            let w = {
                let mut c = Canvas::new(&mut pixels, buf_w, buf_h, SUPERSAMPLE);
                c.draw_text(&mut text, 2.0, 2.0, &s, size, rgb(255, 255, 255))
            };

            // 统计墨量：非黑像素数 + 平均亮度
            let mut ink = 0u32;
            let mut sum = 0u64;
            for &p in pixels.iter() {
                let v = p & 0xFF; // 蓝通道即可（灰度字）
                if v > 8 {
                    ink += 1;
                    sum += v as u64;
                }
            }
            let avg = if ink > 0 { sum as f64 / ink as f64 } else { 0.0 };
            let area = (w * size * 1.2 * SUPERSAMPLE as f32 * SUPERSAMPLE as f32) as f64;
            println!(
                "  {ch}  墨量={ink:5}  平均亮度={avg:6.1}  宽度={w:5.1}  墨量/面积={:.4}",
                if area > 0.0 { ink as f64 / area } else { 0.0 }
            );
        }
        println!();
    }

    println!("若同组内「墨量/面积」差异明显（比如超过 15%），说明是**字体或光栅化**的差异；");
    println!("若同一字重复出现时墨量一致，说明渲染是确定的、不是随机抖动。");
}
