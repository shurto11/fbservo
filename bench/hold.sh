#!/bin/bash
# hold.sh [keys|nokeys]: drmterm の中で j を押し続けたときの流れ方を、画面に描かずに再現する。
# 偽の drmterm (fake_drmterm.py) が、本物のキーボードと同じ間合いで tmux から文字を送る (holdkey.py)。
# keys なら押下・離上を KEY 行でも送る (drmterm がキーボードを握っているときと同じ)。nokeys は KEY 行を
# 送らない古い drmterm。結果は bench/out/hold-<keys|nokeys>
. "$(dirname "$0")/lib.sh"
MODE=${1:-keys}
OUT=$(out_dir "hold-$MODE")
truncate -s $((1280 * 720 * 4)) "$T/shm"
python3 "$BENCH/fake_drmterm.py" serve "$T/o.sock" "$T/shm" "$OUT/trace.txt" \
    --hold "$T/t.sock" "$T/pane" "$MODE" "$OUT/keys.txt" &
H=$!
KILL=$H
wait_sock "$T/o.sock"
start_tmux
# 本物のキーボードは読ませない (測っている間にユーザーが打ったキーが混ざらないように)
P=$(start_fbservo "DRMTERM_OVERLAY_SOCK=$T/o.sock" FBSERVO_KBD=/dev/null)
echo "$P" >"$T/pane"
wait "$H"
keys "$P" Q
sleep 2
cp "$T/fbservo.log" "$OUT/"
check_drmterm "$OUT"
python3 "$BENCH/analyze.py" hold "$OUT"
