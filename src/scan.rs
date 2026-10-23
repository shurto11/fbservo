//! 画面の走査の位置を見積もり、ティアリングせずに fb0 へ書く時刻を決める。
//!
//! fb0 は 1 枚しかなく (書いた端から画面に出る)、パネルは上の行から順に 16.7ms かけて走査する。
//! 走査している途中の範囲に書くと、その回の走査では上と下で別のコマが混ざる (ティアリング)。
//! 垂直帰線の割り込み (FBIO_WAITFORVSYNC) で起きてから書く方式では、CPU が混んでいると
//! 起きるのが 1〜3ms 遅れ、そのころにはもう画面の上のほうを走査している
//! (帰線は 0.6ms で、tmux のペインの上端まではそこから 0.3ms)。スクロール中は上の数十行だけ
//! 前のコマが残り、上側が潰れたように見える。
//!
//! そこで、別スレッドで帰線を待ち続けてその時刻 (位相) を測り、EDID の走査タイミングと合わせて
//! 「いま何行目を走査しているか」を計算する。書き始めるのは、書く範囲の下端を走査が通り過ぎて
//! から、次の回の走査が上端に来るまでの間 (ペインなら約 2.3ms ある。割り込みを待ってから書くと
//! 0.7ms ほどしかない)。それより遅れたら、次の回の同じ間まで待ってから書く。どちらでも、
//! 1 回の走査の中で新しいコマと古いコマが混ざらない (遅れたコマは 1 回あとの走査で出る)。
//!
//! 走査の位置は「行」で数える。帰線の始まり (表示の最後の行の次) を基準の時刻 `anchor` とし、
//! そこを `vactive` 行目として行を足していく (帰線を含めて 1 回の走査は `vtotal` 行)。
//! 画面の y 行目は、k 回目の走査 (k = 1 が anchor の帰線の次) で `y + k * vtotal` 行目に走査される。

use std::collections::VecDeque;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// linux/fb.h の FBIO_WAITFORVSYNC (_IOW('F', 0x20, __u32))
pub const FBIO_WAITFORVSYNC: libc::c_ulong = 0x4004_4620;

/// 走査の位置の見積もりの誤差と、見積もってから書き始めるまでの時間に見込む余裕
const MARGIN: Duration = Duration::from_micros(200);
/// 書ける間の始めからさらに遅らせる時間 (間の半ばまで)。帰線の時刻は割り込みで起きた時刻から
/// 見積もるので、ずれるとすれば遅い側 (= 早く書きすぎる側) に出る。そちらに余裕を持たせる
const EARLY: Duration = Duration::from_micros(500);

/// 画面の走査のタイミング。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timing {
    /// 1 回の走査 (帰線を含む) の時間
    pub period: Duration,
    /// 帰線を含めた行数
    pub vtotal: u32,
    /// 表示される行数
    pub vactive: u32,
}

impl Timing {
    /// 接続中の画面の EDID から、width x height の詳細タイミングを探す。
    pub fn from_edid(width: u32, height: u32) -> Option<Timing> {
        let dir = std::fs::read_dir("/sys/class/drm").ok()?;
        for e in dir.flatten() {
            let p = e.path();
            let connected = std::fs::read_to_string(p.join("status")).is_ok_and(|s| s.trim() == "connected");
            let Ok(edid) = std::fs::read(p.join("edid")) else {
                continue;
            };
            if connected {
                if let Some(t) = parse_edid(&edid, width, height) {
                    return Some(t);
                }
            }
        }
        None
    }

    fn line_secs(&self) -> f64 {
        self.period.as_secs_f64() / self.vtotal as f64
    }
}

