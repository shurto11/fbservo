//! キーボードのデバイス (/dev/input/event*) から hjkl の押下と離上を読む。
//!
//! 端末からは文字しか届かず、キーを離したことが分からない。長押しも端末のキーリピート
//! (250ms 待ってから毎秒 30 回) でしか分からないので、最初の 1 回のあと 0.2 秒ほど止まり、
//! 離したあとも溜まった分だけ流れ続ける。Vimium のように押している間だけ流すため、
//! キーボードから押下・離上を読む (input グループに入っていれば読める)。
//!
//! スクロールを始めるのは端末から文字が届いたときだけで、ここでは「その文字のキーが
//! まだ押されているか」を答える。ほかのペインで打ったキーではスクロールしない。
//! 読めないとき (input グループに入っていない、ssh 越しなど) は、端末の文字だけで動かす。
//!
//! drmterm の中では、drmterm がキーボードを握る (EVIOCGRAB) ので、キーボードからは何も
//! 届かない。代わりに drmterm がソケットで送ってくるキー操作 (KEY 行、src/drmterm.rs) を読む。
//!
//! 文字とキーの対応は US / JIS 配列の位置 (KEY_H/J/K/L) で決め打ちする。
//! 覚えるのはこの 4 つのキーの状態だけで、ほかのキーのイベントは読み捨てる。
//! 起動したあとに挿したキーボードは読まない。

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// (文字, linux/input-event-codes.h のキー番号)
const KEYS: [(char, u16); 4] = [('h', 35), ('j', 36), ('k', 37), ('l', 38)];
const EV_KEY: u16 = 1;
/// EVIOCGBIT(EV_KEY, 96): キーボードが持つキーのビット列 (KEY_MAX 0x2ff まで)
const EVIOCGBIT_KEY: libc::c_ulong = (2 << 30) | (96 << 16) | ((b'E' as libc::c_ulong) << 8) | (0x20 + EV_KEY as libc::c_ulong);

/// 端末の文字は、キーボードのイベントからこれ以内に届くとみなす (tmux を通っても数 ms)
const MATCH: Duration = Duration::from_millis(300);

/// 端末から届いた文字が、キーボードのどの操作によるものか。
#[derive(Debug, PartialEq)]
pub enum Press {
    /// 押したまま。離すまで流す
    Held,
    /// 押してもう離した (文字が届く前に離した)。1 回分だけ進める
    Tap,
    /// 押したままのキーのリピート (離した直後に届いた分も含む)。押下のときに流し始めているので無視する
    Repeat,
    /// キーボードから読めていない。端末の文字だけで動かす
    Unknown,
}

#[derive(Default)]
struct KeyState {
    down: bool,
    /// 押したが、まだ端末から文字が届いていない押下の時刻
    presses: VecDeque<Instant>,
    /// 最後に押した / 離した時刻
    last: Option<Instant>,
}

#[derive(Default)]
pub struct Kbd {
    files: Vec<File>,
    /// drmterm からキー操作を受け取る接続と、まだ改行の来ていない分
    drmterm: Option<(UnixStream, Vec<u8>)>,
    keys: [KeyState; 4],
}

impl Kbd {
    /// hjkl を持つ入力デバイスをすべて開く。FBSERVO_KBD に ':' 区切りでパスを書くと、
    /// そこから読む (動作確認用。input_event を書き込む FIFO など)。
    pub fn open() -> Kbd {
        let (paths, check) = match std::env::var("FBSERVO_KBD") {
            Ok(v) => (v.split(':').map(PathBuf::from).collect(), false),
            Err(_) => (event_devices(), true),
        };
        let mut files = Vec::new();
        for p in paths {
            let Ok(f) = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(&p) else {
                continue;
            };
            if !check || has_hjkl(f.as_raw_fd()) {
                eprintln!("[kbd] {} を読む", p.display());
                files.push(f);
            }
        }
        if files.is_empty() {
            eprintln!("[kbd] キーボードを読めないので、端末のキーリピートで長押しを判断します");
        }
        Kbd { files, drmterm: None, keys: Default::default() }
    }

    /// drmterm から届くキー操作も読む
    pub fn add_drmterm(&mut self, s: UnixStream) {
        eprintln!("[kbd] drmterm からキー操作を受け取る");
        self.drmterm = Some((s, Vec::new()));
    }

    pub fn fds(&self) -> Vec<RawFd> {
        self.files.iter().map(|f| f.as_raw_fd()).chain(self.drmterm.as_ref().map(|(s, _)| s.as_raw_fd())).collect()
    }

