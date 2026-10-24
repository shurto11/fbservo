"""j を押し続ける操作を、本物のキーボードと同じ間合いで再現する (fake_drmterm.py と hold_keys.py が使う)。

押下と同時に 1 文字、250ms 後から 33ms ごとにリピートの文字 (カーネルの既定のキーリピート)、1.5 秒で
離す。これを 3 回。文字は tmux から送り、押下 (1)・リピート (2)・離上 (0) は key(値) で伝える。
押した・離した時刻 (CLOCK_MONOTONIC) を log に `P <時刻>` / `R <時刻>` と書く。

tmux send-keys で文字を一定の間隔に送るだけでは、押し続けたことにならない (fbservo はキーの押下と
離上を見て、押している間だけ流す)。
"""
import subprocess
import time

KEY_J = 36  # linux/input-event-codes.h の KEY_J


def run(tsock, pane, key, log, times=3):
    def char():
        subprocess.run(['tmux', '-S', tsock, 'send-keys', '-t', pane, 'j'])

    with open(log, 'w', buffering=1) as f:
        for _ in range(times):
            t0 = time.monotonic()
            f.write(f'P {t0:.6f}\n')
            key(1)
            char()
            t = t0 + 0.25
            while t < t0 + 1.5:
                time.sleep(max(0, t - time.monotonic()))
                key(2)
                char()
                t += 0.033
            time.sleep(max(0, t0 + 1.5 - time.monotonic()))
            f.write(f'R {time.monotonic():.6f}\n')
            key(0)
            time.sleep(2)
