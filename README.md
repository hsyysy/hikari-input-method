# 纯 Wayland 拼音输入法

一个用 Rust 写的拼音输入法，**直接实现 `zwp_input_method_v2` 协议**，
不依赖 fcitx5 / ibus 之类的输入法框架。

目前**只支持 [hikari](https://github.com/hsyysy/hikari)**（及其它自行接线
IM relay 的合成器），不支持 X11 / XWayland 应用。

## 构建与运行

```sh
# 1. 构建
cargo build --release -p ime-daemon

# 2. 准备词典（首次需要，见下节）

# 3. 启动
tools/ime-launch.sh --daemon

# 4. 状态 / 停止
tools/ime-launch.sh --status
tools/ime-launch.sh --stop
```

启动后在任意支持 `text-input-v3` 的应用里打拼音即可。

## 词典

词典**不入库** —— 上游词库是 GPL-3.0，且编译产物有 25 MB。首次使用需要自己准备。

1. 下载两个 RIME 词库文件，放进 `data/dict/`：

   ```sh
   mkdir -p data/dict && cd data/dict
   BASE=https://raw.githubusercontent.com/iDvel/rime-ice/main/cn_dicts
   curl -LO $BASE/8105.dict.yaml   # 通用规范汉字表（单字）
   curl -LO $BASE/base.dict.yaml   # 基础词库
   ```

2. 编译成引擎用的 blob（只需一次）：

   ```sh
   cargo build --release -p dict-builder
   ./target/release/dict-builder data/dict/pinyin.bin \
       data/dict/8105.dict.yaml data/dict/base.dict.yaml
   ```

3. 验证：

   ```sh
   cargo run --release -p pinyin-engine --example query -- data/dict/pinyin.bin nihao
   ```

`dict-builder` 接受 RIME 格式的 yaml（`词<TAB>拼音<TAB>词频`），多个文件按顺序合并，
所以可以换用别的词库。

接入合成器自启动（以 hikari 为例，写进 `~/.config/hikari/autostart`）：

```sh
/path/to/input-method/tools/ime-launch.sh &
```

`tools/ime-launch.sh` 负责崩溃自动重启、退避、防重复启动，日志在
`${XDG_STATE_HOME:-~/.local/state}/ime/daemon.log`。

## 用法

**合成器启动时输入法是关闭的**，按 `Ctrl+Space` 唤醒。

| 按键 | 作用 |
|---|---|
| `Ctrl+Space` | 总开关：开 / 关输入法 |
| `Shift` + 字母 | 直接输入**大写字母**，不拼拼音（同英文态） |
| `Shift`（单独按一下） | 拼字中 = `Enter`（原样上屏）；没拼字 = 切换中英文 |
| `1`–`9` | 选中第 N 个候选（只覆盖一部分输入的会先「定」下来，见下） |
| `Space` | 上屏当前选中项 |
| `Enter` | 上屏**原始拼音**（想打英文串时的出口） |
| `Esc` | 清空当前拼音 |
| `BackSpace` | 删一个字母；拼音删空后撤销上一次分步选词 |

`Shift` 是"把拼音原样吐出来"的出口：拼字中单独按一下它，效果和 `Enter` 一样，
且**不会**顺手切成英文；再按一下（此时已没在拼字）才切中英。
所以「拼字中连按两下 Shift」= 原样上屏 + 切到英文。

**分步选词**：候选如果只覆盖了输入的一部分，按它的序号**不会直接上屏**，
而是把它「定」成前缀、继续选剩下的。比如 `zheli`：

```
zheli        → 这里  哲理  啫喱  …  这  着  者
                  ↑ 选这个（只吃掉 zhe）
这li         → 里    力    离    …
                  ↑ 再选这个
              → 上屏「这里」
```

候选只覆盖一部分时，它**一定排在完整覆盖的候选后面**（完整覆盖有排序奖励），
所以正常打字（一路按空格）不会误触发；只有主动去挑那个短候选才会走分步。
`BackSpace` 在拼音删空后会撤销上一次这样的选择、把它退回成拼音，`Esc` 则整段取消。

中文态下英文标点会映射成中文标点（`, ` → `，`、`.` → `。`、`?` → `？` …），
引号成对交替；拼字中按标点会先上屏候选再接标点（`nihao` + `,` → `你好，`）。

**与键盘布局无关**：实现用 `xkbcommon` 按语义判定修饰键与字符，源码里没有
硬编码键码，也不依赖任何特定的 xkb 选项。

## 当前限制

| 限制 | 说明 |
|---|---|
| 只支持 hikari | 需要合成器侧的 IM relay，见下 |
| XWayland 应用用不了 | relay 只对启用 `text-input-v3` 的客户端激活 |
| 只出前 9 个候选，无翻页 | |
| 只做前缀匹配，不做整句解码 | 输入 `woshi` 只能出「我」，出不了「我是」 |
| 不能与 fcitx5 并存 | 协议只允许一个输入法 |

## 与 hikari 的配合

本输入法依赖合成器提供 `zwp_input_method_manager_v2` 与
`zwp_virtual_keyboard_manager_v1`，并把 `text-input-v3` 客户端的事件中继过来。

所用的 hikari fork：<https://github.com/hsyysy/hikari>。该分支另有一组与本
输入法配合的改动（虚拟键盘不再抢占 seat 键盘以消除 keymap 广播风暴、候选窗
定位与贴边翻转等）。

## 仓库结构

```
crates/
  ime-protocol/    zwp_input_method_v2 / zwp_virtual_keyboard_v1 的协议绑定（vendored XML + codegen）
  ime-ui/          候选窗渲染：wl_shm 缓冲 + cosmic-text 排版 + 2x 超采样
  pinyin-engine/   拼音引擎：切分、查表、排序（mmap 词典）
  ime-daemon/      守护进程：grab 键盘、状态机、候选窗
tools/
  dict-builder/    把 RIME 词典转成引擎用的 blob（首次安装时用）
  ime-launch.sh    启动 / 重启 / 状态（运行时用）
```

