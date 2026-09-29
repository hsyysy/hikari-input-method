//! 简单软件光栅化 —— 往像素缓冲上画矩形 / 网格 / 位图文字。
//!
//! 所有几何 API 收的都是**逻辑坐标**；内部按 `scale` 放大到缓冲坐标。
//! 这样 2x 超采样只需要把 `Canvas::scale` 设为 2，
//! 调用方代码不用改。

use crate::shm::Pixel;

/// 8x8 位图字体（仅 ASCII 可见字符）
use font8x8::{UnicodeFonts, BASIC_FONTS};

pub struct Canvas<'a> {
    /// 缓冲宽度（物理像素）
    pub buf_width: u32,
    /// 缓冲高度（物理像素）
    pub buf_height: u32,
    /// 超采样倍率：逻辑坐标 × scale = 缓冲坐标
    pub scale: u32,
    pixels: &'a mut [Pixel],
}

impl<'a> Canvas<'a> {
    /// `pixels` 长度必须 = `buf_width * buf_height`
    pub fn new(pixels: &'a mut [Pixel], buf_width: u32, buf_height: u32, scale: u32) -> Self {
        assert_eq!(
            pixels.len(),
            (buf_width * buf_height) as usize,
            "像素缓冲长度与尺寸不符"
        );
        assert!(scale >= 1);
        Self {
            buf_width,
            buf_height,
            scale,
            pixels,
        }
    }

    /// 逻辑尺寸
    pub fn logical_size(&self) -> (u32, u32) {
        (self.buf_width / self.scale, self.buf_height / self.scale)
    }

    #[inline]
    fn put(&mut self, x: u32, y: u32, c: Pixel) {
        if x < self.buf_width && y < self.buf_height {
            self.pixels[(y * self.buf_width + x) as usize] = c;
        }
    }

    /// 逻辑坐标 → 缓冲坐标填一块矩形。
    ///
    /// 用**整数倍放大**填满，不做插值 —— 这正是超采样想要的：
    /// 每个逻辑像素对应 scale×scale 个缓冲像素。
    pub fn fill_rect(&mut self, x: i32, y: i32, w: u32, h: u32, c: Pixel) {
        let s = self.scale;
        let x0 = (x * s as i32).max(0) as u32;
        let y0 = (y * s as i32).max(0) as u32;
        let x1 = ((x + w as i32) * s as i32).max(0) as u32;
        let y1 = ((y + h as i32) * s as i32).max(0) as u32;
        for py in y0..y1.min(self.buf_height) {
            for px in x0..x1.min(self.buf_width) {
                self.put(px, py, c);
            }
        }
    }

    /// 1 逻辑像素宽的边框 —— 超采样是否生效，看它边缘就知道
    pub fn stroke_rect(&mut self, x: i32, y: i32, w: u32, h: u32, c: Pixel, thickness: u32) {
        self.fill_rect(x, y, w, thickness, c); // 上
        self.fill_rect(x, y + h as i32 - thickness as i32, w, thickness, c); // 下
        self.fill_rect(x, y, thickness, h, c); // 左
        self.fill_rect(x + w as i32 - thickness as i32, y, thickness, h, c); // 右
    }

    /// 1 逻辑像素的黑白棋盘格 —— **锐利度检测的最佳图案**：
    /// 若缩放/重采样有问题，它会糊成一片灰。
    pub fn checkerboard(&mut self, x: i32, y: i32, w: u32, h: u32, c1: Pixel, c2: Pixel, cell: u32) {
        for j in 0..h {
            for i in 0..w {
                let on = ((i / cell) + (j / cell)) % 2 == 0;
                self.fill_rect(x + i as i32, y + j as i32, 1, 1, if on { c1 } else { c2 });
            }
        }
    }

    /// 8x8 位图文字；`px` 是每个字形像素放大到多少逻辑像素
    pub fn text(&mut self, x: i32, y: i32, s: &str, c: Pixel, px: u32) {
        let mut cx = x;
        for ch in s.chars() {
            let Some(glyph) = BASIC_FONTS.get(ch) else {
                cx += 8 * px as i32 + px as i32;
                continue;
            };
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..8u32 {
                    // 位图字体是 LSB 在左
                    if bits & (1 << col) != 0 {
                        self.fill_rect(
                            cx + (col * px) as i32,
                            y + (row as u32 * px) as i32,
                            px,
                            px,
                            c,
                        );
                    }
                }
            }
            cx += (8 * px) as i32 + px as i32; // 字间距 1 个 px
        }
    }

    /// 用矢量字体画一行文字。坐标与 `size_px` 都是**逻辑**单位，
    /// 内部换算成缓冲坐标 —— 字号会被乘以 `scale`，这正是超采样锐化文字的机制。
    ///
    /// 返回逻辑宽度。
    pub fn draw_text(
        &mut self,
        tr: &mut crate::TextRenderer,
        x: f32,
        y: f32,
        s: &str,
        size_logical: f32,
        color: Pixel,
    ) -> f32 {
        let sc = self.scale as f32;
        let bx = (x * sc).round() as i32;
        let by = (y * sc).round() as i32;
        let w = tr.draw(
            self.pixels,
            self.buf_width,
            self.buf_height,
            bx,
            by,
            s,
            size_logical * sc,
            color,
        );
        w / sc
    }

    /// 量文字宽度（逻辑单位），不绘制
    pub fn measure_text(&mut self, tr: &mut crate::TextRenderer, s: &str, size_logical: f32) -> f32 {
        tr.measure(s, size_logical * self.scale as f32) / self.scale as f32
    }
}
