//! **M1：零逻辑透传** —— 只接管键盘并把按键原样回送，不做任何拼音处理。
//!
//! 这是整个项目最关键的里程碑：先证明「能安全接管键盘」，再谈拼音。
//! 顺序反过来，一旦转发有 bug，调试环境本身就无法输入命令。
//!
//! 验收标准：
//!   ① 桌面打字与快捷键**完全不受影响**
//!   ② **应用侧收到非空 xkb keymap**
//!   ③ 长按 BackSpace、CapsLock、Ctrl/Alt 状态正确
//!   ④ kill daemon 后键盘立即恢复，无残留按下的键
//!   ⑤ 反复 activate/deactivate 后仍能正常打字
//!
//! # 本文件落实的三条关键约束
//!
//! - **§5.3** keymap 必须转发给虚拟键盘，且**必须早于任何 `key()`**
//!   （否则触发 `ZWP_VIRTUAL_KEYBOARD_V1_ERROR_NO_KEYMAP`，**连接被打断**）
//! - **§5.2 / N7** `deactivate` 必须 `release()` grab
//!   （否则下次 `grab_keyboard()` 被**静默忽略**，输入法静默失效）
//! - **§5.1 B** 虚拟键盘**进程级创建一次**，不可 destroy 后重建
//!   （`has_keymap` 是 per-object 的，归零后发键会断连接）
//!
//! # §5.5 的实现纪律
//!
//! 「静默失效」的路径各写一条显式日志 + 断言，而不是靠"记得"。

use std::collections::BTreeSet;
use std::os::fd::AsFd;
use std::process::ExitCode;

mod compose;
mod popup;
mod xkb;

use compose::{Choice, Composer, ShiftTap};
use ime_ui::CandidateView;
use popup::Popup;
use xkb::Xkb;

use ime_protocol::input_method::{
    ZwpInputMethodKeyboardGrabV2, ZwpInputMethodManagerV2, ZwpInputMethodV2,
};
use ime_protocol::virtual_keyboard::{ZwpVirtualKeyboardManagerV1, ZwpVirtualKeyboardV1};
use ime_ui::{M2_HEIGHT, M2_WIDTH, SUPERSAMPLE};

use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat, wl_shm};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

// ─────────────────────────── 日志 ───────────────────────────

/// 是否打印每个按键（默认关，避免刷屏；`-v` 打开）
static mut VERBOSE: bool = false;

fn verbose() -> bool {
    // 只在启动时写一次，之后只读，无竞争
    unsafe { VERBOSE }
}

macro_rules! info {
    ($($a:tt)*) => { println!("[ime] {}", format!($($a)*)) };
}
macro_rules! vlog {
    ($($a:tt)*) => { if verbose() { println!("[ime] {}", format!($($a)*)) } };
}
/// 「静默失效」路径专用：出错也绝不静默
macro_rules! alarm {
    ($($a:tt)*) => { eprintln!("[ime] ⚠️  {}", format!($($a)*)) };
}

// ─────────────────────────── 状态 ───────────────────────────

#[derive(Default)]
struct Stats {
    keys_forwarded: u64,
    keys_released_on_deactivate: u64,
    activates: u64,
    deactivates: u64,
    // ── M4 ──
    /// 被输入法消费掉的按键（进拼音缓冲 / 选词 / 编辑）
    keys_consumed: u64,
    /// 上屏次数
    commits: u64,
}

struct State {
    // ── 协议对象 ──
    /// 虚拟键盘：**进程级创建一次、全程复用**（§5.1 B）。
    /// 绝不可 destroy 后重建 —— `has_keymap` 归零会导致发键时断连接。
    vk: ZwpVirtualKeyboardV1,
    /// input method 对象。**必须持有**：wayland-client 里 drop 一个 Proxy 会发送
    /// 析构请求，丢掉它输入法就没了。M1 不主动读它（事件里已带 `im` 参数），
    /// 这里存在只为保活。
    #[allow(dead_code)]
    im: Option<ZwpInputMethodV2>,
    /// keyboard grab：**每次 activate 新建、deactivate 必须 release**（§5.1 B / N7）
    grab: Option<ZwpInputMethodKeyboardGrabV2>,

