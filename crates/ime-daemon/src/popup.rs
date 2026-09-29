//! 候选窗 surface —— 显示拼音与候选词。
//!
//! # 生命周期
//!
//! **进程级创建一次、全程复用**。协议原文（input-method-unstable-v2.xml）：
//!
//! - `activate`：*"makes any existing `zwp_input_popup_surface_v2` objects **visible**"*
//!   —— **不销毁**
//! - `deactivate`：*"compositor must make all existing `zwp_input_popup_surface_v2`
//!   objects **invisible** until the next activate event"*
//!
//! 即**可见性由合成器按 `client_active` 自动管理**
//! （wlroots `types/wlr_input_method_v2.c:160-168` `popup_surface_update_mapped()`），
//! 我们不需要在 activate/deactivate 时做任何 map/unmap，更不能重建对象
//! （重建会撞上角色复用规则，见 §5.1 B）。
//!
//! # 尺寸与缩放
//!
//! popup surface **没有 configure 握手**（§2.1），尺寸完全由我们定。
//! 按 §7.2，缓冲按 `SUPERSAMPLE` 倍渲染再 `set_buffer_scale`，
//! 由 hikari 降采样得到更锐利的文字。

use std::io;

use ime_protocol::input_method::{ZwpInputMethodV2, ZwpInputPopupSurfaceV2};
use ime_ui::{
    candidates, CandidateView, Canvas, ShmPool, TextRenderer, SUPERSAMPLE,
};

use wayland_client::protocol::{wl_buffer, wl_compositor, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Dispatch, Proxy, QueueHandle};

use crate::State;

pub struct Popup {
    surface: wl_surface::WlSurface,
    /// 必须保活：drop 会销毁角色对象，下次再建同一 surface 会撞 "role object still exists"
    #[allow(dead_code)]
    popup: ZwpInputPopupSurfaceV2,
    pool: ShmPool,
    /// 逻辑尺寸（未乘超采样）
    pub logical_size: (u32, u32),
    /// 中文文本渲染（`FontSystem::new()` 扫系统字体较慢，进程内建一次）
    pub text: TextRenderer,
}

impl Popup {
    /// 建 surface + popup 角色 + 缓冲池。**只在启动时调一次**（见模块文档）。
    pub fn new(
        compositor: &wl_compositor::WlCompositor,
        im: &ZwpInputMethodV2,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
        initial: (u32, u32),
    ) -> io::Result<Self> {
        let surface = compositor.create_surface(qh, ());
        // 按 §7.2：缓冲按超采样倍率渲染，再由合成器降采样
        surface.set_buffer_scale(SUPERSAMPLE as i32);

        let popup = im.get_input_popup_surface(&surface, qh, ());
        let pool = ShmPool::new(shm, qh, initial.0 * SUPERSAMPLE, initial.1 * SUPERSAMPLE, 2)?;

        Ok(Self {
            surface,
            popup,
            pool,
            logical_size: initial,
            text: TextRenderer::new(),
        })
    }

    /// 显示一屏候选：按内容量尺寸 → 必要时重建池 → 绘制 → 提交。
    ///
    /// **先算尺寸再 attach**，避免先画小后变大的抖动（§5.4）。
    pub fn show(
        &mut self,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
        view: &CandidateView<'_>,
    ) -> io::Result<()> {
        let (w, h) = candidates::measure(view, &mut self.text);
        if self.logical_size != (w, h) {
            self.pool.destroy_pool();
            self.pool = ShmPool::new(shm, qh, w * SUPERSAMPLE, h * SUPERSAMPLE, 2)?;
            self.logical_size = (w, h);
        }
        self.render_and_commit(view);
        Ok(())
    }

    fn render_and_commit(&mut self, view: &CandidateView<'_>) {
        let Some(slot) = self.pool.acquire() else {
            // 两块缓冲都在飞。§7.1.1 ③-b：这是兜底，正常应优先扩池。
            // 候选窗丢帧会停在旧内容直到下次按键，所以值得记一笔。
            return;
        };
        let (w, h) = (self.pool.width(), self.pool.height());

        {
            let pixels = self.pool.pixels_mut(slot).expect("槽位存在");
            let mut canvas = Canvas::new(pixels, w, h, SUPERSAMPLE);
            candidates::draw(&mut canvas, &mut self.text, view);
        }

        let buffer = self.pool.buffer(slot).expect("槽位存在");
        self.surface.attach(Some(buffer), 0, 0);

        // ⚠️ §7.1.1 ②：damage_buffer 用**缓冲坐标**，而 w/h 已是缓冲像素。
        // 若误传逻辑尺寸，只会刷新左上角 1/4 区域，症状像"渲染慢"极难定位。
        self.surface.damage_buffer(0, 0, w as i32, h as i32);
        self.surface.commit();
        self.pool.mark_in_flight(slot);
    }

    /// 隐藏候选窗。
    ///
    /// **必须真的 attach NULL buffer**，不能只是"不再绘制" —— 后者会让
    /// 上一帧的内容一直留在屏幕上。依据：wlroots 的 `wlr_surface_commit()`
    /// 在收到 NULL buffer 时会调 `wlr_surface_unmap()`
    /// （`types/wlr_compositor.c:517-519`），随后 `wlr_surface_has_buffer()`
    /// 变 false，`popup_surface_update_mapped()` 就不会再 map 它。
    pub fn hide(&mut self) {
        self.surface.attach(None, 0, 0);
        self.surface.commit();
    }

    /// 收到 `wl_buffer.release` 时调用（按代理 id 反查）
    pub fn on_buffer_release(&mut self, id: wayland_client::backend::ObjectId) {
        self.pool.mark_released_by_id(id);
    }
}

// ─────────────────── Dispatch 实现 ───────────────────

impl Dispatch<wl_compositor::WlCompositor, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpInputPopupSurfaceV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpInputPopupSurfaceV2,
        event: <ZwpInputPopupSurfaceV2 as Proxy>::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &QueueHandle<Self>,
    ) {
        use ime_protocol::input_method::zwp_input_popup_surface_v2::Event;
        if let Event::TextInputRectangle {
            x,
            y,
            width,
            height,
        } = event
        {
            // 光标矩形（**视图内局部坐标**）。合成器已负责定位候选窗，
            // 这里只用来约束宽度 / 做左右对齐微调（§5.4）。
            if std::env::var("IME_VERBOSE_RECT").is_ok() {
                println!("[ime] 光标矩形: x={x} y={y} w={width} h={height}（视图内局部坐标）");
            }
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        state: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            // §7.1.1 ③：只有收到 release 的缓冲才可重写
            if let Some(p) = state.popup.as_mut() {
                p.on_buffer_release(buffer.id());
            }
        }
    }
}

/// 绑定 `wl_compositor`
pub fn bind_compositor(
    globals: &wayland_client::globals::GlobalList,
    qh: &QueueHandle<State>,
) -> Result<wl_compositor::WlCompositor, wayland_client::globals::BindError> {
    globals.bind(qh, 1..=6, ())
}
