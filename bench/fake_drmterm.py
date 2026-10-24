"""drmterm のふりをするオーバーレイのサーバ。画面には何も出さない。

  fake_drmterm.py serve <ソケット> <共有メモリ> <記録> [--crtc <DRM> <pipe>] [--hold <tmux ソケット> <ペインのファイル> keys|nokeys <キーの記録>]
  fake_drmterm.py dummy <ソケット>

serve: HELLO に FB 行を返し、FRAME を受けた時刻 (CLOCK_MONOTONIC) を記録に `F <時刻>` と書く。
  --crtc を付けると FB 行で CRTC を名乗る (本物の外部モニターの垂直帰線の時刻を fbservo に取らせる)。
  --hold を付けると、fbservo が描き始めて 10 秒後に j の長押しを再現して終わる (holdkey.py)。
  keys なら、drmterm がキーボードを握っているときと同じく、押下・離上・リピートを KEY 行でも送る。
  nokeys は KEY 行を送らない古い drmterm。
dummy: 既定のソケットを塞ぐだけ (本物のテスト用 drmterm に pid 付きのソケットと共有メモリを使わせる)。
"""
import argparse
import socket
import threading
import time

import holdkey

p = argparse.ArgumentParser()
sub = p.add_subparsers(dest='cmd', required=True)
s = sub.add_parser('serve')
s.add_argument('sock')
s.add_argument('shm')
s.add_argument('trace')
s.add_argument('--crtc', nargs=2, metavar=('DRM', 'PIPE'))
s.add_argument('--hold', nargs=4, metavar=('TMUX', 'PANEFILE', 'MODE', 'KEYLOG'))
d = sub.add_parser('dummy')
d.add_argument('sock')
a = p.parse_args()

srv = socket.socket(socket.AF_UNIX)
srv.bind(a.sock)
srv.listen(8)
if a.cmd == 'dummy':
    while True:
        srv.accept()[0].close()

log = open(a.trace, 'w', buffering=1)
crtc = ' '.join(a.crtc) if a.crtc else '- -'
client = None
drawing = threading.Event()


def serve(c):
    global client
    for line in c.makefile():
        w = line.split()
        if not w:
            continue
        if w[0] == 'HELLO':
            c.sendall(f'FB {a.shm} 1280 720 1280 8 16 - {crtc}\n'.encode())
        elif w[0] == 'KEYS':
            client = c
        elif w[0] == 'FRAME':
            log.write(f'F {time.monotonic():.6f}\n')
        elif w[0] == 'RECT' and w[1] != 'none':
            drawing.set()


def accept():
    while True:
        c, _ = srv.accept()
        threading.Thread(target=serve, args=(c,), daemon=True).start()


threading.Thread(target=accept, daemon=True).start()
if not a.hold:
    while True:
        time.sleep(3600)

tsock, panefile, mode, keylog = a.hold
# ページを読み込み終えるまで待つ
if not drawing.wait(60):
    raise SystemExit('fbservo が描き始めない')
time.sleep(10)


def key(value):
    if mode == 'keys' and client:
        client.sendall(f'KEY {holdkey.KEY_J} {value}\n'.encode())


holdkey.run(tsock, open(panefile).read().strip(), key, keylog)
