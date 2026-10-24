"""bench/ のスクリプトが取った記録を読んで、画面に届いたものを数える。

  analyze.py hold <out>     j を押し続けたとき、止まらずに流れたか、離したらすぐ止まったか
  analyze.py phase <out>    fbservo が書く時刻が、外部モニターの垂直帰線から見てどこにあるか
  analyze.py monitor <out>  外部モニターの更新 1 回ごとに、新しいコマがいくつ映ったか (本物の drmterm)

記録 (時刻はどれも CLOCK_MONOTONIC の秒):
  trace.txt  F <FRAME を受けた時刻> (fake_drmterm.py か drmterm の DRMTERM_TRACE)
             B <書き始め> <書き終わり> <o|t> <矩形の上端> <下端> (drmterm の DRMTERM_TRACE)
  keys.txt   P / R <j を押した / 離した時刻> (holdkey.py)。phase と monitor は押している間だけを数える
  vbl.txt    <帰線の通し番号> <時刻> (bench/c/vbl.c。その更新の表示行の走査が始まる時刻)
"""
import bisect
import functools
import sys
from collections import Counter


def trace(d):
    return [l.split() for l in open(f'{d}/trace.txt') if l.strip() and not l.startswith('#')]


def frames(d):
    return [float(w[1]) for w in trace(d) if w[0] == 'F']


def vblanks(d):
    return [float(l.split()[1]) for l in open(f'{d}/vbl.txt')]


@functools.cache
def holds(d):
    """(押した時刻, 離した時刻) の並び"""
    ev = [l.split() for l in open(f'{d}/keys.txt')]
    return list(zip([float(w[1]) for w in ev if w[0] == 'P'], [float(w[1]) for w in ev if w[0] == 'R']))


def holding(d, t):
    """t に押している (押し始めの 0.1 秒、流し始めるまでは除く)"""
    return any(p + 0.1 <= t <= r for p, r in holds(d))


def hold(d):
    F = frames(d)
    print('長押しごとに: 押してから最初のコマ / 押している間のコマの最大の空き / 離してから最後のコマまで')
    for p, r in holds(d):
        during = [f for f in F if p <= f <= r]
        after = [f for f in F if r < f <= r + 1.5]
        if not during:
            print('  押している間にコマが 1 つも届かない')
            continue
        gaps = [(b - a) * 1000 for a, b in zip(during, during[1:])]
        big = max(gaps, default=0)
        at = (during[gaps.index(big)] - p) * 1000 if gaps else 0
        tail = (after[-1] - r) * 1000 if after else 0
        print(f'  {(during[0] - p) * 1000:4.0f}ms / {big:4.0f}ms ({at:.0f}ms 後) / {tail:4.0f}ms')
    print('押している間の空きが 1〜2 コマ分 (17〜33ms) を大きく超えたら、途中で止まって見える')


def phase(d):
    V = vblanks(d)
    F = [f for f in frames(d) if V[0] < f < V[-1]]
    P = (V[-1] - V[0]) / (len(V) - 1)
    ph = [(f - V[bisect.bisect_right(V, f) - 1]) * 1000 for f in F]
    print(f'FRAME {len(F)} 個 / 帰線 {len(V)} 回 (周期 {P * 1000:.3f}ms)')
    hist = Counter(min(int(x), 16) for x in ph)
    print('帰線から FRAME までの ms: ' + ' '.join(f'{k}:{v}' for k, v in sorted(hist.items())))
    # drmterm は FRAME を受けて組み (c ms)、次の帰線で写す。押している間の帰線ごとに新しく出るコマの数
    ks = [k for k in range(1, len(V)) if holding(d, V[k - 1])]
    for c in (0.5, 1.0, 2.0):
        seen = [bisect.bisect_left(F, v - c / 1000) for v in V]
        new = Counter(seen[k] - seen[k - 1] for k in ks)
        print(f'drmterm が組むのに {c}ms かかるとき、帰線ごとの新しいコマ数: '
              + ', '.join(f'{n} コマ {m} 回' for n, m in sorted(new.items())))
    print('帰線の真ん中 (8ms 前後) に集まっていれば、揺れても前後の帰線に転ばない')


def monitor(d, vtotal=750, height=720):
    V = vblanks(d)
    period = (V[-1] - V[0]) / (len(V) - 1)
    line = period / vtotal
    F = [t for t in frames(d) if V[0] <= t <= V[-1]]
    Bl = []
    for w in trace(d):
        if w[0] == 'B':
            Bl.append((float(w[1]), float(w[2]), w[3], int(w[4]) if len(w) > 4 else 0,
                       int(w[5]) if len(w) > 5 else height))
    Bl = [b for b in Bl if V[0] - 0.05 <= b[0] <= V[-1]]
    if not F or not Bl:
        sys.exit('この範囲に FRAME か書き込みがない (DRMTERM_TRACE を付けた drmterm か)')
    # 書き込み j が映す中身 = 書き始めまでに受けた FRAME の数 (それまでに書かれた最新のコマ)
    content = [bisect.bisect_right(F, b[0]) for b in Bl]
    y0, y1 = Bl[-1][3], Bl[-1][4]
    rows = {'上': y0, '中': (y0 + y1) // 2, '下': y1 - 1}

    def written(j, y):
        t0, t1, kind, a, b = Bl[j]
        lo, hi = (a, b) if kind == 'o' else (0, height)
        return t0 + (t1 - t0) * (y - lo) / max(1, hi - lo)

    def shown(k, y):
        best = None
        for j in range(len(Bl)):
            if written(j, y) <= V[k] + y * line:
                best = content[j]
        return best

    adv, tears, prev = Counter(), 0, None
    for k in range(len(V)):
        if not holding(d, V[k]):
            prev = None
            continue
        sh = {n: shown(k, y) for n, y in rows.items()}
        if None in sh.values():
            continue
        tears += sh['上'] != sh['下']
        if prev is not None:
            adv[sh['中'] - prev] += 1
        prev = sh['中']
    n = sum(adv.values())
    print(f'帰線の間隔 {period * 1000:.3f}ms、FRAME {len(F)} 回、書き込み {len(Bl)} 回、矩形 y {y0}..{y1}')
    print(f'更新 {n} 回ごとに進んだコマ数: ' + ', '.join(f'{a} コマ {c} 回' for a, c in sorted(adv.items()))
          + f' (1 コマずつ {adv[1] * 100 // max(n, 1)}%)')
    print(f'上端と下端で別のコマ (ティアリング): {tears} 回')
    ph = Counter()
    for t0, *_ in Bl:
        k = bisect.bisect_right(V, t0) - 1
        if k >= 0:
            ph[int((t0 - V[k]) / line) // 75 * 75] += 1
    print('書き始めたときの走査行 (720 以上が帰線): ' + ', '.join(f'{k}-{k + 74}:{v}' for k, v in sorted(ph.items())))


{'hold': hold, 'phase': phase, 'monitor': monitor}[sys.argv[1]](sys.argv[2])
