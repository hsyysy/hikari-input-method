//! 中文文本渲染 —— 候选窗要显示汉字，位图字体不够用。
//!
//! 用 `cosmic-text`（内含 fontdb 系统字体加载 + swash 光栅化 + 字形回退）。
//!
//! # 与超采样的配合
//!
//! 候选窗缓冲按 [`crate::SUPERSAMPLE`] 倍渲染，
//! 所以这里**按缓冲像素给字号**：逻辑 16px 的字，传 32.0。
//! 光栅化在 2x 分辨率下做，降采样后边缘更干净 —— 这正是超采样的收益所在
//! （M2 用实心方块字体时看不出来，换成矢量字体才显现）。

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};

use crate::shm::Pixel;

/// 带缓存的文本渲染器。**进程内建一次**（`FontSystem::new()` 会扫系统字体，较慢）。
pub struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    /// 实际选中的字体族（供诊断输出）
    family: String,
}

impl Default for TextRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl TextRenderer {
    /// 建渲染器并选一个覆盖中文的字体族。
    pub fn new() -> Self {
        let font_system = FontSystem::new();
        let family = pick_cjk_family(&font_system);
        Self {
            font_system,
            swash_cache: SwashCache::new(),
            family,
        }
    }

    pub fn family(&self) -> &str {
        &self.family
    }

    /// 量一段文字在给定字号下的宽度（**缓冲像素**），不绘制。
    pub fn measure(&mut self, text: &str, size_px: f32) -> f32 {
        let mut buf = self.make_buffer(text, size_px, f32::INFINITY);
        buf.shape_until_scroll(&mut self.font_system, false);
        buf.layout_runs()
            .map(|r| r.line_w)
            .fold(0.0f32, f32::max)
    }

    /// 在**缓冲坐标** `(x, y)` 处画一行文字。
    ///
    /// `y` 是这一行文字**顶部**的位置。`size_px` 是缓冲像素字号。
    /// 返回实际绘制的宽度。
    pub fn draw(
        &mut self,
        pixels: &mut [Pixel],
        buf_width: u32,
        buf_height: u32,
        x: i32,
        y: i32,
        text: &str,
        size_px: f32,
        color: Pixel,
    ) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let mut buf = self.make_buffer(text, size_px, f32::INFINITY);
        buf.shape_until_scroll(&mut self.font_system, false);

        let width = buf
            .layout_runs()
            .map(|r| r.line_w)
            .fold(0.0f32, f32::max);

        // cosmic-text 的 Color 是 0xAARRGGBB，与我们的 Pixel 表示一致
        let c = cosmic_text::Color(color);

        buf.draw(
            &mut self.font_system,
            &mut self.swash_cache,
            c,
            |gx, gy, gw, gh, gc| {
                let a = ((gc.0 >> 24) & 0xFF) as u32;
                if a == 0 {
                    return;
                }
                let (r, g, b) = (
                    ((gc.0 >> 16) & 0xFF) as u32,
                    ((gc.0 >> 8) & 0xFF) as u32,
                    (gc.0 & 0xFF) as u32,
                );
                for row in 0..gh as i32 {
                    for col in 0..gw as i32 {
                        let px = x + gx + col;
                        let py = y + gy + row;
                        if px < 0 || py < 0 || px >= buf_width as i32 || py >= buf_height as i32 {
                            continue;
                        }
                        let idx = (py as u32 * buf_width + px as u32) as usize;
                        if idx >= pixels.len() {
                            continue;
                        }
                        // 字形像素与背景做 alpha 混合。
                        // 注意 wl_shm 是**预乘 alpha**，
                        // 所以混合时背景按 (1-a) 缩放、前景直接加上去。
                        let bg = pixels[idx];
                        let inv = 255 - a;
                        let blend = |shift: u32| -> u32 {
                            let fg = (gc.0 >> shift) & 0xFF;
                            let bgv = (bg >> shift) & 0xFF;
                            (fg * a / 255) + (bgv * inv / 255)
                        };
                        let na = (a + ((bg >> 24) & 0xFF) * inv / 255).min(255);
                        pixels[idx] = (na << 24) | (blend(16) << 16) | (blend(8) << 8) | blend(0);
                    }
                }
                let _ = (r, g, b);
            },
        );
        width
    }

    fn make_buffer(&mut self, text: &str, size_px: f32, width: f32) -> Buffer {
        let metrics = Metrics::new(size_px, size_px * 1.2);
        let mut buf = Buffer::new(&mut self.font_system, metrics);
        buf.set_size(Some(width), None);
        // 先取出族名再借，否则临时值会在语句结束被释放
        let family = self.family.clone();
        let attrs = Attrs::new().family(Family::Name(&family));
        buf.set_text(text, &attrs, Shaping::Advanced, None);
        buf
    }
}

/// 从系统字体里挑一个覆盖中文的族。
///
/// 优先常见的中文字体；都没有时退回默认（cosmic-text 有字形回退，
/// 即便族名不对也可能靠 fallback 渲染出汉字）。
fn pick_cjk_family(font_system: &FontSystem) -> String {
    const CANDIDATES: &[&str] = &[
        "Noto Sans CJK SC",
        "Noto Sans SC",
        "Source Han Sans SC",
        "WenQuanYi Micro Hei",
        "DengXian",
        "SimSun",
        "Microsoft YaHei",
        "Noto Sans",
    ];
    let db = font_system.db();
    for name in CANDIDATES {
        let found = db.faces().any(|f| {
            f.families
                .iter()
                .any(|(fam, _)| fam.eq_ignore_ascii_case(name))
        });
        if found {
            return (*name).to_string();
        }
    }
    // 没找到就用系统默认
    "sans-serif".to_string()
}
