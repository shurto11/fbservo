"""hold_keys.py <tmux ソケット> <ペイン> <FIFO> <キーの記録>: j の長押しを再現し (holdkey.py)、押下・離上・
リピートを input_event にして FIFO へ書く。FIFO は fbservo の FBSERVO_KBD に渡したもので、fbservo は
キーボードを読んだときと同じに受け取る (本物の drmterm をキーボードなしで動かすときに使う)。
"""
import struct
import sys

import holdkey

tsock, pane, fifo, keylog = sys.argv[1:5]
EV_KEY = 1
out = open(fifo, 'wb', buffering=0)


def key(value):
    # struct input_event { struct timeval time; __u16 type; __u16 code; __s32 value; }
    out.write(struct.pack('llHHi', 0, 0, EV_KEY, holdkey.KEY_J, value))


holdkey.run(tsock, pane, key, keylog)
