//! 输入法状态机 —— 拼音缓冲、候选、选词上屏。
//!
//! # 与协议的对接
//!
//! `set_preedit_string` / `commit_string` 都只是**暂存**，
//! 必须再发 `commit(serial)` 才生效，其中 `serial` = **已收到的 `done` 事件计数**。
//! 漏掉 `commit` 的表现是「界面毫无反应」，且**不报错** —— 最难查的一类。
//!
//! # 按键消费
//!
//! 按下时决定是否消费；**释放时必须跟着消费**，否则应用会收到孤立的 release。
//! 所以用 `consumed` 集合记住哪些键是我们吃掉的。

use pinyin_engine::{Candidate, Engine, DEFAULT_TOP_N};

/// 中英状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// 拼音输入
    Chinese,
    /// 直通（字母原样给应用）
    English,
}

/// 一个应用（wayland client）各自的输入法状态。
///
/// 「按应用记住状态」要存的就是这一对 —— 只存总开关不够：
/// 在 A 里 Shift 切到英文，切回 A 就该还是英文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientState {
    pub enabled: bool,
    pub lang: Lang,
}

impl Default for ClientState {
    /// 没见过的新应用一律「关闭 + 中文」—— 和合成器刚起来时一致。
    /// 默认关闭的理由见 `Composer::new`：一上来就吃字母是很糟的体验。
    fn default() -> Self {
        Self {
            enabled: false,
            lang: Lang::Chinese,
        }
    }
}

/// 选中一条候选的结果（见 [`Composer::choose`]）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// 这条候选覆盖了整个输入 → 上屏这段文本
    Commit(String),
    /// 只覆盖了前面一部分 → 已定为前缀，剩下的拼音继续选
    FixedPrefix,
    /// 没有这个序号
    Miss,
}

/// **单独按一下 Shift** 的结果（见 [`Composer::shift_released`]）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShiftTap {
    /// 什么也没发生：总开关关着，或 Shift 不是单独按的
    Nothing,
    /// 没在拼字 → 切换中英
    Lang,
    /// 拼字中 → 上屏原始拼音，与 Enter 相同
    Raw(String),
}

pub struct Composer {
    engine: Engine,
    /// 还没"定"的拼音（只含 `[a-z]`）
    buffer: String,
    /// **分步选词**已"定"下来的段：`(消耗掉的拼音, 对应的词)`
    ///
    /// `zheli` 里选了「这」→ `[("zhe", "这")]`，`buffer` 剩下 `li`，
    /// 候选重新按 `li` 出。这些字**还没上屏给应用**，只在 preedit 里显示 ——
    /// 所以 `Esc` 能整段取消，`BackSpace` 也能把它们退回成拼音。
    ///
    /// 不变量：`fixed` 非空时 `buffer` 必非空。定前缀的前提就是它只覆盖了
    /// 一部分输入（否则直接上屏了），而 `BackSpace` 退掉一段时会把它变回
    /// `buffer`。所以「已定的字悬在半空」这种状态不存在。
    fixed: Vec<(String, String)>,
    candidates: Vec<Candidate>,
    selected: usize,

    /// **总开关**：`Ctrl+Space` 切换。
    ///
    /// 关闭时输入法完全不介入 —— 所有按键直通，等价于「纯英文」。
    /// 与 `lang` 的区别：`enabled == false` 时连 Shift 切换都不响应，
    /// 输入法像是"不存在"。
    enabled: bool,
    /// 中英状态，**仅在 `enabled` 时有效**，由**单独按 Shift** 切换。
    ///
    /// 这是"短暂"切换：不动总开关，只在中文输入与英文直通之间来回。
    pub lang: Lang,
    /// 被我们消费掉的按键（按下时记，释放时清），避免孤立 release 漏给应用
    consumed: std::collections::BTreeSet<u32>,
    /// 记录 Shift 是否单独按下（用于「单独按 Shift 切中英」）
    shift_alone: Option<u32>,
    /// 下一个单/双引号是否是**开引号**（见 `punctuation()`）
    next_quote_open: [bool; 2],
}