    // ── 状态 ──
    /// `commit(serial)` 的 serial。**对象生命周期内单调递增，跨 activate/deactivate 累积**
    /// （wlroots 的 `current_serial` 无重置点，见 §5.2）。
    /// M1 不 commit，但计数保持正确以便 M4 直接用。
    done_count: u32,
    /// 虚拟键盘是否已收到 keymap。
    /// **为 false 时绝不能调 `vk.key()`** —— 会触发 NO_KEYMAP 协议错误并断连接。
    vk_has_keymap: bool,
    /// 当前按下的键，用于 `deactivate` / 退出时补发 release（避免 Ctrl 卡住）
    pressed: BTreeSet<u32>,
    /// 上次转发给虚拟键盘的 keymap 内容，用于**打断死循环**（见下方 keymap 处理）。
    /// 存全量内容而非哈希：只有 ~35KB，且能保证判断绝对正确。
    last_keymap: Option<Vec<u8>>,
    /// 因内容重复而跳过的 keymap 转发次数（用于诊断循环）
    keymap_skipped: u64,

    /// 候选窗（M2）。**进程级创建一次**，见 `popup.rs` 模块文档。
    ///
    /// 只有 `Dispatch<wl_buffer::WlBuffer>` 会在派发时借用它（收 release），
    /// 故用 `Option` 以便 `new()` 失败时降级为「无候选窗但按键仍可用」。
    pub(crate) popup: Option<Popup>,
    /// wl_shm，`resize()` 时需要
    pub(crate) shm: Option<wl_shm::WlShm>,


    // ── M4：输入法状态 ──
    /// keycode → 字符（keymap 来自 grab，见 `xkb.rs`）
    xkb: Option<Xkb>,
    /// 拼音缓冲 + 候选 + 中英状态
    composer: Option<Composer>,
    /// 候选窗当前是否应该可见（有输入时）
    popup_visible: bool,
    /// 上次发出的 preedit，用于去重（避免每键都 commit 一次）
    last_preedit: String,

    stats: Stats,
}

/// 从 fd 读全部内容（用 pread，**不改文件偏移** —— 合成器侧走 mmap 从 offset 0 映射）
fn read_fd(fd: &std::os::fd::OwnedFd, size: usize) -> Option<Vec<u8>> {
    use std::os::fd::AsFd;
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::from(fd.as_fd().try_clone_to_owned().ok()?);
    let mut buf = vec![0u8; size];
    let got = f.read_at(&mut buf, 0).ok()?;
    buf.truncate(got);
    Some(buf)
}

fn now_msec() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0)
}

impl State {
    /// 把未释放的按键全部补发 release。
    ///
    /// 必要性：grab 期间若键按下未释放，直接丢弃状态会让应用侧**残留按下的键**
    /// （典型症状：Ctrl 卡住）。
    fn flush_pressed_keys(&mut self) {
        if self.pressed.is_empty() {
            return;
        }
        if !self.vk_has_keymap {
            // 没 keymap 时发键会断连接；此时也没什么可补发的
            alarm!(
                "有 {} 个未释放按键，但虚拟键盘尚无 keymap，无法补发 release",
                self.pressed.len()
            );
            self.pressed.clear();
            return;
        }

        let t = now_msec();
        let keys: Vec<u32> = self.pressed.iter().copied().collect();
        for key in &keys {
            self.vk.key(t, *key, wl_keyboard_state::RELEASED);
        }
        self.stats.keys_released_on_deactivate += keys.len() as u64;
        info!("补发 {} 个未释放按键的 release: {:?}", keys.len(), keys);
        self.pressed.clear();
    }

    // ─────────────────── M4：输入法逻辑 ───────────────────

