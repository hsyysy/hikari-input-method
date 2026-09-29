//! 探针：数合成器**向普通客户端**广播了多少次 `wl_keyboard.keymap`。
//!
//! 动机：hikari 在 `normal_mode.c:331` 的 `key_handler` 里
//! **每个按键事件**都调 `wlr_seat_set_keyboard()`。wlroots 那边判断"keymap 变了没"
//! 用的是**指针比较**（`wlr_seat_keyboard.c:126-128`）：
//!
//! ```c
//! bool needs_keymap_update =
//!     !seat->keyboard_state.keyboard || !keyboard ||
//!     seat->keyboard_state.keyboard->keymap != keyboard->keymap;   // ← 指针
//! ```
//!
//! 物理键盘与虚拟键盘是**两个不同的 `wlr_keyboard` 对象**，各自的 keymap 指针
//! 必然不等 → 每次在两者之间切换都会重发一遍全量 keymap 给**所有**建过
//! `wl_keyboard` 的客户端。而 `seat_client_send_keymap()` 是
//! `wl_resource_for_each(resource, &client->keyboards)` —— **不看焦点**。
//!
//! 收端要为此重新 `xkb_keymap_new_from_string()` 编译 35 KB 的 keymap，
//! 这才是真正的代价。
//!
//! 用法（**在一个有焦点、能打字的窗口里**跑，或者直接对着桌面打字）：
//!
//! ```sh
//! cargo run --release -p ime-daemon --example keymap-count -- 10
//! #                                                      ↑ 观测秒数
//! ```
//!
//! 输出 `keymap/N 次按键` 这个比值才是重点：
//! 正常应该是 **0**（keymap 只在真的换键盘时才该变），
//! 若接近 2 就说明每个按键都在重发。

use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_keyboard, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};

#[derive(Default)]
struct State {
    keymaps: u64,
    keys: u64,
    enters: u64,
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
        match event {
            wl_keyboard::Event::Keymap { .. } => {
                state.keymaps += 1;
                // fd/size 只用于计量；这里不解析 keymap（我们数的是"次数"）
            }
            wl_keyboard::Event::Key { .. } => state.keys += 1,
            wl_keyboard::Event::Enter { .. } => state.enters += 1,
            _ => {}
        }
    }
}

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let conn = Connection::connect_to_env().expect("连不上 Wayland");
    let (globals, mut queue) = registry_queue_init::<State>(&conn).expect("拿不到 globals");
    let qh = queue.handle();

    // 版本 ≥ 7 才有 repeat_info，跟普通客户端一致
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).expect("没有 wl_seat");
    // 关键：只要建了 wl_keyboard 对象，就会出现在 client->keyboards 里，
    // 从而收到**每一次** keymap 广播 —— 不管有没有焦点。
    let _kb = seat.get_keyboard(&qh, ());

    let mut state = State::default();

    // 先排空初始事件（首次 keymap 是正常的，不该计入"重复"）
    queue.roundtrip(&mut state).expect("roundtrip 失败");
    let baseline = state.keymaps;

    println!("观测 {secs} 秒 —— 请现在开始打字（正常速度即可）…");
    println!("（首次 keymap 不计入统计；基线已扣除）\n");

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        queue.blocking_dispatch(&mut state).expect("dispatch 失败");
    }

    let keymaps = state.keymaps - baseline;
    let keys = state.keys;
    println!("── 结果 ──");
    println!("keymap 广播 : {keymaps}");
    println!("按键事件    : {keys}   （0 = 探针窗口没焦点，不影响 keymap 计数）");
    println!("enter 事件  : {}", state.enters);
    println!();
    if keymaps == 0 {
        println!("✅ 没有多余广播。");
    } else {
        println!("⚠️  观测期内发生了 {keymaps} 次 keymap 广播。");
        println!("    每多一次，**所有**建过 wl_keyboard 的客户端都要重新");
        println!("    xkb_keymap_new_from_string() 编译一遍 ~35 KB 的 keymap。");
        println!("    把 keymaps 除以期间实际敲下的按键数：≈2 说明每个转发的");
        println!("    按键触发两轮（物理→vk、vk→物理），≈1 说明只走了一轮。");
    }
}