    /// 届いているイベントを全部読む。読めなくなったデバイス (抜いたキーボード) は閉じる。
    pub fn read(&mut self, now: Instant) {
        const N: usize = 64;
        let size = std::mem::size_of::<libc::input_event>();
        let mut evs: [libc::input_event; N] = unsafe { std::mem::zeroed() };
        let mut i = 0;
        while i < self.files.len() {
            let fd = self.files[i].as_raw_fd();
            let gone = loop {
                let n = unsafe { libc::read(fd, evs.as_mut_ptr().cast(), size * N) };
                if n <= 0 {
                    // EAGAIN は読み終わり。それ以外 (ENODEV など) と EOF は使えなくなった
                    break n == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN);
                }
                for ev in &evs[..n as usize / size] {
                    if ev.type_ == EV_KEY {
                        self.event(ev.code, ev.value, now);
                    }
                }
            };
            if gone {
                eprintln!("[kbd] デバイスを読めなくなったので閉じる");
                self.files.remove(i);
            } else {
                i += 1;
            }
        }
        self.read_drmterm(now);
    }

    /// drmterm から届いた KEY 行を読む。描くためのソケットと同じものなので、待たずに読む
    fn read_drmterm(&mut self, now: Instant) {
        let Some((s, pending)) = &mut self.drmterm else {
            return;
        };
        let mut buf = [0u8; 1024];
        let gone = loop {
            let n = unsafe { libc::recv(s.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
            if n <= 0 {
                break n == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN);
            }
            pending.extend_from_slice(&buf[..n as usize]);
        };
        let mut events = Vec::new();
        while let Some(end) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            if let Some(ev) = parse_key(&String::from_utf8_lossy(&line)) {
                events.push(ev);
            }
        }
        if gone {
            eprintln!("[kbd] drmterm との接続が切れた");
            self.drmterm = None;
        }
        for (code, value) in events {
            self.event(code, value, now);
        }
    }

    /// value: 1 で押下、0 で離上、2 でリピート
    fn event(&mut self, code: u16, value: i32, now: Instant) {
        let Some(i) = KEYS.iter().position(|&(_, c)| c == code) else {
            return;
        };
        let k = &mut self.keys[i];
        match value {
            1 => {
                k.down = true;
                k.presses.push_back(now);
            }
            0 => k.down = false,
            _ => {}
        }
        k.last = Some(now);
    }

    /// 端末から文字 c が届いた。どの操作によるものかを返す (read を先に呼んでおくこと)。
    pub fn press(&mut self, c: char, now: Instant) -> Press {
        let Some(k) = self.key(c) else {
            return Press::Unknown;
        };
        // 文字が来なかった押下 (ほかのペインや入力モードで打ったもの) は捨てる
        k.presses.retain(|&t| now - t < MATCH);
        if k.presses.pop_front().is_some() {
            return if k.down && k.presses.is_empty() { Press::Held } else { Press::Tap };
        }
        if k.down || k.last.is_some_and(|t| now - t < MATCH) {
            Press::Repeat
        } else {
            Press::Unknown
        }
    }

    pub fn is_down(&mut self, c: char) -> bool {
        self.key(c).is_some_and(|k| k.down)
    }

    fn key(&mut self, c: char) -> Option<&mut KeyState> {
        let i = KEYS.iter().position(|&(k, _)| k == c)?;
        Some(&mut self.keys[i])
    }
}

/// drmterm の `KEY <コード> <値>` 行
fn parse_key(line: &str) -> Option<(u16, i32)> {
    match line.split_whitespace().collect::<Vec<_>>()[..] {
        ["KEY", code, value] => Some((code.parse().ok()?, value.parse().ok()?)),
        _ => None,
    }
}

/// /dev/input/event*
fn event_devices() -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir("/dev/input") else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = dir
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("event")))
        .collect();
    v.sort();
    v
}

/// 入力デバイスが h j k l のキーを持つか (電源ボタンやマウスは読まない)。
fn has_hjkl(fd: RawFd) -> bool {
    let mut bits = [0u8; 96];
    if unsafe { libc::ioctl(fd, EVIOCGBIT_KEY, bits.as_mut_ptr()) } < 0 {
        return false;
    }
    KEYS.iter().all(|&(_, code)| bits[code as usize / 8] & (1 << (code % 8)) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const J: u16 = 36;

    #[test]
    fn hold_tap_and_repeats() {
        let t = Instant::now();
        let ms = |n: u64| t + Duration::from_millis(n);
        let mut k = Kbd::default();
        // 押したまま文字が届く → 長押し。リピートの文字は無視
        k.event(J, 1, ms(0));
        assert_eq!(k.press('j', ms(5)), Press::Held);
        k.event(J, 2, ms(250));
        assert_eq!(k.press('j', ms(255)), Press::Repeat);
        assert!(k.is_down('j'));
        // 離した直後に届いたリピートの文字も無視
        k.event(J, 0, ms(300));
        assert!(!k.is_down('j'));
        assert_eq!(k.press('j', ms(302)), Press::Repeat);
        // 押してすぐ離してから文字を読んだ → 1 回分
        k.event(J, 1, ms(1000));
        k.event(J, 0, ms(1040));
        assert_eq!(k.press('j', ms(1041)), Press::Tap);
        // 2 回すばやく押してから文字を読んでも、2 回とも数える
        k.event(J, 1, ms(2000));
        k.event(J, 0, ms(2030));
        k.event(J, 1, ms(2080));
        assert_eq!(k.press('j', ms(2085)), Press::Tap);
        assert_eq!(k.press('j', ms(2086)), Press::Held);
    }

    #[test]
    fn reads_keys_from_drmterm() {
        let t = Instant::now();
        let (mut tx, rx) = UnixStream::pair().unwrap();
        let mut k = Kbd::default();
        k.add_drmterm(rx);
        // 行の途中で切れて届いても、改行が来てから読む。関係ない行は読み捨てる
        tx.write_all(b"KEY 36 1\nFB x\nKEY 3").unwrap();
        k.read(t);
        assert!(k.is_down('j'));
        assert_eq!(k.press('j', t), Press::Held);
        tx.write_all(b"6 0\n").unwrap();
        k.read(t);
        assert!(!k.is_down('j'));
        // drmterm が終わったら閉じる (そのあとは端末の文字だけで動く)
        drop(tx);
        k.read(t);
        assert!(k.drmterm.is_none());
    }

    #[test]
    fn unknown_without_keyboard_events() {
        let t = Instant::now();
        let mut k = Kbd::default();
        assert_eq!(k.press('j', t), Press::Unknown);
        assert_eq!(k.press('x', t), Press::Unknown);
        // 文字が届かなかった古い押下 (入力モードで打った j など) は数えない
        k.event(J, 1, t);
        k.event(J, 0, t + Duration::from_millis(30));
        assert_eq!(k.press('j', t + Duration::from_secs(2)), Press::Unknown);
    }
}