    /// 当前状态的人话描述 —— **照实读**，不写死。
    /// 「开着但处于英文」这种状态最难查，日志里必须能一眼看出来。
    fn enabled_desc(&self) -> &'static str {
        match self.composer.as_ref() {
            Some(c) if !c.is_enabled() => "关闭（英文直通）",
            Some(c) if c.is_chinese_active() => "开启（中文）",
            Some(_) => "开启（英文，Shift 可切回中文）",
            None => "无拼音引擎（纯透传）",
        }
    }

    /// 处理按下。返回 `true` = 输入法消费（**不转发给应用**）。
    ///
    /// 判定顺序有讲究：先排除命令类修饰键，再处理编辑键，最后才是字母。
    /// 否则 `Ctrl+C` 之类会被吃掉。
    fn handle_key_press(&mut self, keycode: u32, qh: &QueueHandle<Self>) -> bool {
        let Some(xkb) = self.xkb.as_ref() else {
            return false;
        };
        let s = xkb.sym(keycode);

        // ── Ctrl+Space：总开关（开/关输入法）──
        //
        // ⚠️ 必须在下面「命令类修饰键一律不消费」的早退**之前**判断，
        // 否则 Ctrl+Space 会被当成普通 Ctrl 组合放行给应用。
        if xkb.has_ctrl() && xkb::sym::is_space(s) {
            let pending = self.composer.as_mut().and_then(|c| c.toggle_enabled());
            // 关闭时把**原始拼音字母**上屏（不是候选词）——
            // 这个场景通常是「想打英文但忘了关输入法」，用户要的是那串字母本身。
            if let Some(t) = pending {
                self.commit_string(&t);
            }
            // ⚠️ 这句话必须**照实读状态**，不能写死。
            // 曾经写死成「开启（中文）」，而 `toggle_enabled` 当时不复位 `lang`，
            // 于是日志说中文、实际是英文直通 —— 排查时被这行骗了很久。
            info!("输入法 → {}", self.enabled_desc());
            self.refresh_ui(qh);
            return true;
        }

        // Shift 单独按 = 切中英（在 composer 里判定；总开关关着时不响应）
        if xkb::sym::is_shift(s) {
            if let Some(c) = self.composer.as_mut() {
                c.shift_pressed(keycode);
            }
            return false; // Shift 本身不消费，要让应用知道修饰键状态
        }
        if let Some(c) = self.composer.as_mut() {
            c.note_other_key();
        }

        // 命令类修饰键（Ctrl/Alt/Super）一律不消费
        if xkb.has_command_modifier() {
            return false;
        }

        // 总开关关着 → 完全不介入，所有按键直通
        if !self.composer.as_ref().is_some_and(|c| c.is_enabled()) {
            return false;
        }

        let composing = self.composer.as_ref().is_some_and(|c| c.is_composing());
        let chinese = self
            .composer
            .as_ref()
            .is_some_and(|c| c.is_chinese_active());

        // ── 编辑键（仅在输入中生效）──
        if composing {
            if xkb::sym::is_escape(s) {
                self.composer.as_mut().unwrap().clear();
                self.refresh_ui(qh);
                return true;
            }
            if xkb::sym::is_backspace(s) {
                self.composer.as_mut().unwrap().pop_letter();
                self.refresh_ui(qh);
                return true;
            }
            if xkb::sym::is_space(s) {
                // 空格：选中**当前高亮项**（通常是第一个），与数字键走同一条路 ——
                // 只覆盖一部分输入的候选同样只"定"前缀、不上屏，
                // 否则剩下的拼音会被一起丢掉。
                match self.composer.as_mut().unwrap().choose_selected() {
                    Choice::Commit(text) => self.commit_string(&text),
                    Choice::FixedPrefix | Choice::Miss => {}
                }
                self.refresh_ui(qh);
                return true;
            }
            if xkb::sym::is_return(s) {
                // 回车：上屏**原始拼音**（想打英文串时的出口）
                let raw = self.composer.as_mut().unwrap().take_raw();
                self.commit_text_opt(raw, qh);
                return true;
            }
            if let Some(d) = xkb::sym::digit(s) {
                // 数字键 = **选中并上屏**（标准 IME 行为）。
                // 只选中不上屏是错的：用户按了 2 就是要「背景」，不是「把光标移到背景」。
                //
                // 例外：候选只覆盖了输入的一部分（`zheli` 的「这」只吃 `zhe`）时
                // **不上屏**，而是定为前缀、接着选剩下的 —— 见 `Composer::choose`。
                let idx = (d as usize).saturating_sub(1);
                match self.composer.as_mut().unwrap().choose(idx) {
                    Choice::Commit(text) => self.commit_string(&text),
                    // preedit 已经变成「已定前缀 + 剩余拼音」，下面统一刷新
                    Choice::FixedPrefix => {}
                    Choice::Miss => {}
                }
                self.refresh_ui(qh);
                return true; // 数字在输入中被消费（选词），无论是否命中
            }
        }

        // ── 中文态：标点映射 + 字母进拼音缓冲 ──
        //
        // 放在编辑键**之后**：`Escape`/`BackSpace`/`space`/`Return`/数字在拼字时
        // 各有语义，不能被标点抢走。
        if chinese {
            let t = xkb.utf8(keycode);
            if let Some(ch) = t.chars().next() {
                // 标点：拼字中先上屏当前选中项，再接标点（合成一次 commit）。
                // 标准 IME 行为 —— `nihao` 之后按 `,` 应得到「你好，」，
                // 而不是把逗号插进 preedit 中间。
                if let Some(p) = self.composer.as_mut().unwrap().punctuation(ch) {
                    let prefix = if composing {
                        self.composer.as_mut().unwrap().take_selected()
                    } else {
                        None
                    };
                    let text = match prefix {
                        Some(word) => format!("{word}{p}"),
                        None => p.to_string(),
                    };
                    self.commit_string(&text);
                    self.refresh_ui(qh);
                    return true;
                }

                // Shift + 字母：**直接输入大写字母**，与英文态完全一样 ——
                // 不消费，按键原样回送给应用，由应用自己的 xkb 产生大写。
                //
                // 拼字中要先把手上这段拼音上屏，否则 preedit 会悬在那里，
                // 应用收到的字母和屏幕上的拼音打架。上屏的同样是**原始拼音**
                // （与 Enter、单独按 Shift 一致）—— Shift 在这个输入法里
                // 始终是"把拼音原样吐出来"的那个出口。
                if ch.is_ascii_alphabetic() && xkb.has_shift() {
                    if composing {
                        let raw = self.composer.as_mut().unwrap().take_raw();
                        self.commit_text_opt(raw, qh);
                    }
                    return false; // 不消费
                }

                if ch.is_ascii_alphabetic() {
                    let ok = self.composer.as_mut().unwrap().push_letter(ch);
                    if ok {
                        self.refresh_ui(qh);
                        return true;
                    }
                }
            }
        }
        false
    }

    /// 处理释放。返回 `true` = 消费（该键的按下被我们吃掉了）。
    fn handle_key_release(&mut self, keycode: u32, qh: &QueueHandle<Self>) -> bool {
        // 先看是不是**单独按了一下 Shift**：拼字中 = 按 Enter（原样上屏），
        // 没拼字 = 切中英。两种都返回 `false` —— Shift 本身不消费，
        // 否则应用不知道修饰键松开了。
        let tap = self
            .composer
            .as_mut()
            .map(|c| c.shift_released(keycode))
            .unwrap_or(ShiftTap::Nothing);
        match tap {
            ShiftTap::Raw(raw) => {
                info!("Shift（拼字中）→ 上屏原始拼音: {raw}");
                self.commit_string(&raw);
                self.refresh_ui(qh);
                return false;
            }
            ShiftTap::Lang => {
                // 用 enabled_desc 而非自己拼 —— 「切到英文后怎么回去」正是这里最容易丢的信息
                info!("中英切换 → {}", self.enabled_desc());
                self.refresh_ui(qh);
                return false;
            }
            ShiftTap::Nothing => {}
        }
        self.composer
            .as_mut()
            .is_some_and(|c| c.consume_release(keycode))
    }

    /// 上屏一段文字（`None` 表示无内容可上屏，只刷新 UI）
    fn commit_text_opt(&mut self, text: Option<String>, qh: &QueueHandle<Self>) {
        if let Some(t) = text {
            self.commit_string(&t);
        }
        self.refresh_ui(qh);
    }

    /// 上屏。
    ///
    /// ⚠️ `set_preedit_string` / `commit_string` 都只是**暂存**，
    /// 必须再发 `commit(serial)` 才生效。
    /// 漏掉 `commit` 的表现是「界面毫无反应」且**不报错**。
    fn commit_string(&mut self, text: &str) {
        let Some(im) = self.im.as_ref() else { return };
        im.set_preedit_string(String::new(), 0, 0); // 清掉预编辑
        im.commit_string(text.to_string());
        im.commit(self.done_count);
        self.stats.commits += 1;
        info!("上屏: {text}");
    }

    /// 把当前 preedit 与候选推给合成器，并重画候选窗。
    fn refresh_ui(&mut self, qh: &QueueHandle<Self>) {
        let (preedit, words, selected) = match self.composer.as_ref() {
            Some(c) => (
                c.preedit_display(),
                c.candidates().iter().map(|x| x.word.clone()).collect::<Vec<_>>(),
                c.selected(),
            ),
            None => (String::new(), Vec::new(), 0),
        };

        // preedit 变了才发 —— 避免每个按键都无谓地 commit 一次
        if preedit != self.last_preedit {
            if let Some(im) = self.im.as_ref() {
                let n = preedit.len() as i32;
                im.set_preedit_string(preedit.clone(), n, n);
                im.commit(self.done_count);
                // 诊断：把"实际发出去的 preedit 与光标字节偏移"打出来。
                // 排查「候选窗与光标对不齐」时必须能对齐这条时间线 ——
                // 光看合成器回来的 cursor_rectangle 无法判断是发送端还是
                // 客户端把位置算歪了。
                vlog!("preedit 发出: {preedit:?} cursor={n}..{n}（字节偏移）");
            }
            self.last_preedit = preedit.clone();
        }

        // 没有候选 → **真正隐藏**候选窗。
        //
        // 只在状态从"可见"变"不可见"时 hide 一次，避免每键都 attach/commit。
        // 触发场景：选完词（候选被 take 走）、Esc 清空、缓冲删空、
        // Ctrl+Space 关闭输入法。
        if words.is_empty() {
            if self.popup_visible {
                if let Some(p) = self.popup.as_mut() {
                    p.hide();
                }
                self.popup_visible = false;
            }
            return;
        }
        self.popup_visible = true;

        let view = CandidateView {
            preedit: &preedit,
            candidates: &words,
            selected,
        };
        let (Some(popup), Some(shm)) = (self.popup.as_mut(), self.shm.as_ref()) else {
            return;
        };
        if let Err(e) = popup.show(shm, qh, &view) {
            alarm!("候选窗渲染失败: {e}");
        }
    }
}

