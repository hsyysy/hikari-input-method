//! `zwp_input_method_v2` / `zwp_virtual_keyboard_v1` 的 Wayland 绑定。
//!
//! # 为什么这两个协议要 vendor
//!
//! 它们**不在任何已安装的协议包里**：
//!
//! - `wlr-protocols` r112 已移除 `input-method-v2` 与 `virtual-keyboard`
//! - `wayland-protocols` 1.49 从未包含 `input-method-v2`
//! - `wayland-protocols-wlr` 0.3.12 也不含（已解包确认）
//!
//! 权威副本位于 **wlroots 仓库根目录的 `protocol/`**。本 crate 内的 XML
//! 从本机 wlroots 源码拷贝，确保与运行中的合成器版本一致。
//!
//! # 用法要点
//!
//! - `zwp_input_method_keyboard_grab_v2` **不是 global**，由 `grab_keyboard()` 请求产生
//! - 虚拟键盘**必须先收到 keymap**，否则发按键会触发
//!   `ZWP_VIRTUAL_KEYBOARD_V1_ERROR_NO_KEYMAP` 并**断开连接**（§5.3）
//! - `commit(serial)` 的 serial = **收到的 `done` 事件计数**（§2.1）
//!
//! # 与「原始规范」的差异
//!
//! wlroots 的这份 XML 在 keyboard grab 的 `key` / `modifiers` 事件上**多了 `serial` 参数**
//! （`wlr_input_method_v2.c:302-319` 用 `wlr_seat_client_next_serial` 填充）。
//! 这是 wlroots 侧的扩展；本 crate 以 vendored XML 为准，故 Rust 绑定里也会出现该字段。
//! 目标合成器是 hikari（wlroots 0.21），以它为准。

/// `zwp_input_method_v2` 及配套对象。
///
/// 含 `zwp_input_method_v2` / `zwp_input_popup_surface_v2` /
/// `zwp_input_method_keyboard_grab_v2` / `zwp_input_method_manager_v2`。
pub mod input_method {
    use wayland_client;
    use wayland_client::protocol::*;

    // ⚠️ 必需：input-method-v2 的 `content_type` / `text_change_cause` 事件引用了
    // `zwp_text_input_v3` 的枚举（见文件末尾说明），wayland-scanner 生成的代码会以
    // `super::zwp_text_input_v3::…` 引用它 —— 不放进来会**编译失败**。
    // 这也从 codegen 层面证实了 §8 的判断：要的是它的**枚举**，不是它的接口。
    use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocol/input-method-unstable-v2.xml");
    }

    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("./protocol/input-method-unstable-v2.xml");

    // 生成的类型在 `<接口名>` 子模块内，这里转发导出以便直接使用
    pub use self::zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2;
    pub use self::zwp_input_method_manager_v2::ZwpInputMethodManagerV2;
    pub use self::zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2;
    pub use self::zwp_input_method_v2::ZwpInputMethodV2;
}

/// hikari 私有协议：告诉输入法**当前聚焦的是哪个客户端**。
///
/// # 为什么需要它
///
/// `zwp_input_method_v2` 的 `activate` / `deactivate` **不带任何参数**，
/// 输入法只知道「有个文本输入要你了」，不知道**是谁**。而本输入法要按应用
/// 记住状态（在 A 里开了中文，切到 B 不该跟着开），所以这个身份只能由合成器补。
///
/// 这不是标准协议，**只有 hikari 实现它**。因此这个 global 是可选的：
/// bind 不到就退化成「全局一个开关」，行为和以前完全一样。见 `main.rs` 里
/// `switch_client` / `forget_client` 的用法。
///
/// # 两条要小心的行为（协议 XML 里写明了）
///
/// - **`activate` 可能没有前置的 `focused_client`** —— 合成器分配 id 失败
///   （OOM 或 id 用尽）时会这样。此时输入法拿不到新身份，只能沿用当前状态。
/// - **`client_gone` 可能带一个从没见过的 id** —— 比如 id 是在本对象 bind
///   之前分配的。清理未知状态必须是 no-op。
///
/// 与本仓库另外两个模块不同，这个 XML 的权威副本在 **hikari 仓库**的
/// `protocol/hikari-input-method-v1.xml`，这里是从那边拷的，两边要保持一致。
pub mod hikari_input_method {
    use wayland_client;

    // 与另外两个模块不同，这里**不需要**引入 `wayland_client::protocol::*`
    // 和 `__interfaces::*` —— 本协议不引用 `wl_surface` 之类的外部接口，
    // 没有跨接口参数，生成的代码用不到它们（引入了反而报 unused import）。
    pub mod __interfaces {
        wayland_scanner::generate_interfaces!("./protocol/hikari-input-method-v1.xml");
    }

    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("./protocol/hikari-input-method-v1.xml");

    pub use self::hikari_input_method_v1::HikariInputMethodV1;
}

/// `zwp_virtual_keyboard_v1` —— 把按键还给应用的唯一途径。
pub mod virtual_keyboard {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocol/virtual-keyboard-unstable-v1.xml");
    }

    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("./protocol/virtual-keyboard-unstable-v1.xml");

    pub use self::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
    pub use self::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
}

/// `zwp_input_method_v2` 事件/请求里引用的枚举，来自 `zwp_text_input_v3`。
///
/// **注意**：输入法**从不创建 `zwp_text_input_v3` 对象**（那是应用侧的协议，
/// 由合成器代为交互），但 `zwp_input_method_v2` 的事件**引用了它的枚举类型**：
///
/// ```text
/// input-method-unstable-v2.xml:170  <arg name="cause"   enum="zwp_text_input_v3.change_cause"/>
/// input-method-unstable-v2.xml:184  <arg name="hint"    enum="zwp_text_input_v3.content_hint"/>
/// input-method-unstable-v2.xml:185  <arg name="purpose" enum="zwp_text_input_v3.content_purpose"/>
/// ```
///
/// 所以这里要的是**枚举定义**，不是接口对象。`content_hint` 尤其重要——
/// §5.3 的密码框处理（识别 `sensitive_data`）依赖它。
pub mod text_input_enums {
    // 枚举定义在 `<接口名>` 子模块内，故路径是 client::zwp_text_input_v3::…
    pub use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{
        ChangeCause, ContentHint, ContentPurpose,
    };
}
