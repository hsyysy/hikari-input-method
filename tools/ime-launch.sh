#!/bin/sh
# 启动 ime-daemon，并在它退出后自动重启。
#
# 为什么需要重启循环：需要 `Restart=always` 的语义 ——
# 这个进程持有键盘 grab，挂掉后输入法就没了。hikari 的 autostart 本身不提供
# 重启，所以在脚本里补上。
#
# 用法:
#   ime-launch.sh            前台运行（供 hikari autostart 调用）
#   ime-launch.sh --daemon   后台运行（自用/测试）
#   ime-launch.sh --status   查看状态
#   ime-launch.sh --stop     停止
#
# 环境变量:
#   IME_BIN    指定二进制路径（默认找 target/release/ime-daemon）
#   IME_DICT   指定词典路径（默认找 data/dict/pinyin.bin）
#   IME_VERBOSE=1  传 -v 给 daemon（打印每个按键）

set -u

# 脚本所在仓库根目录（本脚本在 tools/ 下）
ROOT=$(cd "$(dirname "$0")/.." && pwd)

STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/ime"
PIDFILE="$STATE_DIR/daemon.pid"
LOG="$STATE_DIR/daemon.log"

mkdir -p "$STATE_DIR"

find_bin() {
    if [ -n "${IME_BIN:-}" ]; then
        echo "$IME_BIN"
    elif [ -x "$ROOT/target/release/ime-daemon" ]; then
        echo "$ROOT/target/release/ime-daemon"
    elif [ -x "$ROOT/target/debug/ime-daemon" ]; then
        echo "$ROOT/target/debug/ime-daemon"
    else
        echo ""
    fi
}

find_dict() {
    if [ -n "${IME_DICT:-}" ]; then
        echo "$IME_DICT"
    elif [ -f "$ROOT/data/dict/pinyin.bin" ]; then
        echo "$ROOT/data/dict/pinyin.bin"
    else
        echo ""
    fi
}

status() {
    if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
        echo "运行中，PID $(cat "$PIDFILE")"
        echo "日志: $LOG"
        return 0
    fi
    echo "未运行"
    return 1
}

stop() {
    if [ -f "$PIDFILE" ]; then
        pid=$(cat "$PIDFILE")
        if kill -0 "$pid" 2>/dev/null; then
            # 杀整个进程组：包装脚本与 daemon 都要停
            kill -TERM -"$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null
            sleep 1
            kill -KILL -"$pid" 2>/dev/null || true
            echo "已停止 (PID $pid)"
        fi
        rm -f "$PIDFILE"
    else
        # 没有 pidfile 时按名字兜底
        pkill -x ime-daemon 2>/dev/null && echo "已停止（按进程名）" || echo "未运行"
    fi
}

run_loop() {
    # 防重复启动：hikari 重启后 autostart 会再拉起一次，
    # 而此时旧的 launcher 可能还活着（它的 daemon 会因连接断开而重启重连）。
    # 两个 IM 实例时，第二个会被 hikari 用 `unavailable` 拒掉（§5.2），徒增困惑。
    if [ -f "$PIDFILE" ]; then
        old=$(cat "$PIDFILE" 2>/dev/null)
        if [ -n "$old" ] && [ "$old" != "$$" ] && kill -0 "$old" 2>/dev/null; then
            echo "已有实例在运行 (PID $old)，退出。日志: $LOG" >&2
            exit 0
        fi
    fi

    bin=$(find_bin)
    if [ -z "$bin" ]; then
        echo "找不到 ime-daemon —— 先构建：" >&2
        echo "  cargo build --release -p ime-daemon" >&2
        exit 1
    fi

    dict=$(find_dict)
    if [ -z "$dict" ]; then
        echo "找不到词典 —— 先跑 tools/fetch-dict.sh" >&2
        echo "（没有词典也能启动，但会退化为纯透传、无法输入中文）" >&2
    else
        export IME_DICT="$dict"
    fi

    verbose=""
    [ "${IME_VERBOSE:-0}" = "1" ] && verbose="-v"

    echo $$ >"$PIDFILE"
    trap 'rm -f "$PIDFILE"; exit 0' TERM INT

    echo "== ime-launch 启动 ==" >>"$LOG"
    echo "binary: $bin" >>"$LOG"
    echo "dict:   ${dict:-（无）}" >>"$LOG"
    echo "WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-（未设）}" >>"$LOG"

    while :; do
        start=$(date +%s)
        # shellcheck disable=SC2086
        "$bin" $verbose >>"$LOG" 2>&1
        code=$?
        end=$(date +%s)
        echo "[ime-launch] daemon 退出 code=$code，运行 $((end - start))s" >>"$LOG"

        # 退避：起来就挂（多半是配置问题）等久一点，别刷屏
        if [ $((end - start)) -lt 3 ]; then
            sleep 5
        else
            sleep 1
        fi
    done
}

case "${1:-}" in
    --stop) stop ;;
    --status) status ;;
    --daemon)
        # 后台运行：setsid 脱离当前会话，与 hikari autostart 的行为一致
        setsid "$0" >>"$LOG" 2>&1 &
        sleep 1
        status
        ;;
    "" | --foreground) run_loop ;;
    *)
        echo "用法: $0 [--daemon|--status|--stop]" >&2
        exit 1
        ;;
esac