/// `wl_keyboard.key_state` 的两个取值（枚举在 wayland.xml 里，这里直接用具名常量避免额外依赖）
mod wl_keyboard_state {
    pub const RELEASED: u32 = 0;
    pub const PRESSED: u32 = 1;
}

// ─────────────────────── 必需 global 检查 ───────────────────────

const REQUIRED: &[&str] = &[
    "zwp_input_method_manager_v2",
    "zwp_virtual_keyboard_manager_v1",
    "wl_seat",
    "wl_shm",
];

fn check_globals(globals: &GlobalList) -> Vec<String> {
    let present: Vec<String> = globals
        .contents()
        .clone_list()
        .iter()
        .map(|g| g.interface.clone())
        .collect();
    REQUIRED
        .iter()
        .filter(|g| !present.iter().any(|p| p == *g))
        .map(|g| g.to_string())
        .collect()
}

// ─────────────────────── Dispatch 实现 ───────────────────────

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

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpInputMethodManagerV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpInputMethodManagerV2,
        _: <ZwpInputMethodManagerV2 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ── grab：按键来源 ──

impl Dispatch<ZwpInputMethodKeyboardGrabV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpInputMethodKeyboardGrabV2,
        event: <ZwpInputMethodKeyboardGrabV2 as Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ime_protocol::input_method::zwp_input_method_keyboard_grab_v2::Event;
        match event {
            // ── §5.3 第一步：转发 keymap，必须早于任何 key() ──
            Event::Keymap { format, fd, size } => {
                let format = match format {
                    wayland_client::WEnum::Value(f) => f,
                    wayland_client::WEnum::Unknown(v) => {
                        alarm!("未知 keymap format {v} —— 无法转发，按键将不可用");
                        return;
                    }
                };
                // ── 去重：打断 wlroots 的 keymap 回环 ──
                //
                // 背景：我们转发 keymap 给虚拟键盘后，hikari 会调
                // `wlr_keyboard_set_keymap(vk_kb)`，触发 vk 的 keymap 事件；
                // 而 wlroots 的 `handle_keyboard_keymap()` **无任何守卫**，
                // 会无条件把 keymap 再发回给我们 → 我们再转发 → 死循环。
                // （wlroots 注释里提到的 `wlr_keyboard_keymaps_match` 守卫
                //   只在 `set_keyboard()` 里，这条路径走不到。）
                //
                // 因为 grab 的键盘就是我们的虚拟键盘（daemon 创建 vk 时 hikari 的
                // `add_input` 调了 `wlr_seat_set_keyboard(seat, vk_kb)`，
                // activate 时 grab 取的正是 seat 当前键盘），所以这条回环必然发生。
                //
                // 客户端按内容去重即可干净打断：内容相同 = 回环回声，跳过。
                let content = read_fd(&fd, size as usize);
                if let (Some(new), Some(old)) = (&content, &state.last_keymap) {
                    if new == old {
                        state.keymap_skipped += 1;
                        if state.keymap_skipped == 1 {
                            alarm!(
                                "检测到 keymap 回环（wlroots handle_keyboard_keymap 无守卫）                                 —— 已按内容去重打断，后续相同 keymap 不再转发"
                            );
                        }
                        return;
                    }
                }

                // 收到的 fd 是【我们进程】里的 fd（SCM_RIGHTS 由内核新建），
                // 直接转发即可，无需 dup。
                state.vk.keymap(format, fd.as_fd(), size);
                state.vk_has_keymap = true;

                // 顺便用同一份 keymap 建 xkb 状态（M4 用它把 keycode 翻成字符）。
                // 复用 content 而不是再读一次 fd —— 文本已经在上面的去重逻辑里读出来了。
                if let Some(bytes) = &content {
                    match std::str::from_utf8(bytes) {
                        Ok(text) => match Xkb::new(text) {
                            Some(x) => {
                                info!("xkb 状态已建立（{} 字节 keymap）", bytes.len());
                                state.xkb = Some(x);
                            }
                            None => alarm!("xkb 无法解析这份 keymap —— 拼音输入将不可用"),
                        },
                        Err(_) => alarm!("keymap 不是合法 UTF-8 —— 拼音输入将不可用"),
                    }
                }
                state.last_keymap = content;
                info!("keymap 已转发给虚拟键盘 (size={size}) —— 现在可以发按键了");
            }

            Event::Key {
                serial: _,
                time,
                key,
                state: ks,
            } => {
                let pressed = match ks {
                    wayland_client::WEnum::Value(v) => v as u32,
                    wayland_client::WEnum::Unknown(v) => v,
                };

                // §5.5 断言：没 keymap 就发键会触发 NO_KEYMAP 协议错误并断连接
                if !state.vk_has_keymap {
                    alarm!("收到按键但虚拟键盘尚无 keymap —— 丢弃（发送会断连接）");
                    return;
                }

                // ── M4：先问输入法要不要消费这个键 ──
                let consumed = if pressed == wl_keyboard_state::PRESSED {
                    state.pressed.insert(key);
                    let c = state.handle_key_press(key, qh);
                    if c {
                        if let Some(cm) = state.composer.as_mut() {
                            cm.mark_consumed(key);
                        }
                    }
                    c
                } else {
                    state.pressed.remove(&key);
                    // ⚠️ 释放必须跟着消费 —— 否则应用会收到孤立的 release
                    state.handle_key_release(key, qh)
                };

                if consumed {
                    state.stats.keys_consumed += 1;
                    vlog!("key code={key} 被输入法消费");
                    return;
                }

                // ── 异常检测：「中文模式 + 已开启」下字母本该全部被消费 ──
                //
                // 若走到这里，说明有东西不对。最可能的是**修饰键卡住**
                // （`has_command_modifier()` 恒为 true），此时所有字母都会被放行，
                // 表现为「打不了拼音」。这类静默失效靠"记得去看"是抓不住的，
                // 所以这里显式报出来。
                //
                // `Shift + 字母`是**有意**放行的（直接输入大写字母），要排除掉，
                // 否则每敲一个大写字母就报一次假警。
                if pressed == wl_keyboard_state::PRESSED {
                    if let (Some(x), Some(c)) = (state.xkb.as_ref(), state.composer.as_ref()) {
                        let is_letter = x
                            .utf8(key)
                            .chars()
                            .next()
                            .is_some_and(|ch| ch.is_ascii_alphabetic());
                        if is_letter && c.is_chinese_active() && !x.has_shift() {
                            alarm!(
                                "中文模式下字母 keycode={key} 未被消费 —— \
                                 多半是修饰键卡住，拼音将无法输入"
                            );
                        }
                    }
                }

                // 未消费 → 原样回送
                state.vk.key(time, key, pressed);
                state.stats.keys_forwarded += 1;
                vlog!(
                    "key time={time} code={key} {} (按下 {} 个)",
                    if pressed == wl_keyboard_state::PRESSED { "↓" } else { "↑" },
                    state.pressed.len()
                );
            }

            Event::Modifiers {
                serial: _,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                if !state.vk_has_keymap {
                    alarm!("收到 modifiers 但虚拟键盘尚无 keymap —— 丢弃");
                    return;
                }
                // 四个字段都要传：locked 含 CapsLock，group 是键盘布局
                state.vk.modifiers(mods_depressed, mods_latched, mods_locked, group);
                // xkb 状态也要跟上，否则 key_get_utf8 会拿到错误的字符
                if let Some(x) = state.xkb.as_mut() {
                    x.update_mask(mods_depressed, mods_latched, mods_locked, group);
                }
                vlog!(
                    "modifiers dep={mods_depressed:#x} lat={mods_latched:#x} \
                     lock={mods_locked:#x} group={group}"
                );
            }

            Event::RepeatInfo { rate, delay } => {
                // 虚拟键盘协议没有 repeat_info 请求。
                // M1 无需自己重复：物理键的重复由 wlroots 产生，会作为额外的 key 事件送进来。
                vlog!("repeat_info rate={rate} delay={delay}（M1 不需要自实现重复）");
            }

            _ => {}
        }
    }
}