impl Composer {
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            buffer: String::new(),
            fixed: Vec::new(),
            candidates: Vec::new(),
            selected: 0,
            // 默认**关闭**：合成器一起来就吃字母是很糟的体验 ——
            // 刚登录时多半想打英文（敲命令、输密码），却被输入法吞进拼音缓冲。
            // 想用中文按 Ctrl+Space 唤醒（开启时一律回到中文，见 toggle_enabled）。
            enabled: false,
            lang: Lang::Chinese,
            consumed: Default::default(),
            shift_alone: None,
            next_quote_open: [true, true],
        }
    }

    pub fn is_composing(&self) -> bool {
        !self.buffer.is_empty() || !self.fixed.is_empty()
    }

    /// 分步选词已定下来的那几段文字（拼起来就是 preedit 的中文部分）
    fn fixed_text(&self) -> String {
        self.fixed.iter().map(|(_, w)| w.as_str()).collect()
    }

    /// 总开关是否打开
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// 当前是否真的会处理拼音输入（总开关开着 **且** 处于中文态）
    pub fn is_chinese_active(&self) -> bool {
        self.enabled && self.lang == Lang::Chinese
    }

    /// 切换总开关（`Ctrl+Space`）。返回需要在关闭前上屏的内容。
    ///
    /// **关闭时上屏的是原始拼音字母，不是候选词。**
    /// 理由：在拼字过程中去关输入法，典型场景是「本来想打英文，忘了先关输入法」——
    /// 这时用户想要的是刚敲下的那串字母（比如 `nihao` 想表达某个缩写），
    /// 而不是被替换成「你好」。丢掉或替换都比保留原字母更让人恼火。
    ///
    /// 开启时不返回内容（此时缓冲本来就该是空的）。
    ///
    /// ⚠️ **开启时必须把 `lang` 复位成中文。**
    /// 否则「Shift 切到英文 → `Ctrl+Space` 关 → `Ctrl+Space` 开」会停在英文：
    /// 输入法明明"开着"，字母却全部直通，**拼音一个字也打不出来**。
    /// 这是实测踩到的坑（日志里还能看到那行骗人的「开启（中文）」）——
    /// 用户按 `Ctrl+Space` 的语义就是"我要用中文输入法"，停在英文没有道理。
    pub fn toggle_enabled(&mut self) -> Option<String> {
        let pending = if self.enabled {
            self.take_raw() // 原始拼音，不是候选
        } else {
            None
        };
        self.enabled = !self.enabled;
        self.clear();
        if self.enabled {
            self.lang = Lang::Chinese;
        }
        // 按 `Ctrl+Space` 时若 Shift 还按着，不算"单独按 Shift"，
        // 否则松手时会**额外**再切一次中英（把刚复位的中文又切回英文）。
        self.note_other_key();
        pending
    }

    /// 当前状态的快照，供「按应用记住状态」存回各自那一格。
    pub fn state(&self) -> ClientState {
        ClientState {
            enabled: self.enabled,
            lang: self.lang,
        }
    }

    /// 切到另一个应用：套用它的状态，并**清空**当前输入。
    ///
    /// 清空是必须的 —— 上一个应用没拼完的 `zhe` 跟过来只会让人困惑。
    /// （合成器失焦时已经让应用丢掉 preedit 了，缓冲若还留着两边就对不上。）
    ///
    /// 同时抹掉「Shift 单独按」的待定状态：切应用时用户很可能正按着 Shift，
    /// 松手时不该被算成一次中英切换。
    pub fn restore(&mut self, state: ClientState) {
        self.enabled = state.enabled;
        self.lang = state.lang;
        self.clear();
        self.note_other_key();
    }

    #[allow(dead_code)] // M5 翻页/状态显示会用到
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// 显示用的 preedit：**已定的中文** + 还没定的拼音（音节间加 `'`）。
    ///
    /// `zheli` 里选了「这」之后显示 `这li`，而不是 `zhe'li` ——
    /// 用户得看得见自己刚才那一下选中了什么。
    pub fn preedit_display(&self) -> String {
        let mut s = self.fixed_text();
        s.push_str(&self.engine.display_preedit(&self.buffer));
        s
    }

    // ── 缓冲编辑 ──

    /// 追加一个字母。返回是否真的吃下了（非 `[a-z]` 返回 false 让调用方转发）。
    pub fn push_letter(&mut self, ch: char) -> bool {
        if !ch.is_ascii_alphabetic() {
            return false;
        }
        // 大写也接受（用户可能开着 CapsLock），统一转小写
        self.buffer.push(ch.to_ascii_lowercase());
        self.refresh();
        true
    }

    /// 退格。缓冲删空了就**撤销上一次分步选词**，把那段退回成拼音。
    ///
    /// 不这么做的话，定了前缀之后退格会先删光剩下的拼音、然后卡住 ——
    /// 用户盯着屏幕上的「这」却删不掉它，只能 `Esc` 全丢。
    pub fn pop_letter(&mut self) {
        if self.buffer.pop().is_none() {
            if let Some((pinyin, _)) = self.fixed.pop() {
                self.buffer = pinyin;
            }
        }
        self.refresh();
    }

    /// 中文标点映射。返回 `None` = 这个字符不映射，调用方应当原样直通。
    ///
    /// **只在中文态调用**（`is_chinese_active()`）—— 英文态与总开关关闭时，
    /// 用户明确要打英文标点，映射过去只会碍事。
    ///
    /// 引号是**成对交替**的：`"` → `“` → `”` → `“` …… 固定给其中一个必然有
    /// 一半是错的。状态在 `clear()`（即一次上屏之后）复位，于是新的一句从开引号
    /// 开始。
    ///
    /// 其余按键取自常见中文输入法的默认表；`@ # % & *` 等不映射 ——
    /// 它们在中文里也写作半角，映射过去反而不对。
    pub fn punctuation(&mut self, ch: char) -> Option<&'static str> {
        Some(match ch {
            ',' => "，",
            '.' => "。",
            '?' => "？",
            '!' => "！",
            ';' => "；",
            ':' => "：",
            '\\' => "、",
            '<' => "《",
            '>' => "》",
            '(' => "（",
            ')' => "）",
            '[' => "【",
            ']' => "】",
            '{' => "「",
            '}' => "」",
            '_' => "——",
            '^' => "……",
            '$' => "￥",
            '~' => "～",
            '\'' => {
                let open = self.next_quote_open[0];
                self.next_quote_open[0] = !open;
                if open {
                    "‘"
                } else {
                    "’"
                }
            }
            '"' => {
                let open = self.next_quote_open[1];
                self.next_quote_open[1] = !open;
                if open {
                    "“"
                } else {
                    "”"
                }
            }
            _ => return None,
        })
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.fixed.clear();
        self.candidates.clear();
        self.selected = 0;
        // 一次上屏之后重新从开引号数起
        self.next_quote_open = [true, true];
    }

    /// 选中项右移 / 左移（翻页用，M5 会加真正的翻页）
    #[allow(dead_code)]
    pub fn select_next(&mut self) {
        if !self.candidates.is_empty() {
            self.selected = (self.selected + 1) % self.candidates.len();
        }
    }
    #[allow(dead_code)]
    pub fn select_prev(&mut self) {
        if !self.candidates.is_empty() {
            self.selected = (self.selected + self.candidates.len() - 1) % self.candidates.len();
        }
    }

    /// 选中第 `idx` 条候选（0-based）—— **分步选词**的核心。
    ///
    /// - **覆盖了整个输入** → 上屏（[`Choice::Commit`]）。
    /// - **只覆盖了一部分** → 把它"定"为前缀，把用掉的那几个字母从缓冲里切走，
    ///   剩下的拼音继续出候选（[`Choice::FixedPrefix`]）。
    ///
    /// 例：`zheli` 选「这」→ 缓冲变成 `li`，preedit 显示 `这li`，
    /// 候选重新按 `li` 出；接着选「里」才上屏「这里」。
    ///
    /// 为什么不直接上屏「这」：那等于替用户做了"这句就打这一个字"的决定。
    /// 用户敲了 `zheli` 却先选了「这」，几乎总是想接着选后面的。
    pub fn choose(&mut self, idx: usize) -> Choice {
        let Some(c) = self.candidates.get(idx) else {
            return Choice::Miss;
        };
        // 完整覆盖 → 交给 take_selected 上屏
        if c.consumed >= self.buffer.len() {
            self.selected = idx;
            return match self.take_selected() {
                Some(text) => Choice::Commit(text),
                None => Choice::Miss, // 走不到：idx 刚查过
            };
        }
        let (word, consumed) = (c.word.clone(), c.consumed);
        let pinyin = self.buffer[..consumed].to_string();
        self.buffer = self.buffer[consumed..].to_string();
        self.fixed.push((pinyin, word));
        self.refresh();
        Choice::FixedPrefix
    }

    /// `Space`：选中当前高亮的那条 —— 与数字键走**同一条路**。
    ///
    /// 不让空格直接上屏高亮项，是因为若高亮项只覆盖一部分输入，
    /// `take_selected()` 会把**剩下的拼音连同缓冲一起丢掉**（`clear()` 不区分）。
    /// 眼下完整覆盖的候选一定排在部分覆盖的前面（`COMPLETE_BONUS`），
    /// 正常打字碰不到；但那是排序的**经验结果**，"会不会丢字"不该依赖它。
    pub fn choose_selected(&mut self) -> Choice {
        self.choose(self.selected)
    }

    /// 取当前选中项（用于上屏）。**带上已定的前缀** —— 分步选词定的那几段
    /// 一直没交给应用，正是等到这一刻一起交出去。
    pub fn take_selected(&mut self) -> Option<String> {
        let c = self.candidates.get(self.selected)?.word.clone();
        let text = self.fixed_text() + &c;
        self.clear();
        Some(text)
    }

    /// 把**原始拼音**直接上屏（用户想输入英文串时）。
    ///
    /// 已定的前缀照样带上：`zheli` 里定了「这」再按回车，得到 `这li` 而不是
    /// `li` —— 那几段是用户已经选过的，不该因为退出拼音而丢掉。
    ///
    /// 与 [`take_selected`](Self::take_selected) 一样算**一次上屏**，
    /// 所以引号状态也要复位 —— 否则「上屏一次」的语义就分成了两套。
    pub fn take_raw(&mut self) -> Option<String> {
        if !self.is_composing() {
            return None;
        }
        let text = self.fixed_text() + &self.buffer;
        self.clear();
        Some(text)
    }

    /// 重算候选
    fn refresh(&mut self) {
        self.selected = 0;
        if self.buffer.is_empty() {
            self.candidates.clear();
            return;
        }
        self.candidates = self.engine.candidates(&self.buffer, DEFAULT_TOP_N);
    }

    // ── 按键消费判定 ──

    /// 标记某键已被消费（按下时调）
    pub fn mark_consumed(&mut self, keycode: u32) {
        self.consumed.insert(keycode);
    }

    /// 释放时：若该键的按下被消费过，则释放也要消费掉。返回 `true` = 消费。
    pub fn consume_release(&mut self, keycode: u32) -> bool {
        self.consumed.remove(&keycode)
    }

    /// 还有哪些键处于「我们消费中」（诊断用）
    #[allow(dead_code)]
    pub fn consumed_count(&self) -> usize {
        self.consumed.len()
    }

    // ── Shift 单独按 = 切中英 ──

    /// Shift 按下。记录时间戳以便判断是否"单独按"。
    pub fn shift_pressed(&mut self, keycode: u32) {
        self.shift_alone = Some(keycode);
    }

    /// 有其他键按下 → 说明 Shift 不是单独按的
    pub fn note_other_key(&mut self) {
        self.shift_alone = None;
    }

    /// Shift 释放。若期间没有其他键**且总开关开着**，则按一下 Shift 有**两种**语义：
    ///
    /// - **拼字中** → 上屏**原始拼音**，与 Enter 完全一样（[`ShiftTap::Raw`]）。
    ///   用户按 Shift 的意图是"这段拼音就这样吧"，不是"换成英文"——
    ///   顺手把语言切了的话，下一句拼音就打不出来了。
    /// - **没拼字** → 切换中英（[`ShiftTap::Lang`]）。
    ///
    /// 于是「拼字中连按两下 Shift」= 先原样上屏、再切到英文，正好是
    /// "我要打英文"的完整出口，不用先去按 `Ctrl+Space`。
    ///
    /// 总开关关着时 Shift 就是普通 Shift —— 否则用户在英文输入下
    /// 随手按一下 Shift 就会莫名其妙地激活输入法。
    pub fn shift_released(&mut self, keycode: u32) -> ShiftTap {
        if self.shift_alone != Some(keycode) {
            return ShiftTap::Nothing;
        }
        self.shift_alone = None;
        if !self.enabled {
            return ShiftTap::Nothing;
        }
        if let Some(raw) = self.take_raw() {
            return ShiftTap::Raw(raw);
        }
        self.lang = match self.lang {
            Lang::Chinese => Lang::English,
            Lang::English => Lang::Chinese,
        };
        // 切换时清空缓冲，避免残留
        self.clear();
        ShiftTap::Lang
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 刚建出来的状态 —— **默认关闭**，与合成器启动后的实际情况一致。
    fn composer() -> Composer {
        let (bytes, _) = pinyin_engine::build_dict([
            ("你", "ni", 3_000_000u32),
            ("你好", "ni hao", 480_000),
            ("是", "shi", 33_000_000),
        ])
        .expect("建表失败");
        Composer::new(Engine::from_bytes(&bytes).expect("加载失败"))
    }

    /// 已唤醒（`Ctrl+Space` 按过一次）的状态。
    ///
    /// 测试里**显式**唤醒，不依赖默认值 —— 否则哪天默认值再变，
    /// 一堆与"默认态"无关的测试会莫名其妙地红，且看不出为什么。
    fn composer_awake() -> Composer {
        let mut c = composer();
        c.toggle_enabled();
        assert!(c.is_enabled(), "toggle 一次应当开启");
        c
    }

    /// 合成器刚起来时输入法是**关闭**的 —— 此时字母必须直通，
    /// 否则刚登录敲命令会被吞进拼音缓冲。
    #[test]
    fn starts_disabled() {
        let c = composer();
        assert!(!c.is_enabled(), "启动时应当是关闭的");
        assert!(!c.is_chinese_active(), "关闭态不该处理拼音");
    }

    /// **开启总开关必须回到中文。**
    ///
    /// 实测踩到的坑：Shift 切到英文 → `Ctrl+Space` 关 → `Ctrl+Space` 开，
    /// 停在英文，字母全部直通，表现为「输入法开着却打不出拼音」，
    /// 而日志还照打「开启（中文）」—— 排查时被误导了很久。
    #[test]
    fn enabling_always_returns_to_chinese() {
        let mut c = composer_awake();

        c.shift_pressed(42);
        assert_eq!(
            c.shift_released(42),
            ShiftTap::Lang,
            "没拼字时单独按 Shift 应切中英"
        );
        assert_eq!(c.lang, Lang::English);
        assert!(!c.is_chinese_active(), "英文态不该处理拼音");

        c.toggle_enabled();
        assert!(!c.is_enabled());
        c.toggle_enabled();
        assert!(c.is_enabled());
        assert!(
            c.is_chinese_active(),
            "重新开启后必须是中文，实际 {:?}",
            c.lang
        );
    }

    /// 关闭时上屏的是**原始拼音字母**，不是候选词。
    /// 场景：想打英文但忘了先关输入法，用户要的是那串字母本身。
    #[test]
    fn disabling_commits_raw_pinyin_not_candidate() {
        let mut c = composer_awake();
        for ch in "nihao".chars() {
            assert!(c.push_letter(ch));
        }
        let pending = c.toggle_enabled().expect("应返回待上屏内容");
        assert_eq!(pending, "nihao", "应是原始拼音，不是「你好」");
        assert!(!c.is_composing(), "关闭后缓冲应清空");
    }

    /// 按 `Ctrl+Space` 时若 Shift 还按着，松手不该**再**切一次中英 ——
    /// 否则刚复位的中文会被立刻切回英文，症状与上一条一模一样。
    #[test]
    fn ctrl_space_cancels_pending_shift_toggle() {
        let mut c = composer_awake();
        c.shift_pressed(42);
        c.toggle_enabled(); // 期间按了别的键
        c.toggle_enabled(); // 再开回来 → 中文
        assert_eq!(
            c.shift_released(42),
            ShiftTap::Nothing,
            "Ctrl+Space 已取消「单独按 Shift」，不该再切"
        );
        assert!(c.is_chinese_active(), "应仍是中文");
    }

    // ─────────── 中文标点 ───────────

    /// 基本映射：常见的那些，逐个钉住
    #[test]
    fn punctuation_maps_the_common_set() {
        let mut c = composer_awake();
        for (from, want) in [
            (',', "，"),
            ('.', "。"),
            ('?', "？"),
            ('!', "！"),
            (';', "；"),
            (':', "："),
            ('\\', "、"),
            ('<', "《"),
            ('>', "》"),
            ('(', "（"),
            (')', "）"),
            ('[', "【"),
            (']', "】"),
            ('{', "「"),
            ('}', "」"),
            ('_', "——"),
            ('^', "……"),
            ('$', "￥"),
            ('~', "～"),
        ] {
            assert_eq!(c.punctuation(from), Some(want), "{from} 应映射为 {want}");
        }
    }

    /// 引号必须**成对交替** —— 固定给一个必然有一半是错的
    #[test]
    fn quotes_alternate() {
        let mut c = composer_awake();
        assert_eq!(c.punctuation('"'), Some("“"));
        assert_eq!(c.punctuation('"'), Some("”"));
        assert_eq!(c.punctuation('"'), Some("“"));
        assert_eq!(c.punctuation('\''), Some("‘"));
        assert_eq!(c.punctuation('\''), Some("’"));
        // 单双引号各自独立计数
        assert_eq!(c.punctuation('"'), Some("”"));
    }

    /// 一次上屏之后引号状态复位，新的一句从开引号开始
    #[test]
    fn quotes_reset_after_commit() {
        let mut c = composer_awake();
        assert_eq!(c.punctuation('"'), Some("“"));
        assert_eq!(c.punctuation('"'), Some("”"));
        c.clear(); // take_selected() / 切总开关都会走到这里
        assert_eq!(c.punctuation('"'), Some("“"), "clear 之后应回到开引号");
    }

    /// `take_raw()` 同样算一次上屏，引号状态也要复位 ——
    /// 否则「上屏」的语义会分成两套，`Shift`/`Enter` 上屏后引号还是反的。
    #[test]
    fn quotes_reset_after_raw_commit() {
        let mut c = composer_awake();
        c.push_letter('n');
        assert_eq!(c.punctuation('"'), Some("“"));
        assert_eq!(c.punctuation('"'), Some("”"));
        assert_eq!(c.take_raw(), Some("n".to_string()));
        assert_eq!(c.punctuation('"'), Some("“"), "take_raw 之后应回到开引号");
    }

    /// 不该映射的字符返回 None，让调用方原样直通。
    ///
    /// `@ # % & *` 在中文里也写作半角，映射过去反而不对。
    #[test]
    fn punctuation_leaves_the_rest_alone() {
        let mut c = composer_awake();
        for ch in ['@', '#', '%', '&', '*', '-', '+', '=', '/', 'a', '1', '中'] {
            assert_eq!(c.punctuation(ch), None, "{ch} 不该被映射");
        }
    }

    /// 总开关关着时 Shift 就是普通 Shift，不该激活输入法。
    ///
    /// 现在这就是**启动后的默认状态**，所以这条比过去更要紧：
    /// 刚起来时随手按一下 Shift（打大写）不该把输入法弄开。
    #[test]
    fn shift_is_inert_while_disabled() {
        let mut c = composer();
        assert!(!c.is_enabled(), "前置条件：默认关闭");
        c.shift_pressed(42);
        assert_eq!(
            c.shift_released(42),
            ShiftTap::Nothing,
            "关闭态下 Shift 不该切换中英"
        );
        assert!(!c.is_enabled(), "Shift 不该把它打开");
    }

    // ─────────── 单独按 Shift：拼字中 = Enter ───────────

    /// **拼字中按一下 Shift = 按一下 Enter** —— 把已输入的部分原样上屏。
    ///
    /// 且**不切中英**：用户按 Shift 是"这段拼音就这样吧"，顺手切成英文的话
    /// 下一句拼音就打不出来了。
    #[test]
    fn shift_tap_commits_raw_while_composing() {
        let mut c = composer_awake();
        for ch in "nihao".chars() {
            assert!(c.push_letter(ch));
        }
        assert!(c.is_composing(), "前置条件：正在拼字");

        c.shift_pressed(42);
        assert_eq!(
            c.shift_released(42),
            ShiftTap::Raw("nihao".to_string()),
            "拼字中单独按 Shift 应原样上屏，与 Enter 相同"
        );
        assert!(!c.is_composing(), "上屏后缓冲应清空");
        assert_eq!(c.lang, Lang::Chinese, "不该顺手切成英文");
    }

    /// 拼字中连按两下 Shift：第一下上屏原始拼音，第二下才切中英。
    /// 这就是"我要打英文"的完整出口。
    #[test]
    fn two_shift_taps_commit_then_switch() {
        let mut c = composer_awake();
        for ch in "shi".chars() {
            c.push_letter(ch);
        }

        c.shift_pressed(42);
        assert_eq!(c.shift_released(42), ShiftTap::Raw("shi".to_string()));
        assert_eq!(c.lang, Lang::Chinese);

        c.shift_pressed(42);
        assert_eq!(c.shift_released(42), ShiftTap::Lang);
        assert_eq!(c.lang, Lang::English);
    }

    /// 上屏的是**原始拼音**而不是候选词 —— 与 Enter 的行为对齐
    /// （`shi` 的候选里有「是」，这里要的是 `shi` 本身）。
    #[test]
    fn shift_tap_does_not_take_the_candidate() {
        let mut c = composer_awake();
        for ch in "shi".chars() {
            c.push_letter(ch);
        }
        assert!(!c.candidates().is_empty(), "前置条件：有候选");

        c.shift_pressed(42);
        assert_eq!(c.shift_released(42), ShiftTap::Raw("shi".to_string()));
    }

    /// Shift 期间敲了别的键 → 不算"单独按"，松手时什么也不做。
    /// 这是 `Shift + 字母 = 直接输入大写字母` 的前提：
    /// 那条路径必须**不**触发上屏/切语言。
    #[test]
    fn shift_with_another_key_does_nothing_on_release() {
        let mut c = composer_awake();
        for ch in "ni".chars() {
            c.push_letter(ch);
        }

        c.shift_pressed(42);
        c.note_other_key(); // 按住 Shift 期间敲了字母
        assert_eq!(
            c.shift_released(42),
            ShiftTap::Nothing,
            "Shift 不是单独按的，松手不该有动作"
        );
        assert!(c.is_composing(), "缓冲应原封不动，由调用方决定怎么处理");
        assert_eq!(c.lang, Lang::Chinese);
    }

    // ─────────── 分步选词 ───────────

    /// 分步选词专用词典 + 已唤醒。
    ///
    /// 需要「这里」（整词）与「这」（只吃 `zhe`）同时存在，
    /// 还要有「里」来接住剩下的 `li`。
    fn composer_split() -> Composer {
        let (bytes, _) = pinyin_engine::build_dict([
            ("这", "zhe", 20_000_000u32),
            ("这里", "zhe li", 500_000),
            ("里", "li", 20_000_000),
            ("你", "ni", 30_000_000),
            ("你好", "ni hao", 480_000),
        ])
        .expect("建表失败");
        let mut c = Composer::new(Engine::from_bytes(&bytes).expect("加载失败"));
        c.toggle_enabled();
        assert!(c.is_enabled(), "前置条件：已唤醒");
        c
    }

    /// 候选里 `word` 的序号（0-based）
    fn index_of(c: &Composer, word: &str) -> usize {
        c.candidates()
            .iter()
            .position(|x| x.word == word)
            .unwrap_or_else(|| {
                let have: Vec<&str> = c.candidates().iter().map(|x| x.word.as_str()).collect();
                panic!("候选里没有「{word}」，只有 {have:?}")
            })
    }

    fn type_str(c: &mut Composer, s: &str) {
        for ch in s.chars() {
            assert!(c.push_letter(ch), "{ch} 应被吃进拼音缓冲");
        }
    }

    /// **只覆盖一部分输入的候选 → 定为前缀，不直接上屏。**
    /// 这就是 `zheli` 里选「这」的行为。
    #[test]
    fn partial_candidate_becomes_prefix() {
        let mut c = composer_split();
        type_str(&mut c, "zheli");

        let i = index_of(&c, "这");
        assert_eq!(
            c.choose(i),
            Choice::FixedPrefix,
            "「这」只吃 zhe，该定为前缀而不是上屏"
        );
        assert_eq!(c.buffer(), "li", "用掉的 zhe 该从缓冲里切走");
        assert_eq!(c.preedit_display(), "这li", "preedit = 已定的中文 + 剩余拼音");
        assert!(c.is_composing(), "还没交给应用，仍算输入中");
    }

    /// 定完前缀再选剩下的 → **一次上屏整段**
    #[test]
    fn prefix_is_committed_together_with_the_rest() {
        let mut c = composer_split();
        type_str(&mut c, "zheli");
        let i = index_of(&c, "这");
        assert_eq!(c.choose(i), Choice::FixedPrefix);

        // 缓冲现在是 li，候选只剩「里」这一档
        assert_eq!(
            c.candidates().iter().map(|x| x.word.as_str()).collect::<Vec<_>>(),
            vec!["里"],
            "定完前缀后候选该重新按剩下的拼音出"
        );
        assert_eq!(
            c.choose(0),
            Choice::Commit("这里".to_string()),
            "前缀与剩下的该**一起**上屏"
        );
        assert!(!c.is_composing());
    }

    /// 完整覆盖的候选照旧直接上屏 —— 分步选词不该影响正常路径
    #[test]
    fn full_candidate_still_commits_directly() {
        let mut c = composer_split();
        type_str(&mut c, "zheli");
        let i = index_of(&c, "这里");
        assert_eq!(c.choose(i), Choice::Commit("这里".to_string()));
        assert!(!c.is_composing());
    }

    /// 序号越界返回 `Miss`，不该动状态
    #[test]
    fn choosing_out_of_range_is_a_miss() {
        let mut c = composer_split();
        type_str(&mut c, "zheli");
        assert_eq!(c.choose(99), Choice::Miss);
        assert_eq!(c.buffer(), "zheli", "未命中不该改缓冲");
    }

    /// **空格与数字键同路**：高亮项只覆盖一部分输入时同样只定前缀。
    ///
    /// 否则 `take_selected()` 会把剩下的 `li` 连同缓冲一起丢掉 ——
    /// 用户看着「这」上屏，敲的 `li` 却没了。
    #[test]
    fn space_also_fixes_partial_candidates() {
        let mut c = composer_split();
        type_str(&mut c, "zheli");
        // 正常排序下「这」在整词后面，这里把它调到高亮位
        let i = index_of(&c, "这");
        for _ in 0..i {
            c.select_next();
        }
        assert_eq!(c.selected(), i, "前置条件：高亮项是「这」");

        assert_eq!(c.choose_selected(), Choice::FixedPrefix);
        assert_eq!(c.buffer(), "li", "剩下的拼音不该被丢掉");
    }

    /// 已定的前缀**不能丢**：回车（原样上屏）时要带上，
    /// 否则用户选好的字会因为"退出拼音"而消失。
    #[test]
    fn raw_commit_keeps_the_fixed_prefix() {
        let mut c = composer_split();
        type_str(&mut c, "zheli");
        let i = index_of(&c, "这");
        assert_eq!(c.choose(i), Choice::FixedPrefix);

        assert_eq!(c.take_raw(), Some("这li".to_string()));
        assert!(!c.is_composing());
    }

    /// 退格删空缓冲后，**再退格撤销上一次分步选词**，把那段退回成拼音。
    /// 不这么做的话用户盯着屏幕上的「这」却删不掉。
    #[test]
    fn backspace_undoes_a_fix() {
        let mut c = composer_split();
        type_str(&mut c, "zheli");
        let i = index_of(&c, "这");
        assert_eq!(c.choose(i), Choice::FixedPrefix);
        assert_eq!(c.buffer(), "li");

        c.pop_letter(); // li → l
        assert_eq!(c.buffer(), "l");
        c.pop_letter(); // l → ""
        assert_eq!(c.buffer(), "");
        c.pop_letter(); // 缓冲空了 → 撤销那次选词
        assert_eq!(c.buffer(), "zhe", "该把「这」退回成拼音");
        assert_eq!(c.preedit_display(), "zhe", "已定的字也该跟着消失");

        // 再退一格是普通的删字母，不该再往回退
        c.pop_letter();
        assert_eq!(c.buffer(), "zh");
    }

    /// `Esc` 清空 = 整段取消，已定的前缀也不留
    #[test]
    fn clear_drops_the_fixed_prefix() {
        let mut c = composer_split();
        type_str(&mut c, "zheli");
        let i = index_of(&c, "这");
        assert_eq!(c.choose(i), Choice::FixedPrefix);

        c.clear();
        assert!(!c.is_composing());
        assert_eq!(c.preedit_display(), "");
    }

    // ─────────── 按应用记住状态 ───────────

    /// 新应用一律「关闭 + 中文」—— 不能一上来就吃字母
    #[test]
    fn default_client_state_is_off_and_chinese() {
        let s = ClientState::default();
        assert!(!s.enabled, "新应用默认关闭");
        assert_eq!(s.lang, Lang::Chinese);
    }

    /// 快照 / 恢复要完整带回「总开关 **和** 中英」——
    /// 只带总开关的话，在 A 里 Shift 切到英文、切回来就丢了
    #[test]
    fn state_round_trips_enabled_and_lang() {
        let mut c = composer_awake();

        // 弄成「开启 + 英文」
        c.shift_pressed(42);
        assert_eq!(c.shift_released(42), ShiftTap::Lang);
        assert!(c.is_enabled());
        assert_eq!(c.lang, Lang::English);

        let saved = c.state();
        assert!(saved.enabled);
        assert_eq!(saved.lang, Lang::English);

        // 换成另一个应用的状态
        c.restore(ClientState::default());
        assert!(!c.is_enabled());
        assert_eq!(c.lang, Lang::Chinese);

        // 切回来
        c.restore(saved);
        assert_eq!(c.state(), saved, "恢复后应回到存档时的状态");
        assert!(c.is_enabled(), "存档是「开启」");
        assert_eq!(c.lang, Lang::English, "存档是「英文」");
        assert!(
            !c.is_chinese_active(),
            "「开启 + 英文」不是中文态，不该能打拼音"
        );

        // 中文态的存档也要原样回来（这条才是「切回来还能打中文」）
        let chinese = ClientState {
            enabled: true,
            lang: Lang::Chinese,
        };
        c.restore(chinese);
        assert_eq!(c.state(), chinese);
        assert!(c.is_chinese_active(), "恢复中文态后应该能打拼音");
    }

    /// 切应用要**清空**未拼完的拼音 —— 上一个应用没打完的 `zhe` 不该跟过来
    #[test]
    fn restore_clears_the_buffer() {
        let mut c = composer_awake();
        type_str(&mut c, "zheli");
        assert!(c.is_composing());

        c.restore(ClientState::default());
        assert!(!c.is_composing(), "缓冲该被清掉");
        assert_eq!(c.preedit_display(), "");
    }

    /// `restore` 要抹掉「Shift 单独按」的待定状态 ——
    /// 切应用时用户很可能正按着 Shift，松手不该被算成一次中英切换
    #[test]
    fn restore_cancels_a_pending_shift_tap() {
        let mut c = composer_awake();
        c.shift_pressed(42);
        c.restore(ClientState::default());
        assert_eq!(
            c.shift_released(42),
            ShiftTap::Nothing,
            "切应用时按着的 Shift，松手不该触发中英切换"
        );
    }
}