/// EDID の 4 つの記述子のうち、詳細タイミングで width x height のもの。
fn parse_edid(edid: &[u8], width: u32, height: u32) -> Option<Timing> {
    (54..126).step_by(18).find_map(|off| {
        let t = edid.get(off..off + 18)?;
        let clock_khz = u16::from_le_bytes([t[0], t[1]]) as u64 * 10;
        if clock_khz == 0 {
            return None;
        }
        let hactive = t[2] as u32 | ((t[4] as u32 & 0xf0) << 4);
        let hblank = t[3] as u32 | ((t[4] as u32 & 0x0f) << 8);
        let vactive = t[5] as u32 | ((t[7] as u32 & 0xf0) << 4);
        let vblank = t[6] as u32 | ((t[7] as u32 & 0x0f) << 8);
        if (hactive, vactive) != (width, height) || vblank == 0 {
            return None;
        }
        let (htotal, vtotal) = ((hactive + hblank) as u64, vactive + vblank);
        let period = Duration::from_nanos(htotal * vtotal as u64 * 1_000_000 / clock_khz);
        Some(Timing { period, vtotal, vactive })
    })
}

/// 別スレッドで垂直帰線を待ち続け、帰線が始まる時刻 (1 回の走査の中での位置 = 位相) を測る。
///
/// 位相はパネルが決めるもので変わらないが、割り込みで起きた時刻は CPU が混んでいると 1 回の
/// 走査分近く遅れることもある。最初の位相は Servo を起動する前の静かなうちに測り (`start`)、
/// そのあとは、見積もった帰線の前後 2ms に起きた時刻だけを使って、1 回に 20µs までずつ
/// 合わせ直す (起動中のように皆そろって遅れても、見積もりが大きく飛ばない)。
pub struct Clock {
    base: Instant,
    period: Duration,
    /// 位相 (base から数えた ns を周期で割った余り) + 1。0 なら不明
    phase: Arc<AtomicU64>,
}

impl Clock {
    /// 合わせ直すのに使う時間
    const WINDOW: Duration = Duration::from_secs(2);
    /// 合わせ直すのに、見積もった帰線の前後これ以内に起きた時刻がいくつ要るか
    const SAMPLES: usize = 15;
    const NEAR: f64 = 2e-3;
    /// 1 回に動かす位相の上限 (秒)
    const MAX_STEP: f64 = 20e-6;

    /// file は fb0 (FBIO_WAITFORVSYNC を待てるもの)。最初の位相を測るため、8 回待つ (約 0.13 秒)。
    pub fn start(file: File, period: Duration) -> Clock {
        let base = Instant::now();
        let p = period.as_secs_f64();
        let secs = move |t: Instant| (t - base).as_secs_f64();
        let wait = move |file: &File| {
            let arg: u32 = 0;
            if unsafe { libc::ioctl(file.as_raw_fd(), FBIO_WAITFORVSYNC, &arg) } == 0 {
                Ok(Instant::now())
            } else {
                Err(std::io::Error::last_os_error())
            }
        };
        let first: Vec<f64> = (0..8).map_while(|_| wait(&file).ok()).map(|t| secs(t).rem_euclid(p)).collect();
        let initial = if first.len() == 8 { phase(first, p) } else { None };
        let shared = Arc::new(AtomicU64::new(0));
        let store = shared.clone();
        let save = move |ph: Option<f64>| store.store(ph.map_or(0, |ph| (ph * 1e9) as u64 + 1), Ordering::Relaxed);
        save(initial);
        let spawned = std::thread::Builder::new().name("vblank".into()).spawn(move || {
            let mut ph = initial;
            let mut times: VecDeque<Instant> = VecDeque::new();
            loop {
                let now = match wait(&file) {
                    Ok(t) => t,
                    // 帰線を待てない書き込み先 (ファイルなど) ならやめる
                    Err(e) if e.raw_os_error() == Some(libc::ENOTTY) => return,
                    // ほかのプロセスが DRM を握っている間 (EBUSY) などは、しばらくしてからやり直す
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                };
                times.push_back(now);
                while times.front().is_some_and(|&t| now - t > Self::WINDOW) {
                    times.pop_front();
                }
                ph = match ph {
                    // 最初に測れなかった (起動したときに DRM を握られていた) なら、ここで測る
                    None if times.len() >= Self::SAMPLES => phase(times.iter().map(|&t| secs(t).rem_euclid(p)).collect(), p),
                    None => None,
                    Some(cur) => {
                        let mut r: Vec<f64> = times
                            .iter()
                            .map(|&t| (secs(t) - cur + p / 2.0).rem_euclid(p) - p / 2.0)
                            .filter(|r| r.abs() < Self::NEAR)
                            .collect();
                        if r.len() >= Self::SAMPLES {
                            r.sort_by(f64::total_cmp);
                            Some((cur + r[r.len() / 10].clamp(-Self::MAX_STEP, Self::MAX_STEP)).rem_euclid(p))
                        } else {
                            Some(cur)
                        }
                    }
                };
                save(ph);
            }
        });
        if let Err(e) = spawned {
            eprintln!("[scan] スレッドを作れません: {e}");
        }
        Clock { base, period, phase: shared }
    }

