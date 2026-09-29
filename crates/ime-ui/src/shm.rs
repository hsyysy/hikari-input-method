//! `wl_shm` 缓冲池 —— 候选窗的像素来源。
//!
//! # 三个坑
//!
//! 1. **`ARGB8888` 的字节序**：协议原文是
//!    *"32-bit ARGB format, [31:0] A:R:G:B 8:8:8:8 little endian"*
//!    （`wayland.xml:335`）。即 32 位值是 `0xAARRGGBB`，小端落盘后
//!    **内存字节序是 B,G,R,A**。本模块用 `u32` 直接写内存，天然正确；
//!    绝不按 "A,R,G,B" 字母顺序逐字节拼。
//! 2. **预乘 alpha**：`wayland.xml:330-331` 规定 wl_shm 全部格式都是
//!    **pre-multiplied alpha**。写半透明像素时**必须先乘 alpha**
//!    （`c = c * a / 255`），否则圆角/阴影边缘会出现发亮 halo。
//!    → 用 [`rgba_premul`] 而不是自己拼。
//! 3. **`wl_buffer.release` 门槛**：只有收到 `release` 的缓冲才可重写
//!    （`wayland.xml`："Sent when this wl_buffer is no longer used by the
//!    compositor"）。在合成器还没释放时重写会撕裂/花屏。
//!    → 用 [`ShmPool::acquire`] 取缓冲，它只返回已释放的槽位。

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;

use wayland_client::protocol::{wl_buffer, wl_shm, wl_shm_pool};
use wayland_client::{Dispatch, Proxy, QueueHandle};

/// 一个像素：`0xAARRGGBB`（写进内存即为小端 B,G,R,A，见模块文档 ①）
pub type Pixel = u32;

/// 不透明像素
#[inline]
pub const fn rgb(r: u8, g: u8, b: u8) -> Pixel {
    0xFF00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// **预乘 alpha** 的像素（见模块文档 ②）。
///
/// 传入**非预乘**的 RGB + alpha，这里做乘法。半透明像素必须走这个函数。
#[inline]
pub const fn rgba_premul(r: u8, g: u8, b: u8, a: u8) -> Pixel {
    const fn m(c: u8, a: u8) -> u32 {
        (c as u32 * a as u32) / 255
    }
    ((a as u32) << 24) | (m(r, a) << 16) | (m(g, a) << 8) | m(b, a)
}

/// `memfd_create` —— 建匿名共享内存文件给 `wl_shm_pool` 用（std 没有这个 API）
fn create_shm_fd(size: usize) -> std::io::Result<OwnedFd> {
    let name = c"ime-shm";
    // SAFETY: name 是 NUL 结尾的静态字符串，flags=0 合法
    let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd > 0 由 memfd_create 返回，所有权移交给 OwnedFd
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: fd 有效
    if unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

/// mmap 出来的像素区。
struct Mapping {
    ptr: NonNull<u32>,
    /// 以 u32 计的长度
    len: usize,
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: ptr 来自 mmap，len 是当时的字节数/4，此处只 munmap 一次
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len * 4) };
    }
}

/// 池中的一块缓冲
struct Slot {
    buffer: wl_buffer::WlBuffer,
    /// 该缓冲在池内的起始像素下标
    start: usize,
    /// 合成器是否已释放它（模块文档 ③）
    released: bool,
}

/// 固定尺寸的 shm 缓冲池，含 `n` 块同尺寸缓冲。
pub struct ShmPool {
    pool: wl_shm_pool::WlShmPool,
    map: Mapping,
    slots: Vec<Slot>,
    width: u32,
    height: u32,
    /// 已因「全在飞」而放弃的帧数（诊断用）
    pub dropped_frames: u64,
}

impl ShmPool {
    /// `n` 通常取 2；建议全在飞时**优先扩池**而非丢帧。
    pub fn new<D>(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<D>,
        width: u32,
        height: u32,
        n: usize,
    ) -> std::io::Result<Self>
    where
        D: Dispatch<wl_shm_pool::WlShmPool, ()> + Dispatch<wl_buffer::WlBuffer, ()> + 'static,
    {
        let stride = width * 4; // 每像素 4 字节
        let slot_px = (width * height) as usize;
        let slot_bytes = slot_px * 4;
        let total = slot_bytes * n;

        let fd = create_shm_fd(total)?;
        let pool = shm.create_pool(fd.as_fd(), total as i32, qh, ());

        // SAFETY: fd 有效且已 ftruncate 到 total；PROT_READ|WRITE + MAP_SHARED 是 wl_shm 常规用法
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        // 申请的内存由 Box 的分配器保证对齐足够，这里的 mmap 返回页对齐，u32 对齐满足
        let ptr = NonNull::new(raw.cast::<u32>()).expect("mmap 非空");

        let mut slots = Vec::with_capacity(n);
        for i in 0..n {
            let start = i * slot_px;
            let buffer = pool.create_buffer(
                (i * slot_bytes) as i32,
                width as i32,
                height as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
                qh,
                (),
            );
            slots.push(Slot {
                buffer,
                start,
                released: true, // 新缓冲立即可用
            });
        }

        Ok(Self {
            pool,
            map: Mapping { ptr, len: total / 4 },
            slots,
            width,
            height,
            dropped_frames: 0,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// 取一块可重写的缓冲下标；全都在飞时返回 `None`。
    ///
    /// 返回 `None` 时**不要**直接丢弃内容：
    /// 候选窗丢帧会停在旧内容直到下次按键，故优先扩池。
    pub fn acquire(&mut self) -> Option<usize> {
        match self.slots.iter().position(|s| s.released) {
            Some(i) => Some(i),
            None => {
                self.dropped_frames += 1;
                None
            }
        }
    }

    /// 交给合成器后调用（`attach` + `commit` 之后）
    pub fn mark_in_flight(&mut self, idx: usize) {
        if let Some(s) = self.slots.get_mut(idx) {
            s.released = false;
        }
    }

    /// 收到 `wl_buffer.release` 时调用（按代理 id 反查）
    pub fn mark_released_by_id(&mut self, id: wayland_client::backend::ObjectId) -> Option<usize> {
        let idx = self.slots.iter().position(|s| s.buffer.id() == id)?;
        self.slots[idx].released = true;
        Some(idx)
    }

    pub fn buffer(&self, idx: usize) -> Option<&wl_buffer::WlBuffer> {
        self.slots.get(idx).map(|s| &s.buffer)
    }

    /// 某槽位的像素区（长度 = width*height，按行优先）
    pub fn pixels_mut(&mut self, idx: usize) -> Option<&mut [u32]> {
        let start = self.slots.get(idx)?.start;
        let len = (self.width * self.height) as usize;
        // SAFETY: start+len 不超出映射区（slot 布局保证）；&mut self 保证独占
        Some(unsafe { std::slice::from_raw_parts_mut(self.map.ptr.as_ptr().add(start), len) })
    }

    /// 销毁底层 pool 对象。已 attach 的缓冲不受影响。
    pub fn destroy_pool(&self) {
        self.pool.destroy();
    }
}
