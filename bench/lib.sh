# bench/ のスクリプトが共通で使うもの (source して使う)。
#
# 画面を守るため、テストの fbservo はかならずここの start_fbservo で起動する。
#  - FBSERVO_FB でダミーのファイルを渡す。drmterm (偽物でも本物でも) につながらなかったとき、
#    fbservo は /dev/fb0 へ描こうとするが、内蔵画面には描かせない
#  - ユーザーのとは別の tmux サーバのペインで動かす
#  - 偽の drmterm は、ソケットができてから fbservo を起動する (wait_sock)

set -eu
BENCH=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(dirname "$BENCH")
FBSERVO=$REPO/target/release/fbservo
URL=${URL:-https://doc.rust-lang.org/std/}

# ソケットのパスは 108 バイトまでなので、短い場所に作る
T=$(mktemp -d "${TMPDIR:-/tmp}/fbb.XXXX")
KILL=""
cleanup() {
    tmux -S "$T/t.sock" kill-server 2>/dev/null || true
    tmux -S "$T/c.sock" kill-server 2>/dev/null || true
    for p in $KILL; do kill "$p" 2>/dev/null || true; done
    wait 2>/dev/null || true
    rm -rf "${T:?}"
}
trap cleanup EXIT

# 内蔵画面 (/dev/fb0) の代わりの書き込み先。fb0 と同じ大きさ
truncate -s $(($(cat /sys/class/graphics/fb0/stride) * $(cut -d, -f2 /sys/class/graphics/fb0/virtual_size))) "$T/fb0"

# 結果を置く場所 (bench/out/<名前>、前の中身は消す)
out_dir() {
    local d=$BENCH/out/$1
    mkdir -p "$d"
    rm -f "${d:?}"/*
    echo "$d"
}

wait_sock() {
    for _ in $(seq 100); do
        [ -S "$1" ] && return
        sleep 0.1
    done
    echo "$1 ができない" >&2
    exit 1
}

# 別の tmux サーバにセッション ext (160x44) を作り、さらに別のサーバの中から 160x45 のクライアントで
# つなぐ (どのクライアントにも映っていないペインには、fbservo は描かない)
start_tmux() {
    tmux -S "$T/t.sock" -f /dev/null new-session -d -s ext -x 160 -y 44 'sleep 3600'
    tmux -S "$T/c.sock" -f /dev/null new-session -d -x 160 -y 46 "env -u TMUX tmux -S $T/t.sock attach -t ext"
    sleep 0.3
}

# start_fbservo <環境変数=値...>: ext の左に 80 桁のペインを作って fbservo を動かし、ペインの番号を出す。
# ログは $T/fbservo.log
start_fbservo() {
    tmux -S "$T/t.sock" split-window -h -b -l 80 -t ext -c "$REPO" -P -F '#{pane_id}' \
        "env $* FBSERVO_FB=$T/fb0 XDG_RUNTIME_DIR=$T timeout 120 $FBSERVO $URL"
}

keys() {
    tmux -S "$T/t.sock" send-keys -t "$1" "${@:2}"
}

# fbservo が drmterm につながったか (つながらなければ、測ったものは drmterm の中の動きではない)
check_drmterm() {
    if ! grep -aq '^\[drmterm\]' "$T/fbservo.log"; then
        echo "fbservo が drmterm につながらなかった (ログ: $1/fbservo.log)" >&2
        exit 1
    fi
}

# C の道具 (bench/c/<名前>.c) を bench/bin にビルドしてパスを出す
tool() {
    local bin=$BENCH/bin/$1 src=$BENCH/c/$1.c
    mkdir -p "$BENCH/bin"
    if [ ! -x "$bin" ] || [ "$src" -nt "$bin" ]; then
        if [ "$1" = noinput ]; then
            cc -O2 -shared -fPIC -o "$bin" "$src" -ldl
        else
            # shellcheck disable=SC2046
            cc -O2 -o "$bin" "$src" $(pkg-config --cflags --libs libdrm)
        fi
    fi
    echo "$bin"
}

# 外部モニターをいま駆動している CRTC の番号 (pipe)。内蔵画面 (eDP、種類 14) 以外で最初のもの
external_pipe() {
    "$(tool modes)" | awk '
        /^crtc\[/ { i = $1; gsub(/crtc\[|\]/, "", i); id = $3; sub(/:$/, "", id); pipe[id] = i }
        /^connector / && $4 !~ /^14-/ && $6 != 0 && !found { print pipe[$6]; found = 1 }'
}