// ── input method：状态机 ──

impl Dispatch<ZwpInputMethodV2, ()> for State {
    fn event(
        state: &mut Self,
        im: &ZwpInputMethodV2,
        event: <ZwpInputMethodV2 as Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ime_protocol::input_method::zwp_input_method_v2::Event;
        match event {
            Event::Activate => {
                state.stats.activates += 1;

                // §5.5 断言：重复 grab 会被合成器【静默忽略】（N7），
                // 所以这里必须先确认没有残留的 grab。
                if let Some(old) = state.grab.take() {
                    alarm!("activate 时发现残留 grab —— 先 release 再重新 grab（N7 场景）");
                    old.release();
                }

                info!("activate → grab_keyboard()");
                state.grab = Some(im.grab_keyboard(qh, ()));

                // 候选窗可见性由合成器按 client_active 自动管理（见 popup.rs 模块文档），
                // 我们只需在内容变化时重画。这里刷新一次以覆盖状态变化。
                state.refresh_ui(qh);
            }

            Event::Deactivate => {
                state.stats.deactivates += 1;

                // ① 先补发未释放按键（在 release grab 之前，此时 grab 还有效）
                state.flush_pressed_keys();

                // ② 再 release grab —— 这步不可省略（§5.2 / N7）。
                //    漏掉不会报错，而是下次 grab_keyboard() 被静默忽略，
                //    表现为收不到 keymap、输入法静默失效。
                // 失焦时清掉输入状态并隐藏候选窗 —— 否则切走后旧候选还留在屏幕上
                if let Some(c) = state.composer.as_mut() {
                    c.clear();
                }
                if state.popup_visible {
                    if let Some(p) = state.popup.as_mut() {
                        p.hide();
                    }
                    state.popup_visible = false;
                }
                state.last_preedit.clear();

                match state.grab.take() {
                    Some(g) => {
                        g.release();
                        vlog!("deactivate → release() grab");
                    }
                    None => {
                        // hikari 会连发两次 deactivate（M0 实测），故这里是正常路径
                        vlog!("deactivate → 无 grab 可释放（幂等，正常）");
                    }
                }
            }

            Event::Done => {
                // 单调递增，**不因 deactivate 归零**（§5.2）
                state.done_count += 1;
                vlog!("done #{}", state.done_count);
            }

            Event::Unavailable => {
                alarm!("unavailable —— 已有别的 IM 占位（如 fcitx5 的 wayland 前端）");
                alarm!("按 §5.2 对象变惰性，只能 destroy");
            }

            Event::SurroundingText { .. }
            | Event::TextChangeCause { .. }
            | Event::ContentType { .. } => {
                // M1 不需要这些；M4 处理候选、M5 处理密码框
            }

            _ => {}
        }
    }
}

