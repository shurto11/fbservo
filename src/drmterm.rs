//! drmterm (外部モニターの DRM 端末) へ描くためのクライアント (fbrowse の drmterm.rs から写したもの)。
//!
//! drmterm に接続すると、画面全体と同じ大きさの共有メモリのパスと大きさが返るので、
//! /dev/fb0 の代わりにそこへ書く (src/fb.rs)。書く範囲 (= tmux のペイン) を RECT で申告すると、
//! drmterm は描き直すたびに端末を描いてから、その矩形だけ共有メモリの絵を重ねる。
//!
//! drmterm は PTY の子へ `DRMTERM_OVERLAY_SOCK` を渡すが、tmux サーバが drmterm より先に
//! 起動していると届かない (`tmux new -A` は既存のサーバへつなぐだけ)。そのため環境変数が
//! 無ければ既定のソケットを探し、drmterm 上の tmux クライアントが本当にこのペインの
//! セッションを映しているかを確かめる。
//!
//! プロトコル (行ごと・空白区切り)
//!   C→S  HELLO <名前>
//!   S→C  FB <パス> <幅> <高さ> <行幅px> <セル幅> <セル高> [tty] [DRM デバイス] [CRTC の番号]
//!   C→S  RECT <x> <y> <w> <h> | RECT none
//!   C→S  FRAME                (共有メモリを書き換えたので描き直してほしい)
//!   C→S  KEYS                 (キーボードを握っている間、キー操作を送ってほしい)
//!   S→C  KEY <コード> <値>    (linux/input-event-codes.h のコード。1 で押下、0 で離上、2 でリピート)
//!
//! drmterm は FRAME を受けると、次の垂直帰線まで待ってから画面へ写す。帰線の時刻は、
//! FB 行の DRM デバイスと CRTC の番号から自分で取る (`Vblank`)。
//!
//! drmterm はフォーカスがある間キーボードを握る (EVIOCGRAB) ので、キーボードを直接読んでも
//! 何も届かない。押したままかどうかは KEY 行で受け取る (src/kbd.rs)。

use crate::fb::Rect;
use crate::tmux;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct Info {
    /// 共有メモリのパス
    pub path: String,
    pub width: u32,
    pub height: u32,
    /// 1 行の画素数
    pub stride: u32,
    pub cell_w: u32,
    pub cell_h: u32,
    /// drmterm の PTY (= drmterm 上の tmux クライアントの tty)。分からなければ None
    pub tty: Option<String>,
    /// 画面を出している (DRM デバイスのパス, CRTC の番号)。VNC や古い drmterm なら None
    pub crtc: Option<(String, u32)>,
}

pub struct Overlay {
    sock: UnixStream,
    pub info: Info,
    /// 最後に申告した矩形 (まだ何も申告していなければ None)
    declared: Option<Option<Rect>>,
    /// 書き込みに失敗した (drmterm が終わった) ら以後は送らない
    closed: bool,
}

impl Overlay {
    fn send(&mut self, line: &str) {
        if self.closed {
            return;
        }
        if self.sock.write_all(format!("{line}\n").as_bytes()).is_err() {
            self.closed = true;
        }
    }

    /// 絵を重ねてほしい矩形 (画面全体の座標) を申告する。None で取り下げ。前と同じなら何もしない。
    pub fn set_rect(&mut self, r: Option<Rect>) {
        if self.declared == Some(r) {
            return;
        }
        self.declared = Some(r);
        let line = match r {
            Some(r) => format!("RECT {} {} {} {}", r.x, r.y, r.w, r.h),
            None => "RECT none".to_string(),
        };
        self.send(&line);
    }

    /// 共有メモリを書き換えたので描き直してもらう。
    pub fn frame(&mut self) {
        self.send("FRAME");
    }

    /// KEY 行を読むための接続 (同じソケットを複製したもの)
    pub fn key_stream(&self) -> Option<UnixStream> {
        self.sock.try_clone().ok()
    }
}

