//! 候选窗内容 —— 拼音 + 候选词列表。
//!
//! 布局（逻辑像素）：
//!
//! ```text
//! ┌─────────────────────────────────┐
//! │ ni'hao                          │  ← 拼音（小字、灰）
//! │ 1.你好  2.拟好  3.你  4.尼       │  ← 候选（选中项高亮）
//! └─────────────────────────────────┘
//! ```

use crate::canvas::Canvas;
use crate::shm::{rgb, rgba_premul, Pixel};
use crate::text::TextRenderer;

// ── 布局常量（逻辑像素）──
const PAD: u32 = 8;
const PREEDIT_SIZE: f32 = 13.0;
const CAND_SIZE: f32 = 18.0;
const GAP: u32 = 3;
const ITEM_GAP: u32 = 14;
/// 候选序号与词之间的间距
const NUM_GAP: u32 = 2;

const BG: Pixel = rgba_premul(26, 28, 34, 242);
const BORDER: Pixel = rgb(110, 170, 245);
const PREEDIT_FG: Pixel = rgb(150, 158, 172);
const CAND_FG: Pixel = rgb(228, 230, 238);
const SEL_BG: Pixel = rgba_premul(70, 120, 200, 255);
const SEL_FG: Pixel = rgb(255, 255, 255);

/// 一屏候选
pub struct CandidateView<'a> {
    /// 拼音缓冲（原样显示，便于用户确认输入）
    pub preedit: &'a str,
    /// 候选词
    pub candidates: &'a [String],
    /// 当前选中项
    pub selected: usize,
}

/// 把拼音按音节切开显示（`nihao` → `ni'hao`），便于阅读。
///
/// `boundaries` 是每个音节的**字节起始位置**（不含 0）。
pub fn format_preedit(buffer: &str, boundaries: &[usize]) -> String {
    if boundaries.is_empty() {
        return buffer.to_string();
    }
    let mut out = String::with_capacity(buffer.len() + boundaries.len());
    let mut prev = 0;
    for &b in boundaries {
        if b > prev && b < buffer.len() {
            out.push_str(&buffer[prev..b]);
            out.push('\'');
        }
        prev = b;
    }
    out.push_str(&buffer[prev..]);
    out
}

/// 计算候选窗所需的**逻辑尺寸**（先算尺寸再分配缓冲，避免抖动）
pub fn measure(view: &CandidateView<'_>, text: &mut TextRenderer) -> (u32, u32) {
    let s = crate::SUPERSAMPLE as f32;

    let preedit_w = if view.preedit.is_empty() {
        0.0
    } else {
        text.measure(view.preedit, PREEDIT_SIZE * s) / s
    };

    let mut cand_w = 0.0f32;
    for (i, c) in view.candidates.iter().enumerate() {
        let label = format!("{}.{}", i + 1, c);
        cand_w += text.measure(&label, CAND_SIZE * s) / s;
        if i + 1 < view.candidates.len() {
            cand_w += ITEM_GAP as f32;
        }
    }

    let w = PAD as f32 + preedit_w.max(cand_w) + PAD as f32;
    let preedit_h = PREEDIT_SIZE * 1.2;
    let cand_h = CAND_SIZE * 1.2;
    let h = PAD as f32 + preedit_h + GAP as f32 + cand_h + PAD as f32;

    (
        (w.ceil() as u32).max(120),
        (h.ceil() as u32).max(40),
    )
}

/// 画一屏候选。`canvas` 的 scale 即超采样倍率。
///
/// ⚠️ 这里传给 `Canvas` 的字号与坐标**一律是逻辑单位** ——
/// `Canvas` 内部会乘 `scale`。曾经在这里又乘了一次 `SUPERSAMPLE`，
/// 导致字号被平方（13→52 而非 26）、文字过大且 advance 只按一半算，画面重叠。
pub fn draw(canvas: &mut Canvas<'_>, text: &mut TextRenderer, view: &CandidateView<'_>) {
    let (w, h) = canvas.logical_size();

    // 背景 + 边框
    canvas.fill_rect(0, 0, w, h, BG);
    canvas.stroke_rect(0, 0, w, h, BORDER, 1);

    let mut y = PAD as f32;

    // ── 拼音行 ──
    if !view.preedit.is_empty() {
        canvas.draw_text(text, PAD as f32, y, view.preedit, PREEDIT_SIZE, PREEDIT_FG);
    }
    y += PREEDIT_SIZE * 1.2 + GAP as f32;

    // ── 候选行 ──
    let mut x = PAD as f32;
    for (i, c) in view.candidates.iter().enumerate() {
        let label = format!("{}.{}", i + 1, c);
        let lw = canvas.measure_text(text, &label, CAND_SIZE);
        let selected = i == view.selected;

        if selected {
            // 选中项：整块高亮
            canvas.fill_rect(
                (x - NUM_GAP as f32) as i32,
                (y - 1.0) as i32,
                (lw + NUM_GAP as f32 * 2.0).ceil() as u32,
                (CAND_SIZE * 1.2 + 2.0).ceil() as u32,
                SEL_BG,
            );
        }
        canvas.draw_text(
            text,
            x,
            y,
            &label,
            CAND_SIZE,
            if selected { SEL_FG } else { CAND_FG },
        );
        x += lw + ITEM_GAP as f32;
    }
}
