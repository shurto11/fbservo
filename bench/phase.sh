#!/bin/bash
# phase.sh: drmterm の中で j を押し続けたとき、fbservo が書く時刻が外部モニターの垂直帰線から見て
# どこにあるかを測る。画面には描かない。偽の drmterm に、いま外部モニターを駆動している CRTC を
# 名乗らせる (帰線の時刻を取るだけなら DRM マスターは要らないので、ユーザーの drmterm は止めない)。
# NOCRTC=1 なら名乗らない (帰線に合わせない古い動き)。結果は bench/out/phase
. "$(dirname "$0")/lib.sh"
PIPE=$(external_pipe)
if [ -z "$PIPE" ]; then
    echo "外部モニターが映っていない" >&2
    exit 1
fi
OUT=$(out_dir phase)
truncate -s $((1280 * 720 * 4)) "$T/shm"
CRTC=(--crtc /dev/dri/card1 "$PIPE")
[ -n "${NOCRTC:-}" ] && CRTC=()
python3 "$BENCH/fake_drmterm.py" serve "$T/o.sock" "$T/shm" "$OUT/trace.txt" "${CRTC[@]}" \
    --hold "$T/t.sock" "$T/pane" keys "$OUT/keys.txt" &
H=$!
wait_sock "$T/o.sock"
start_tmux
P=$(start_fbservo "DRMTERM_OVERLAY_SOCK=$T/o.sock" FBSERVO_KBD=/dev/null)
echo "$P" >"$T/pane"
# 長押しを終えるまで帰線の時刻を取り続ける
"$(tool vbl)" "$PIPE" 100000 >"$OUT/vbl.txt" &
V=$!
KILL="$H $V"
wait "$H"
kill "$V"
keys "$P" Q
sleep 2
cp "$T/fbservo.log" "$OUT/"
check_drmterm "$OUT"
python3 "$BENCH/analyze.py" phase "$OUT"