// ─────────────────────────── main ───────────────────────────

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-v" || a == "--verbose") {
        unsafe { VERBOSE = true };
    }

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[ime] ✖ {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env().map_err(|e| {
        format!(
            "无法连接 Wayland（WAYLAND_DISPLAY={:?}）: {e}",
            std::env::var("WAYLAND_DISPLAY")
        )
    })?;
    let (globals, mut event_queue) = registry_queue_init::<State>(&conn)?;
    let qh = event_queue.handle();

    let missing = check_globals(&globals);
    if !missing.is_empty() {
        return Err(format!(
            "缺少必需 global: {}",
            missing.join(", ")
        )
        .into());
    }

    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let im_manager: ZwpInputMethodManagerV2 = globals.bind(&qh, 1..=1, ())?;
    let vk_manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ())?;
    let compositor = popup::bind_compositor(&globals, &qh)?;

    // 虚拟键盘：**进程级创建一次**（§5.1 B）
    let vk = vk_manager.create_virtual_keyboard(&seat, &qh, ());

    // input method 对象：进程级一次
    let im = im_manager.get_input_method(&seat, &qh, ());

    // 候选窗：**进程级创建一次**，含 surface + popup 角色 + 缓冲池（popup.rs 模块文档）
    let popup = match Popup::new(&compositor, &im, &shm, &qh, (M2_WIDTH, M2_HEIGHT)) {
        Ok(p) => {
            info!("候选窗已创建: 超采样 {}x，字体 {:?}", SUPERSAMPLE, p.text.family());
            Some(p)
        }
        Err(e) => {
            // 降级：没有候选窗，但按键转发仍可用（M1 的能力不受影响）
            alarm!("候选窗创建失败，降级为「无候选窗」模式: {e}");
            None
        }
    };

    // 加载拼音引擎。失败也不致命 —— 退化为 M1 的纯透传。
    let dict_path = std::env::var("IME_DICT").unwrap_or_else(|_| {
        // 默认找仓库内的 data/dict/pinyin.bin（开发时）；找不到再说
        "data/dict/pinyin.bin".to_string()
    });
    let composer = match pinyin_engine::Engine::open(&dict_path) {
        Ok(e) => {
            info!(
                "拼音引擎已加载: {} 音节 / {} 条目（{dict_path}）",
                e.syllable_count(),
                e.dict().n_entries()
            );
            Some(Composer::new(e))
        }
        Err(e) => {
            alarm!("拼音引擎加载失败，退化为纯透传模式: {e}");
            alarm!("提示: 先跑 tools/fetch-dict.sh 生成词典");
            None
        }
    };

    let mut state = State {
        vk,
        im: Some(im),
        grab: None,
        done_count: 0,
        vk_has_keymap: false,
        pressed: BTreeSet::new(),
        last_keymap: None,
        keymap_skipped: 0,
        popup,
        shm: Some(shm),
        xkb: None,
        composer,
        popup_visible: false,
        last_preedit: String::new(),
        stats: Stats::default(),
    };

    // 注意：启动时**不画候选窗** —— 没有输入时不该显示。
    // 可见性由合成器按 client_active 自动管理，内容由 refresh_ui 在有输入时更新。

    // 照实读状态，别写死 —— 初始态是"关闭"，把这句话说错会让人以为输入法坏了
    info!("已就绪 —— {}", state.enabled_desc());
    info!(
        "按 Ctrl+Space 开关输入法；开启后：Shift+字母 直接打大写，\
         单独按一下 Shift 在拼字中等于 Enter、否则切中英"
    );
    info!("用 -v 查看每个按键；Ctrl+C 退出");
    info!("");

    // 退出时报告统计（Ctrl+C 走不到这里，但正常退出/出错时有用）
    let result = loop {
        if let Err(e) = event_queue.blocking_dispatch(&mut state) {
            break Err(e);
        }
    };

    let s = &state.stats;
    info!(
        "统计: 转发按键 {} / activate {} / deactivate {} / 补发 release {}",
        s.keys_forwarded, s.activates, s.deactivates, s.keys_released_on_deactivate
    );

    result.map_err(|e| e.into())
}