/// 探す順: 環境変数 → $XDG_RUNTIME_DIR → /tmp。(パス, 環境変数で渡されたか) のうち、あるものだけ。
fn socket_candidates() -> Vec<(String, bool)> {
    let mut list = Vec::new();
    if let Ok(p) = std::env::var("DRMTERM_OVERLAY_SOCK") {
        list.push((p, true));
    }
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        list.push((format!("{d}/drmterm-overlay.sock"), false));
    }
    list.push((format!("/tmp/drmterm-overlay-{}.sock", unsafe { libc::getuid() }), false));
    let mut seen = Vec::new();
    list.retain(|(p, _)| {
        let keep = !p.is_empty() && !seen.contains(p) && Path::new(p).exists();
        seen.push(p.clone());
        keep
    });
    list
}

/// drmterm の中で動いていれば接続して返す。そうでなければ None。
pub fn connect() -> Option<Overlay> {
    for (path, from_env) in socket_candidates() {
        let Some(ov) = try_connect(&path) else { continue };
        // 環境変数で渡された = drmterm の PTY の子。自分で探したときは、drmterm が本当に
        // このペインを映しているか確かめる (違えば drop で切断する)
        if from_env || ov.info.tty.as_deref().is_some_and(|t| tmux::shows_session(t, &tmux::own_session_name())) {
            return Some(ov);
        }
    }
    None
}

fn try_connect(path: &str) -> Option<Overlay> {
    let mut sock = UnixStream::connect(path).ok()?;
    sock.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    sock.write_all(b"HELLO fbservo\n").ok()?;
    let mut line = String::new();
    BufReader::new(sock.try_clone().ok()?).read_line(&mut line).ok()?;
    let info = parse_info(&line)?;
    sock.set_read_timeout(None).ok()?;
    // KEYS を知らない古い drmterm は読み捨てるだけ
    sock.write_all(b"KEYS\n").ok()?;
    // drmterm が詰まっても描画を止めない
    sock.set_write_timeout(Some(Duration::from_millis(200))).ok()?;
    Some(Overlay { sock, info, declared: None, closed: false })
}

fn parse_info(line: &str) -> Option<Info> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.first() != Some(&"FB") || parts.len() < 7 {
        return None;
    }
    let num = |i: usize| parts[i].parse::<u32>().ok().filter(|&v| v > 0);
    Some(Info {
        path: parts[1].to_string(),
        width: num(2)?,
        height: num(3)?,
        stride: num(4)?,
        cell_w: num(5)?,
        cell_h: num(6)?,
        tty: parts.get(7).filter(|t| **t != "-").map(|t| t.to_string()),
        crtc: parts
            .get(8)
            .filter(|c| **c != "-")
            .zip(parts.get(9).and_then(|p| p.parse().ok()))
            .map(|(c, p)| (c.to_string(), p)),
    })
}

/// drm.h の DRM_IOCTL_WAIT_VBLANK (_IOWR('d', 0x3a, union drm_wait_vblank))
const DRM_IOCTL_WAIT_VBLANK: libc::c_ulong = 0xc018_643a;
/// drm.h の DRM_IOCTL_DROP_MASTER (_IO('d', 0x1f))
const DRM_IOCTL_DROP_MASTER: libc::c_ulong = 0x641f;
const DRM_VBLANK_RELATIVE: u32 = 0x1;
const DRM_VBLANK_HIGH_CRTC_SHIFT: u32 = 1;
const DRM_VBLANK_HIGH_CRTC_MASK: u32 = 0x3e;

/// union drm_wait_vblank。頼むとき (request) は type, sequence, signal、返るとき (reply) は
/// type, sequence, 秒, マイクロ秒の並び
#[repr(C)]
struct WaitVblank {
    kind: u32,
    sequence: u32,
    sec: libc::c_long,
    usec: libc::c_long,
}

