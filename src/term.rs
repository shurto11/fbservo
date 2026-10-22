//! 端末の raw モードと、標準入力のバイト列からキーへの変換。

use std::io::Write;

/// 生きている間だけ端末を raw モードにし、カーソルを隠す (fbterm のカーソル点滅が
/// ページの絵を上書きしないように)。drop で元に戻す。
pub struct RawMode {
    orig: Option<libc::termios>,
}

impl RawMode {
    pub fn enter() -> RawMode {
        let orig = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                None
            } else {
                let orig = t;
                t.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
                t.c_cflag |= libc::CS8;
                t.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
                t.c_cc[libc::VMIN] = 1;
                t.c_cc[libc::VTIME] = 0;
                libc::tcsetattr(0, libc::TCSANOW, &t);
                Some(orig)
            }
        };
        show_cursor(false);
        RawMode { orig }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(t) = &self.orig {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
        show_cursor(true);
    }
}

pub fn show_cursor(on: bool) {
    let mut out = std::io::stdout();
    let _ = out.write_all(if on { b"\x1b[?25h" } else { b"\x1b[?25l" });
    let _ = out.flush();
}

/// 端末の最下行に 1 行だけ書く (URL 入力欄)。
pub fn bottom_line(text: &str) {
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[999;1H\x1b[2K{text}");
    let _ = out.flush();
}

/// 画面を消して文章を出す (raw モードでは改行で行頭に戻らないので \r\n にする)。
pub fn show_text(text: &str) {
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[2J\x1b[H{}", text.replace('\n', "\r\n"));
    let _ = out.flush();
}

#[derive(Debug, Clone, PartialEq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Enter,
    Tab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Delete,
}

/// read() 1 回ぶんのバイト列をキーの並びにする。
pub fn parse(buf: &[u8]) -> Vec<Key> {
    let s = String::from_utf8_lossy(buf);
    let mut keys = Vec::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        let k = match c {
            '\x1b' => match it.peek() {
                Some('[') | Some('O') => {
                    it.next();
                    // CSI: 数字と ';' のあとに終端文字が来る
                    let mut num = String::new();
                    let mut fin = None;
                    for c in it.by_ref() {
                        if c.is_ascii_digit() || c == ';' {
                            num.push(c);
                        } else {
                            fin = Some(c);
                            break;
                        }
                    }
                    match (fin, num.as_str()) {
                        (Some('A'), _) => Key::Up,
                        (Some('B'), _) => Key::Down,
                        (Some('C'), _) => Key::Right,
                        (Some('D'), _) => Key::Left,
                        (Some('H'), _) | (Some('~'), "1") | (Some('~'), "7") => Key::Home,
                        (Some('F'), _) | (Some('~'), "4") | (Some('~'), "8") => Key::End,
                        (Some('~'), "3") => Key::Delete,
                        (Some('~'), "5") => Key::PageUp,
                        (Some('~'), "6") => Key::PageDown,
                        _ => continue,
                    }
                }
                _ => Key::Esc,
            },
            '\r' | '\n' => Key::Enter,
            '\t' => Key::Tab,
            '\x7f' | '\x08' => Key::Backspace,
            c if (c as u32) < 0x20 => Key::Ctrl((c as u8 + b'a' - 1) as char),
            c => Key::Char(c),
        };
        keys.push(k);
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keys() {
        assert_eq!(parse(b"jk"), vec![Key::Char('j'), Key::Char('k')]);
        assert_eq!(parse(b"\x1b[A\x1b[6~"), vec![Key::Up, Key::PageDown]);
        assert_eq!(parse(b"\x1b"), vec![Key::Esc]);
        assert_eq!(parse(b"\x03\r\x7f"), vec![Key::Ctrl('c'), Key::Enter, Key::Backspace]);
        assert_eq!(parse("あ".as_bytes()), vec![Key::Char('あ')]);
    }
}