    /// 直近の (now より前の) 帰線が始まった時刻。位相が分かっていなければ None。
    pub fn vblank(&self, now: Instant) -> Option<Instant> {
        let v = self.phase.load(Ordering::Relaxed);
        if v == 0 {
            return None;
        }
        let (p, ph) = (self.period.as_secs_f64(), (v - 1) as f64 / 1e9);
        let secs = (now - self.base).as_secs_f64();
        Some(self.base + Duration::from_secs_f64((secs - (secs - ph).rem_euclid(p)).max(0.0)))
    }
}

/// 割り込みで起きた時刻の、1 回の走査 (p 秒) の中での位置から、帰線が始まる位置を求める。
///
/// 起きるのが遅れることはあっても早まることはないので、位置は帰線の直後に固まり、遅れた分だけ
/// その後ろに散る。2ms の幅にいちばん多く入る所を固まりとし、その中で早いほうから 1 割の所を
/// 帰線とみなす (いちばん早いものを使うと、1 回の走査分近く遅れて固まりの直前に回り込んだ
/// 時刻を選んでしまう)。半分以上が固まっていなければ (CPU がずっと混んでいた) 当てにしない。
fn phase(ph: Vec<f64>, p: f64) -> Option<f64> {
    const WIDTH: f64 = 2e-3;
    let within = |a: f64| ph.iter().map(move |&b| (b - a).rem_euclid(p)).filter(|&d| d < WIDTH);
    let (start, count) = ph.iter().map(|&a| (a, within(a).count())).max_by_key(|&(_, c)| c)?;
    if count * 2 < ph.len() {
        return None;
    }
    let mut d: Vec<f64> = within(start).collect();
    d.sort_by(f64::total_cmp);
    Some((start + d[d.len() / 10]) % p)
}

/// 書く範囲。
#[derive(Clone, Copy, Debug)]
pub struct Span {
    /// 画面の行 y0..y1 に書く
    pub y0: f64,
    pub y1: f64,
    /// 1 行書く時間の、走査 1 行の時間に対する比 (分からなければ 0 = 一瞬で書けるとみなす。
    /// 書くのは走査より速いので、0 とみなすのがいちばん控えめ)
    pub speed: f64,
}

/// いつ書くか。
#[derive(Debug, PartialEq)]
pub enum Decision {
    /// この行まで走査が進むのを待ってから考え直す
    Wait(f64),
    /// いま書けば、この回の走査で出る
    Write(i64),
}

/// 走査が line 行目にあるとき、want 回目以降の走査で出すにはいつ書くか。
pub fn decide(line: f64, want: i64, span: Span, t: Timing) -> Decision {
    let vt = t.vtotal as f64;
    let m = MARGIN.as_secs_f64() / t.line_secs();
    // k 回目の走査で出すには、前の回に一番下の行を走査し終えてから書き始め
    // (書いている間に走査も進むので、書く速さの分だけ早められる)、
    // k 回目に一番上の行を走査するより前に書き始める
    let earliest = |k: i64| (k - 1) as f64 * vt + span.y1 - (span.y1 - span.y0) * span.speed + m;
    let latest = |k: i64| k as f64 * vt + span.y0 - m;
    let early = EARLY.as_secs_f64() / t.line_secs();
    let start = |k: i64| (earliest(k) + early).min((earliest(k) + latest(k)) / 2.0).max(earliest(k));
    let next = if earliest(1) < latest(1) {
        // いまからでも一番上の行に間に合う最初の回
        ((line - span.y0 + m) / vt).floor() as i64 + 1
    } else {
        // 書ける間が無い (範囲が高すぎる)。せめて前の回の走査とは重ならないように書く
        ((line - earliest(1)) / vt - 1e-6).ceil() as i64 + 1
    };
    let k = want.max(next);
    if line < start(k) {
        Decision::Wait(start(k))
    } else {
        Decision::Write(k)
    }
}

