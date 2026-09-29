//! 候选窗渲染（M2 阶段：骨架 + 锐利度自检）
//!
//! 目前只有 `wl_shm` 软件光栅化的基础设施与一个**诊断用**的候选窗内容。
//! M4 会用真实候选词替换这里的绘制逻辑。
//!
//! 关键约定见 [`shm`] 模块文档——那里记着三个坑。

pub mod canvas;
pub mod candidates;
pub mod shm;
pub mod text;

pub use canvas::Canvas;
pub use shm::{rgb, rgba_premul, Pixel, ShmPool};
pub use candidates::CandidateView;
pub use text::TextRenderer;

/// M2 候选窗的逻辑尺寸（不含超采样放大）
pub const M2_WIDTH: u32 = 220;
pub const M2_HEIGHT: u32 = 64;

/// 超采样倍率：桌面跑在逻辑 scale 1，
/// 用 2x 渲染 + `set_buffer_scale(2)` 换取更锐利的文字。
pub const SUPERSAMPLE: u32 = 2;

/// M2 的诊断画面：外框 + 棋盘格 + 文字标签。
///
/// 之所以用棋盘格而不是纯色块：**1 逻辑像素的棋盘格是锐利度最灵敏的探针**——
/// 若超采样/重采样哪里错了，它会立刻糊成一片均匀的灰，
/// 而纯色块在任何缩放倍数下看起来都一样，测不出问题。
pub fn draw_m2_diagnostic(canvas: &mut Canvas<'_>, label: &str) {
    let (w, h) = canvas.logical_size();

    // 背景：半透明白（**走 rgba_premul**，见 §7.1.1 ② —— 若忘了预乘，
    // 下面圆角外那圈会发亮）
    let bg = rgba_premul(24, 24, 28, 235);
    canvas.fill_rect(0, 0, w, h, bg);

    // 1 逻辑像素边框：超采样是否生效一眼可见
    let border = rgb(120, 180, 255);
    canvas.stroke_rect(0, 0, w, h, border, 1);

    // 棋盘格区域（1 逻辑像素格）
    let (cx, cy, cw, ch) = (8, 8, 32, 32);
    canvas.checkerboard(cx, cy, cw, ch, rgb(255, 255, 255), rgb(0, 0, 0), 1);

    // 2 逻辑像素格的棋盘格作对照（更粗，抗糊）
    canvas.checkerboard(cx + cw as i32 + 8, cy, cw, ch, rgb(255, 200, 0), rgb(40, 0, 60), 2);

    // 文字标签：位图字体，1 逻辑像素/字形像素
    let fg = rgb(235, 235, 240);
    canvas.text(8, 46, label, fg, 1);

    // 右下角标注超采样倍率
    let note = format!("{}x", SUPERSAMPLE);
    let nx = w as i32 - (note.len() as i32) * 9 - 6;
    canvas.text(nx, 46, &note, rgb(150, 200, 255), 1);
}
