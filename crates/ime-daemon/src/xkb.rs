//! keycode → 字符/符号的解析。
//!
//! keymap 我们**本来就有** —— 它随 grab 的 `keymap` 事件一起到达，
//! 且为了打断 wlroots 的 keymap 回环（M1 发现的 bug），我们已经把它读成了字符串。
//! 这里直接复用那份字符串建 xkb 状态，不需要额外读 fd。
//!
//! 用 xkb 而非硬编码键表的好处：**尊重用户的键盘布局与 xkb 选项**。
//! 例如本机 hikari 配了 `ctrl:swapcaps`，CapsLock 与 Ctrl 是交换的。

use xkbcommon::xkb;

pub struct Xkb {
    /// `State::new` 不复制 keymap，故必须与 state 一同保活
    _keymap: xkb::Keymap,
    state: xkb::State,
}

impl Xkb {
    /// 从 keymap 文本建状态。文本来自 grab 的 `keymap` 事件。
    pub fn new(keymap_text: &str) -> Option<Self> {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(
            &ctx,
            keymap_text.to_string(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )?;
        let state = xkb::State::new(&keymap);
        Some(Self {
            _keymap: keymap,
            state,
        })
    }

    /// 修饰键状态变化（来自 grab 的 `modifiers` 事件）
    pub fn update_mask(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.state.update_mask(depressed, latched, locked, 0, 0, group);
    }

    /// 该键在当前状态下产生的 keysym
    pub fn sym(&self, keycode: u32) -> xkb::Keysym {
        // 注意 +8：wl_keyboard 的 keycode 是 evdev 码，xkb 用的是「evdev + 8」
        self.state.key_get_one_sym(xkb::Keycode::new(keycode + 8))
    }

    /// 该键产生的文本（已考虑 Shift / 布局）。控制键返回空串或控制字符。
    pub fn utf8(&self, keycode: u32) -> String {
        self.state.key_get_utf8(xkb::Keycode::new(keycode + 8))
    }

    /// 是否按着 Ctrl（`Ctrl+Space` 总开关要用）
    pub fn has_ctrl(&self) -> bool {
        self.state
            .mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE)
    }

    /// 是否按着 Shift（判定「Shift + 字母 = 直接输入大写字母」要用）。
    ///
    /// 用修饰键名判定而不是「`utf8()` 出来是大写」—— 后者分不清 Shift 和
    /// CapsLock。开着 CapsLock 敲拼音是正常用法（`push_letter` 会转小写），
    /// 不该因此把字母放行给应用。
    pub fn has_shift(&self) -> bool {
        self.state
            .mod_name_is_active(xkb::MOD_NAME_SHIFT, xkb::STATE_MODS_EFFECTIVE)
    }

    /// 是否按着「命令类」修饰键（Ctrl / Alt / Super）。
    ///
    /// 有这些修饰键时输入法**不该消费**按键 —— 否则 Ctrl+C 之类会被吃掉。
    pub fn has_command_modifier(&self) -> bool {
        use xkb::STATE_MODS_EFFECTIVE;
        self.state.mod_name_is_active(xkb::MOD_NAME_CTRL, STATE_MODS_EFFECTIVE)
            || self.state.mod_name_is_active(xkb::MOD_NAME_ALT, STATE_MODS_EFFECTIVE)
            || self.state.mod_name_is_active(xkb::MOD_NAME_LOGO, STATE_MODS_EFFECTIVE)
    }
}

/// 常用 keysym 判定（避免到处写裸数字）
pub mod sym {
    use xkbcommon::xkb::Keysym;

    pub fn is_shift(s: Keysym) -> bool {
        s == Keysym::Shift_L || s == Keysym::Shift_R
    }
    pub fn is_space(s: Keysym) -> bool {
        s == Keysym::space
    }
    pub fn is_backspace(s: Keysym) -> bool {
        s == Keysym::BackSpace
    }
    pub fn is_escape(s: Keysym) -> bool {
        s == Keysym::Escape
    }
    pub fn is_return(s: Keysym) -> bool {
        s == Keysym::Return || s == Keysym::KP_Enter
    }
    #[allow(dead_code)] // M5 用 Tab 翻页时会用
    pub fn is_tab(s: Keysym) -> bool {
        s == Keysym::Tab
    }
    /// 主键盘数字 1..9 → `Some(1..9)`；0 返回 `None`（留给用户打 0）
    pub fn digit(s: Keysym) -> Option<u32> {
        let v = s.raw();
        if (xkbcommon::xkb::Keysym::_1.raw()..=xkbcommon::xkb::Keysym::_9.raw()).contains(&v) {
            Some(v - xkbcommon::xkb::Keysym::_1.raw() + 1)
        } else {
            None
        }
    }
}
