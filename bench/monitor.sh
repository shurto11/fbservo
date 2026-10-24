#!/bin/bash
# monitor.sh: 本物の drmterm を外部モニターに出し、その中の fbservo で j を押し続けて、外部モニターの
# 更新 1 回ごとに新しいコマがいくつ映ったか (1 ずつなら滑らか) と、ティアリングを数える。
# 画面に届いたものを測るのはこれだけ。結果は bench/out/monitor
#
# 外部モニターはユーザーの drmterm が使っているので、動かす前にユーザーに頼んで止めてもらう
# (止めてよいか聞いてから止める)。drmterm が動いていたら何もしない。
# テスト用の drmterm にはキーボードを開かせない (LD_PRELOAD の noinput)。キー操作は FBSERVO_KBD の
# FIFO から fbservo へ直接渡す (hold_keys.py)。DRMTERM=<drmterm のパス> で別のビルドを測れる。
. "$(dirname "$0")/lib.sh"
DRMTERM=${DRMTERM:-$HOME/ssd/drmterm/target/release/drmterm}
if pgrep -x drmterm >/dev/null; then
    echo "drmterm が動いている。外部モニターを使うので、ユーザーに止めてよいか聞いてから止めること" >&2
    exit 1
fi
OUT=$(out_dir monitor)
NOINPUT=$(tool noinput)
VBL=$(tool vbl)
# 既定のソケットを塞ぎ、テスト用の drmterm に pid 付きのソケットと共有メモリを使わせる
# (ユーザーの /dev/shm/drmterm-fb-<uid> に触らない)
mkdir -p "$T/tmux"
python3 "$BENCH/fake_drmterm.py" dummy "$T/drmterm-overlay.sock" &
L=$!
KILL=$L
wait_sock "$T/drmterm-overlay.sock"
tmux -S "$T/t.sock" -f /dev/null new-session -d -s ext -x 160 -y 44 'sleep 3600'
env -u TMUX -u TMUX_PANE TMUX_TMPDIR="$T/tmux" XDG_RUNTIME_DIR="$T" LD_PRELOAD="$NOINPUT" DRMTERM_TRACE="$OUT/trace.txt" \
    "$DRMTERM" -- tmux -S "$T/t.sock" attach -t ext </dev/null >"$OUT/drmterm.log" 2>&1 &
D=$!
KILL="$L $D"
wait_sock "$T/drmterm-overlay.sock.$D"
sleep 1
PIPE=$(external_pipe)
mkfifo "$T/kbd"
# fbservo が読み始める前から書き手を開いておく (書き手がいないと fbservo は読み終わったとみなして閉じる)
exec 3<>"$T/kbd"
P=$(start_fbservo "DRMTERM_OVERLAY_SOCK=$T/drmterm-overlay.sock.$D" "FBSERVO_KBD=$T/kbd")
sleep 12
"$VBL" "$PIPE" 100000 >"$OUT/vbl.txt" &
V=$!
KILL="$L $D $V"
python3 "$BENCH/hold_keys.py" "$T/t.sock" "$P" "$T/kbd" "$OUT/keys.txt"
kill "$V"
keys "$P" Q
sleep 2
cp "$T/fbservo.log" "$OUT/"
kill "$D"
wait "$D" 2>/dev/null || true
rm -f "/dev/shm/drmterm-fb-$(id -u).$D"
check_drmterm "$OUT"
python3 "$BENCH/analyze.py" monitor "$OUT"
