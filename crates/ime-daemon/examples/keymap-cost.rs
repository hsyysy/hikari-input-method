//! 量一次 keymap 广播的**代价** —— 客户端收到 `wl_keyboard.keymap` 后必须
//! 用 xkbcommon 重新编译整份 keymap，这个例子测的就是那一步有多贵。
//!
//! 配套 [`keymap-count`](keymap-count.rs)：那个测"发生多少次"，这个测"每次多贵"。
//!
//! 取的是**真实 keymap** —— 连上合成器，从 `wl_keyboard.keymap` 的 fd 里读出来，
//! 不做任何近似（hikari 的 keymap 带 `ctrl:swapcaps` 等选项，35 KB 左右）。
//!
//! ```sh
//! cargo run --release -p ime-daemon --example keymap-cost
//! ```

use std::os::fd::AsFd;
use std::time::Instant;

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_keyboard, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};

#[derive(Default)]
struct State {
    keymap: Option<Vec<u8>>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { fd, size, .. } = event {
            use std::os::unix::fs::FileExt;
            let f = std::fs::File::from(fd.as_fd().try_clone_to_owned().unwrap());
            let mut buf = vec![0u8; size as usize];
            if let Ok(got) = f.read_at(&mut buf, 0) {
                buf.truncate(got);
                state.keymap = Some(buf);
            }
        }
    }
}

fn main() {
    let conn = Connection::connect_to_env().expect("连不上 Wayland");
    let (globals, mut queue) = registry_queue_init::<State>(&conn).expect("拿不到 globals");
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).expect("没有 wl_seat");
    let _kb = seat.get_keyboard(&qh, ());

    let mut state = State::default();
    // 等到 keymap 到达为止
    for _ in 0..10 {
        queue.roundtrip(&mut state).expect("roundtrip 失败");
        if state.keymap.is_some() {
            break;
        }
    }

    let bytes = state.keymap.expect("没收到 keymap");
    let text = std::str::from_utf8(&bytes).expect("keymap 不是 UTF-8");

    println!("真实 keymap: {} 字节 ({:.1} KB)", bytes.len(), bytes.len() as f64 / 1024.0);
    println!();

    // 预热一次（首次有页错误、内存分配等一次性成本）
    let _ = xkbcommon::xkb::Keymap::new_from_string(
        &xkbcommon::xkb::Context::new(0),
        text.to_string(),
        xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1,
        0,
    );

    const REPS: u32 = 200;
    let ctx = xkbcommon::xkb::Context::new(0);
    let t = Instant::now();
    for _ in 0..REPS {
        let km = xkbcommon::xkb::Keymap::new_from_string(
            &ctx,
            text.to_string(),
            xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1,
            0,
        )
        .expect("编译失败");
        std::hint::black_box(&km);
    }
    let per = t.elapsed().as_secs_f64() / REPS as f64;

    println!("编译一次耗时: {:.3} ms（{REPS} 次平均）", per * 1000.0);
    println!();
    println!("按实测的「每个按键事件 2 次广播」算：");
    for n in [1u32, 3, 5, 10] {
        let ms = per * 1000.0 * 2.0 * n as f64;
        println!("  {n:2} 个客户端 × 2 次 = 每次按键 {ms:6.2} ms 的额外 CPU（分布在各客户端进程）");
    }
    println!();
    println!("注：这是**每次按键事件**（按下 + 抬起各算一次）的成本，");
    println!("    且不论客户端有没有焦点 —— 全部客户端都要重算。");
}