/// 走査の位置と時刻の換算。anchor は帰線が始まった時刻 (vactive 行目)。
pub fn line_at(t: Timing, anchor: Instant, at: Instant) -> f64 {
    let secs = if at >= anchor { (at - anchor).as_secs_f64() } else { -(anchor - at).as_secs_f64() };
    t.vactive as f64 + secs / t.line_secs()
}

pub fn time_at(t: Timing, anchor: Instant, line: f64) -> Instant {
    anchor + Duration::from_secs_f64(((line - t.vactive as f64) * t.line_secs()).max(0.0))
}

/// refresh 回目の走査の前の帰線の時刻
pub fn vblank_of(t: Timing, anchor: Instant, refresh: i64) -> Instant {
    anchor + t.period.mul_f64((refresh - 1).max(0) as f64)
}

/// 帰線の時刻 v のあとの走査が何回目か
pub fn refresh_after(t: Timing, anchor: Instant, v: Instant) -> i64 {
    let secs = if v >= anchor { (v - anchor).as_secs_f64() } else { -(anchor - v).as_secs_f64() };
    (secs / t.period.as_secs_f64()).round() as i64 + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    // このマシンの eDP パネル (1366x768、73.07MHz、htotal 1526、vtotal 798)
    const PANEL: Timing = Timing { period: Duration::from_nanos(16_665_498), vtotal: 798, vactive: 768 };
    // tmux のペイン (画面の 16〜688 行目)。1 行書くのは走査の 1/20 の時間
    const PANE: Span = Span { y0: 16.0, y1: 688.0, speed: 0.05 };

    /// 行 y を line 行目の時点で書いたとき、それが最初に画面に出る走査の回
    fn shows_in(y: f64, line: f64) -> i64 {
        ((line - y) / 798.0).floor() as i64 + 1
    }

    /// 決めたとおりに書いたとき、どの行も同じ回の走査で出ることを確かめる (ずれがあればティアリング)。
    /// 何回目の走査で出るかを返す
    fn check(line: f64, want: i64, span: Span) -> i64 {
        let mut line = line;
        loop {
            match decide(line, want, span, PANEL) {
                Decision::Wait(l) => {
                    assert!(l > line);
                    line = l;
                }
                Decision::Write(k) => {
                    let mut y = span.y0;
                    while y < span.y1 {
                        assert_eq!(shows_in(y, line + (y - span.y0) * span.speed), k, "line {line} y {y}");
                        y += 1.0;
                    }
                    return k;
                }
            }
        }
    }

    #[test]
    fn writes_between_scans() {
        // 帰線の割り込みですぐ起きた (768 行目) なら、次の走査に出る
        assert_eq!(check(770.0, 1, PANE), 1);
        // 下端を走査し終えていれば、帰線の前から書ける
        assert_eq!(decide(700.0, 1, PANE, PANEL), Decision::Write(1));
        // まだ下端を走査しているなら、通り過ぎるまで待つ
        assert!(matches!(decide(600.0, 1, PANE, PANEL), Decision::Wait(_)));
        assert_eq!(check(600.0, 1, PANE), 1);
        // 先の回を頼まれたら、その回まで待つ
        assert_eq!(check(770.0, 3, PANE), 3);
    }

    #[test]
    fn late_frames_wait_instead_of_tearing() {
        // 帰線から 1ms 遅れた (上端 16 行目のあたりを走査中) → 1 回あとの走査で出す
        let line = 768.0 + 1.0e-3 / PANEL.line_secs();
        assert!(matches!(decide(line, 1, PANE, PANEL), Decision::Wait(_)));
        assert_eq!(check(line, 1, PANE), 2);
        // ペインの中を走査中なら、どこでも 1 回あとの走査でずれずに出る
        // (1 回目の走査でペインの上端 16 行目は 814 行目、下端は 1486 行目)
        for line in (820..1486).step_by(7) {
            assert_eq!(check(line as f64, 1, PANE), 2, "line {line}");
        }
    }

    #[test]
    fn full_screen_uses_write_speed() {
        // 画面全体は帰線の間 (30 行) しか空かないが、書くのは走査より速いので、帰線の少し前から書ける
        let full = Span { y0: 0.0, y1: 768.0, speed: 0.09 };
        assert_eq!(decide(740.0, 1, full, PANEL), Decision::Write(1));
        for line in (600..2400).step_by(5) {
            check(line as f64, 1, full);
        }
        // 書く速さが分からなければ、帰線に入ってから書く
        let slow = Span { speed: 0.0, ..full };
        assert!(matches!(decide(720.0, 1, slow, PANEL), Decision::Wait(_)));
        for line in (600..2400).step_by(5) {
            check(line as f64, 1, slow);
        }
        // 書ける間が無いほど高い範囲でも、待ち続けはしない
        let tall = Span { speed: 0.0, y0: 0.0, y1: 790.0 };
        let mut line = 800.0;
        for _ in 0..3 {
            match decide(line, 1, tall, PANEL) {
                Decision::Wait(l) => line = l,
                Decision::Write(_) => return,
            }
        }
        panic!("書かずに待ち続けた");
    }

    #[test]
    fn finds_vblank_among_late_wakeups() {
        let p = 0.016_665;
        // 帰線は 3ms の位置。ふだんは 0〜0.3ms 遅れて起き、ときどき 5〜13ms 遅れる
        let mut ph: Vec<f64> = (0..40).map(|i| 0.003 + (i % 7) as f64 * 0.000_05).collect();
        ph.extend([0.008, 0.011, 0.014, 0.016, 0.0005]);
        let head = phase(ph.clone(), p).unwrap();
        assert!((head - 0.003).abs() < 1e-9, "{head}");
        // 1 回の走査分近く遅れて、固まりの直前に回り込んだ時刻があっても選ばない
        let mut before = ph.clone();
        before.push(0.0027);
        let head = phase(before, p).unwrap();
        assert!((head - 0.003).abs() < 1e-9, "{head}");
        // 帰線が周期の終わり近く (16.5ms) にあり、遅れて起きると 0 のほうへ回り込んでも同じ
        let wrapped: Vec<f64> = ph.iter().map(|x| (x + 0.0135) % p).collect();
        let head = phase(wrapped, p).unwrap();
        assert!((head - 0.0165).abs() < 1e-9, "{head}");
        // ずっと混んでいて、どれだけ遅れたか分からないときは使わない
        let spread: Vec<f64> = (0..40).map(|i| i as f64 * 0.0004).collect();
        assert_eq!(phase(spread, p), None);
    }

    #[test]
    fn reads_edid_timing() {
        let mut edid = vec![0u8; 128];
        // 73.07MHz、1366x768、水平帰線 160、垂直帰線 30
        edid[54..72].copy_from_slice(&[0x8b, 0x1c, 0x56, 0xa0, 0x50, 0x00, 0x1e, 0x30, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let t = parse_edid(&edid, 1366, 768).unwrap();
        assert_eq!((t.vtotal, t.vactive), (798, 768));
        assert!((t.period.as_secs_f64() * 1000.0 - 16.6656).abs() < 0.001);
        assert_eq!(parse_edid(&edid, 1920, 1080), None);
    }

    #[test]
    fn converts_lines_and_times() {
        let a = Instant::now();
        let v = vblank_of(PANEL, a, 3);
        assert_eq!(refresh_after(PANEL, a, v), 3);
        assert!((line_at(PANEL, a, v) - (768.0 + 2.0 * 798.0)).abs() < 0.01);
        assert!((line_at(PANEL, a, time_at(PANEL, a, 1000.0)) - 1000.0).abs() < 0.01);
    }
}