/// drmterm が画面を出している CRTC の垂直帰線。DRM マスターでなくても時刻を取れる。
pub struct Vblank {
    card: File,
    pipe: u32,
}

impl Vblank {
    pub fn open(info: &Info) -> Option<Vblank> {
        let (path, pipe) = info.crtc.as_ref()?;
        let card = OpenOptions::new().read(true).write(true).open(path).ok()?;
        // DRM マスターがいないときに開くと、こちらがマスターになって drmterm を起動し直せなく
        // なる。帰線の時刻を取るだけなので手放す (マスターでなければ何も起きない)
        unsafe { libc::ioctl(card.as_raw_fd(), DRM_IOCTL_DROP_MASTER) };
        Some(Vblank { card, pipe: *pipe })
    }

    /// n 回あとの垂直帰線を待ち、その (通し番号, 時刻) を返す。n = 0 なら待たずに直近のもの
    fn wait(&self, n: u32) -> Option<(u32, Instant)> {
        let mut v = WaitVblank {
            kind: DRM_VBLANK_RELATIVE | ((self.pipe << DRM_VBLANK_HIGH_CRTC_SHIFT) & DRM_VBLANK_HIGH_CRTC_MASK),
            sequence: n,
            sec: 0,
            usec: 0,
        };
        if unsafe { libc::ioctl(self.card.as_raw_fd(), DRM_IOCTL_WAIT_VBLANK, &mut v) } != 0 {
            return None;
        }
        // 時刻は CLOCK_MONOTONIC (Instant と同じ時計) なので、いまとの差で Instant に直す
        let now = Instant::now();
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        let ago = (ts.tv_sec - v.sec) * 1_000_000 + (ts.tv_nsec / 1000 - v.usec);
        let at = if ago >= 0 {
            now.checked_sub(Duration::from_micros(ago as u64))?
        } else {
            now + Duration::from_micros(ago.unsigned_abs())
        };
        Some((v.sequence, at))
    }

    /// 直近の垂直帰線の時刻
    pub fn last(&self) -> Option<Instant> {
        self.wait(0).map(|(_, at)| at)
    }

    /// 垂直帰線の間隔。何回か待って、カーネルが記録した時刻から測る (約 0.08 秒。時刻は
    /// 数十マイクロ秒揺れるので、何コマ分かの間隔で割る)
    pub fn period(&self) -> Option<Duration> {
        let (s0, t0) = self.wait(1)?;
        let (s1, t1) = self.wait(4)?;
        let p = t1.checked_duration_since(t0)? / s1.wrapping_sub(s0).max(1);
        (Duration::from_millis(5)..Duration::from_millis(50)).contains(&p).then_some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fb_line() {
        let info = parse_info("FB /dev/shm/drmterm-fb-1000 1280 720 1280 8 16 /dev/pts/11\n").unwrap();
        assert_eq!((info.width, info.height, info.stride, info.cell_w, info.cell_h), (1280, 720, 1280, 8, 16));
        assert_eq!(info.tty.as_deref(), Some("/dev/pts/11"));
        assert_eq!(info.crtc, None);
        // tty は付かないことも、"-" のこともある
        assert_eq!(parse_info("FB /x 1280 720 1280 8 16 -").unwrap().tty, None);
        assert_eq!(parse_info("FB /x 1280 720 1280 8 16").unwrap().tty, None);
        // tty のあとに DRM デバイスと CRTC の番号が付く (VNC なら "- -")
        let info = parse_info("FB /x 1280 720 1280 8 16 /dev/pts/3 /dev/dri/card1 2").unwrap();
        assert_eq!(info.crtc, Some(("/dev/dri/card1".to_string(), 2)));
        assert_eq!(parse_info("FB /x 1280 720 1280 8 16 - - -").unwrap().crtc, None);
        assert!(parse_info("FB /x 0 720 1280 8 16").is_none());
        assert!(parse_info("ERR").is_none());
    }
}
