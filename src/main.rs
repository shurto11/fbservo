//! fbservo: Servo (Rust 製ブラウザエンジン) でページを描き、/dev/fb0 へ直接書く試作。
//!
//! 動作中は端末に何も出さない (出すとページの絵を上書きする)。ログは
//! $XDG_RUNTIME_DIR/fbservo.log に書き、終了時にフレーム時間とメモリの集計を表示する。

mod engine;
mod fb;
mod hints;
mod term;
mod tmux;

use anyhow::{Context, Result};
use engine::Engine;
use fb::Fb;
use hints::HintMode;
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use term::{Key, RawMode};
use url::Url;

const USAGE: &str = "\
使い方: fbservo [--scale N] [URL | ファイル | 検索語]

キー (fbrowse と同じ配置):
  hjkl          スクロール            gg / G       先頭 / 末尾
  f             リンクヒント (ラベルの 2 文字でクリック。f / Esc で取消)
                入力欄を選ぶとそのまま入力 (Enter で確定、Esc で戻る)
                開いたページの入力欄にフォーカスがあれば、選ばなくても入力
  d             直前の入力を削除
  b / r         戻る / 再読込         Enter/Space  そのままページへ送る (矢印も)
  s             描き直し              + - 0        ズームイン / アウト / リセット
  t             新しいタブ            ] / [        次 / 前のタブ
  x             タブを閉じる          y            URL を tmux バッファへコピー
  ?             このヘルプ            Q            終了 (Ctrl-C はどのモードでも)
fbservo だけのキー:
  L             進む                  O            今の URL を編集して開く
まだ無いもの (ROADMAP.md): m カーソル, o お気に入り, p クリップボード,
  T タブを別ペインへ, M/P mpv";

/// "example.com/path" のようにドメインらしく見えるか (fbrowse と同じ判定)。
fn looks_like_domain(s: &str) -> bool {
    if s.chars().any(char::is_whitespace) {
        return false;
    }
    let host = s.split('/').next().unwrap_or("");
    match host.rsplit_once('.') {
        Some((head, tld)) => !head.is_empty() && tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()),
        None => false,
    }
}

/// encodeURIComponent 相当。
fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// URL・ローカルのファイル・ドメイン・検索語のいずれかを URL にする。空なら None。
fn resolve(input: &str) -> Option<Url> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let has_scheme = ["http://", "https://", "file://", "about:", "data:", "servo:"]
        .iter()
        .any(|p| input.starts_with(p));
    if has_scheme {
        return Url::parse(input).ok();
    }
    if Path::new(input).exists() {
        return std::fs::canonicalize(input).ok().and_then(|p| Url::from_file_path(p).ok());
    }
    if input.starts_with("www.") || looks_like_domain(input) {
        return Url::parse(&format!("https://{input}")).ok();
    }
    Url::parse(&format!("https://duckduckgo.com/?q={}", encode_component(input))).ok()
}

/// 1 フレームを fb0 に出すまでの各段階の時間。
#[derive(Default)]
struct Stats {
    frames: u32,
    sum: [Duration; 3],
    max: [Duration; 3],
    /// スクロールキーを押してから、その結果が fb0 に出るまで
    keys: u32,
    key_sum: Duration,
    key_max: Duration,
}

impl Stats {
    // 全角文字があると {:<N} では揃わないので、表示幅 9 に合わせて空白を入れておく
    const NAMES: [&'static str; 3] = ["paint    ", "readback ", "fb 転送  "];

    fn add(&mut self, t: [Duration; 3]) {
        self.frames += 1;
        for i in 0..3 {
            self.sum[i] += t[i];
            self.max[i] = self.max[i].max(t[i]);
        }
    }

    fn add_key(&mut self, d: Duration) {
        self.keys += 1;
        self.key_sum += d;
        self.key_max = self.key_max.max(d);
    }

    fn summary(&self) -> String {
        let mut s = format!("フレーム数: {}\n", self.frames);
        if self.frames == 0 {
            return s;
        }
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        for i in 0..3 {
            let avg = self.sum[i] / self.frames;
            s += &format!("  {} 平均 {:6.1} ms  最大 {:6.1} ms\n", Self::NAMES[i], ms(avg), ms(self.max[i]));
        }
        let total: Duration = self.sum.iter().sum::<Duration>() / self.frames;
        s += &format!("  合計      平均 {:6.1} ms\n", ms(total));
        if self.keys > 0 {
            s += &format!(
                "キー→画面 平均 {:6.1} ms  最大 {:6.1} ms ({} 回)\n",
                ms(self.key_sum / self.keys),
                ms(self.key_max),
                self.keys
            );
        }
        s
    }
}

/// 最大常駐メモリ (MB)。
fn max_rss_mb() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    ru.ru_maxrss as u64 / 1024
}

fn log_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    Path::new(&dir).join("fbservo.log")
}

enum Mode {
    Normal,
    /// キーをそのままページへ送る (リンクヒントで入力欄を選ぶか、開いたページの入力欄に
    /// フォーカスが当たっていると入る)
    Insert,
    /// 端末の最下行で URL を入力中 (その間は fb0 へ描かない)
    Prompt(Open, String),
    /// 端末にキー一覧を出している (その間は fb0 へ描かない。何かキーを押すと戻る)
    Help,
    /// リンクヒントのラベルを打っている
    Hint(HintMode),
}

impl Mode {
    /// 端末に文字を出していて、fb0 へ描いてはいけない間
    fn uses_terminal(&self) -> bool {
        matches!(self, Mode::Prompt(..) | Mode::Help)
    }
}

/// 入力した URL をどこで開くか
#[derive(Clone, Copy, PartialEq)]
enum Open {
    /// 今のタブ (起動時と O)
    Here,
    /// 新しいタブ (t)
    NewTab,
}

fn show_prompt(open: Open, buf: &str) {
    let label = match open {
        Open::Here => "URL",
        Open::NewTab => "新しいタブ",
    };
    term::show_cursor(true);
    term::bottom_line(&format!("{label}: {buf}"));
}

/// 操作の結果をログに残す。
/// TODO(ROADMAP 1-4): 絵の中のステータス行にも出す (ペインに描いている間は端末が見えない)
fn say(msg: &str) {
    eprintln!("[ui] {msg}");
}

fn main() {
    let mut scale = 1.0f32;
    let mut target = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            "--scale" => scale = args.next().and_then(|s| s.parse().ok()).unwrap_or(1.0),
            _ => target = Some(target.map_or(a.clone(), |t: String| format!("{t} {a}"))),
        }
    }

    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let mut fb = match Fb::open() {
        Ok(fb) => fb,
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
    };

    // Servo のログや panic を含め、stderr はすべてログファイルへ回す
    let log = log_path();
    let orig_stderr = unsafe { libc::dup(2) };
    if let Ok(f) = std::fs::File::create(&log) {
        unsafe { libc::dup2(f.as_raw_fd(), 2) };
    }

    let mut stats = Stats::default();
    let result = run(&mut fb, target.as_deref(), scale, &mut stats);

    unsafe { libc::dup2(orig_stderr, 2) };
    print!("\x1b[2J\x1b[H");
    if let Err(e) = &result {
        eprintln!("エラー: {e:#}");
    }
    print!("{}", stats.summary());
    println!("最大メモリ (RSS): {} MB", max_rss_mb());
    println!("ログ: {}", log.display());
    if result.is_err() {
        std::process::exit(1);
    }
}

fn run(fb: &mut Fb, target: Option<&str>, scale: f32, stats: &mut Stats) -> Result<()> {
    let (wake_fd, waker) = engine::pipe_waker()?;
    let first = target.and_then(resolve);
    let mode = if first.is_some() { Mode::Normal } else { Mode::Prompt(Open::Here, String::new()) };
    let mut ui = Ui { mode, pending_g: false, typed: 0 };
    let url = first.unwrap_or_else(|| Url::parse("about:blank").unwrap());
    eprintln!("[start] {url} ({}x{}, scale {scale})", fb.width, fb.height);

    let _raw = RawMode::enter();
    let mut eng = Engine::new(fb.width, fb.height, scale, url, waker)?;
    if let Mode::Prompt(open, buf) = &ui.mode {
        show_prompt(*open, buf);
    }

    // 最後に描いたフレーム。端末 (tmux の時計など) に上書きされても描き直せるよう取っておく。
    let mut last: Option<(Vec<u8>, u32, u32)> = None;
    let mut last_blit = Instant::now();
    // まだ画面に反映していない最初のスクロールキーを押した時刻
    let mut key_at: Option<Instant> = None;
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 256];
    let start = Instant::now();
    let ms = |t: Instant| (t - start).as_secs_f64() * 1000.0;

    loop {
        let timeout = if eng.animating() { 16 } else { 1000 };
        let mut fds = [
            libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake_fd, events: libc::POLLIN, revents: 0 },
        ];
        unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if fds[1].revents & libc::POLLIN != 0 {
            drain(wake_fd);
        }

        let mut redraw = false;
        if fds[0].revents & libc::POLLIN != 0 {
            let n = stdin.read(&mut buf).context("標準入力を読めません")?;
            if n == 0 {
                break;
            }
            for k in term::parse(&buf[..n]) {
                if matches!(ui.mode, Mode::Normal) && is_scroll_key(&k) {
                    // 先頭で k を押したときのように画面が変わらないキーもある。そうした
                    // 古い時刻を次のキーの計測に持ち越さないよう、しばらく経ったら捨てる。
                    let now = Instant::now();
                    if key_at.is_none_or(|t| now - t > KEY_STALE) {
                        key_at = Some(now);
                    }
                }
                match handle_key(&mut eng, &mut ui, k) {
                    Action::None => {}
                    Action::Redraw => redraw = true,
                    Action::Quit => {
                        fb.clear();
                        return Ok(());
                    }
                }
            }
        }
        // キー処理のあとに回す。スクロールは Servo 内のキューに積まれるだけで、
        // spin_event_loop まで処理されない (waker も呼ばれないので、先に回すと次の
        // poll のタイムアウトまで最大 1 秒待たされる)。
        eng.spin();
        // ヒントの JS の結果は spin の中で届く。先に打ったラベルがあればここでクリックする
        finish_hint(&eng, &mut ui);
        // 開いたページが最初から入力欄にフォーカスを当てていたら (検索欄など)、そのまま打てるようにする
        if eng.take_input_focused() && matches!(ui.mode, Mode::Normal) {
            enter_insert(&mut ui);
        }
        if ui.mode.uses_terminal() {
            continue;
        }

        if eng.take_frame() {
            let t0 = Instant::now();
            eng.paint();
            let t1 = Instant::now();
            let img = eng.read();
            let t2 = Instant::now();
            if let Some(img) = img {
                let (w, h) = img.dimensions();
                fb.blit(img.as_raw(), w, h, 0, 0);
                last = Some((img.into_raw(), w, h));
                last_blit = Instant::now();
                stats.add([t1 - t0, t2 - t1, last_blit - t2]);
                if let Some(t) = key_at.take() {
                    stats.add_key(last_blit - t);
                    eprintln!("[key] 押してから {:.1}ms で表示", (last_blit - t).as_secs_f64() * 1000.0);
                }
                eprintln!(
                    "[frame] t={:.0} paint {:.1}ms read {:.1}ms blit {:.1}ms",
                    ms(t0),
                    (t1 - t0).as_secs_f64() * 1000.0,
                    (t2 - t1).as_secs_f64() * 1000.0,
                    (last_blit - t2).as_secs_f64() * 1000.0
                );
            }
        } else if redraw || last_blit.elapsed() >= Duration::from_secs(1) {
            if let Some((px, w, h)) = &last {
                fb.blit(px, *w, *h, 0, 0);
            }
            last_blit = Instant::now();
        }
    }
    Ok(())
}

/// スクロールキーを押してからこれだけ経ってもフレームが来なければ、画面が変わらない
/// キーだったとみなす (いつもは 30〜50ms で出る)。
const KEY_STALE: Duration = Duration::from_millis(500);

fn drain(fd: RawFd) {
    let mut b = [0u8; 64];
    while unsafe { libc::read(fd, b.as_mut_ptr().cast(), b.len()) } > 0 {}
}

/// 押してから画面に出るまでを計る、ページを動かすキー
fn is_scroll_key(k: &Key) -> bool {
    matches!(
        k,
        Key::Char('j' | 'k' | 'h' | 'l' | ' ')
            | Key::Up
            | Key::Down
            | Key::Left
            | Key::Right
            | Key::PageUp
            | Key::PageDown
    )
}

/// キー入力の状態。
struct Ui {
    mode: Mode,
    /// g を 1 回押して、次の g を待っている (gg で先頭)
    pending_g: bool,
    /// 入力モードでページへ送った文字数。d でその分を消す
    typed: usize,
}

enum Action {
    None,
    /// 端末に書いた入力欄などを消すため、直前のフレームを描き直す
    Redraw,
    Quit,
}

/// ノーマルモードのキーが表す操作。
#[derive(Debug, PartialEq)]
enum Cmd {
    Scroll(f32, f32),
    Hints,
    Top,
    Bottom,
    /// Enter・Space・矢印などをそのままページへ送る
    Page(Key),
    Back,
    Forward,
    Reload,
    Redraw,
    DeleteTyped,
    NewTab,
    EditUrl,
    /// 次 (1) / 前 (-1) のタブ
    Tab(isize),
    CloseTab,
    CopyUrl,
    /// 倍率に足す量。None なら等倍
    Zoom(Option<f32>),
    Help,
    Quit,
    /// fbrowse にあって、まだ作っていない機能 (名前, ROADMAP.md の項目)
    NotYet(&'static str, &'static str),
    /// fbrowse にあって、fbservo では要らない機能
    NotNeeded(&'static str),
    None,
}

/// hjkl 1 回ぶんのスクロール量 (px)。fbrowse は撮り直しが重いので 360px ずつ動かすが、
/// fbservo はすぐ描けるので細かく動かす。
const LINE: f32 = 60.0;

/// ノーマルモードのキーを操作にする。配置は fbrowse に合わせている。
fn normal_cmd(k: &Key, pending_g: &mut bool) -> Cmd {
    let after_g = std::mem::take(pending_g);
    match k {
        Key::Char('h') => Cmd::Scroll(-LINE, 0.0),
        Key::Char('j') => Cmd::Scroll(0.0, LINE),
        Key::Char('k') => Cmd::Scroll(0.0, -LINE),
        Key::Char('l') => Cmd::Scroll(LINE, 0.0),
        Key::Char('g') if after_g => Cmd::Top,
        Key::Char('g') => {
            *pending_g = true;
            Cmd::None
        }
        Key::Char('G') => Cmd::Bottom,
        Key::Enter
        | Key::Char(' ')
        | Key::Up
        | Key::Down
        | Key::Left
        | Key::Right
        | Key::PageUp
        | Key::PageDown
        | Key::Home
        | Key::End => Cmd::Page(k.clone()),
        Key::Char('b') => Cmd::Back,
        Key::Char('L') => Cmd::Forward,
        Key::Char('r') => Cmd::Reload,
        Key::Char('s') => Cmd::Redraw,
        Key::Char('d') => Cmd::DeleteTyped,
        Key::Char('t') => Cmd::NewTab,
        Key::Char('O') => Cmd::EditUrl,
        Key::Char(']') => Cmd::Tab(1),
        Key::Char('[') => Cmd::Tab(-1),
        Key::Char('x') => Cmd::CloseTab,
        Key::Char('y') => Cmd::CopyUrl,
        Key::Char('+' | '=') => Cmd::Zoom(Some(0.1)),
        Key::Char('-') => Cmd::Zoom(Some(-0.1)),
        Key::Char('0') => Cmd::Zoom(None),
        Key::Char('?') => Cmd::Help,
        Key::Char('Q') => Cmd::Quit,
        Key::Char('f') => Cmd::Hints,
        Key::Char('m') => Cmd::NotYet("カーソルモード", "3-2"),
        Key::Char('o') => Cmd::NotYet("お気に入りに追加", "2-4"),
        Key::Char('p') => Cmd::NotYet("クリップボード表示", "3-4"),
        Key::Char('T') => Cmd::NotYet("タブを別ペインへ移す", "3-8"),
        Key::Char('M' | 'P') => Cmd::NotYet("mpv で再生", "4-1"),
        Key::Char('c' | 'C') => Cmd::NotNeeded("動画モード (fbservo は常に動く絵を描いている)"),
        Key::Char('i') => Cmd::NotNeeded("入力モード (f で入力欄を選ぶと入る)"),
        _ => Cmd::None,
    }
}

fn handle_key(eng: &mut Engine, ui: &mut Ui, k: Key) -> Action {
    // どのモードでも Ctrl-C で終われるようにする (入力モードでは q も Q もページへ送るので、
    // ほかに抜け道が無い)
    if k == Key::Ctrl('c') {
        return Action::Quit;
    }
    match &mut ui.mode {
        Mode::Normal => {
            let cmd = normal_cmd(&k, &mut ui.pending_g);
            run_cmd(eng, ui, cmd)
        }
        Mode::Insert => {
            match k {
                Key::Esc => {
                    ui.mode = Mode::Normal;
                    say("入力モード終了");
                    return Action::None;
                }
                // fbrowse と同じく Enter で確定して抜ける (検索語を打って Enter したあとも
                // 入力モードのままだと、続けて押したキーまでページへ送ってしまう)
                Key::Enter => {
                    eng.send_key(&k);
                    ui.mode = Mode::Normal;
                    say("入力モード終了 (Enter)");
                    return Action::None;
                }
                Key::Char(_) => ui.typed += 1,
                Key::Backspace => ui.typed = ui.typed.saturating_sub(1),
                _ => {}
            }
            eng.send_key(&k);
            Action::None
        }
        Mode::Help => {
            ui.mode = Mode::Normal;
            term::show_cursor(false);
            Action::Redraw
        }
        Mode::Hint(h) => {
            match k {
                // f はラベルに使わない文字なので、fbrowse と同じく取消に使える
                Key::Esc | Key::Char('f') => {
                    eng.run_js(hints::CLEAR_JS);
                    ui.mode = Mode::Normal;
                    say("ヒント取消");
                }
                Key::Backspace => {
                    h.typed.clear();
                    eng.run_js(&hints::narrow_js(""));
                }
                Key::Char(c) if hints::LABEL_CHARS.contains(c.to_ascii_lowercase()) => {
                    h.typed.push(c.to_ascii_lowercase());
                    eng.run_js(&hints::narrow_js(&h.typed));
                    finish_hint(eng, ui);
                }
                Key::Char(c) => {
                    eng.run_js(hints::CLEAR_JS);
                    ui.mode = Mode::Normal;
                    say(&format!("ヒントに無い文字: {c}"));
                }
                _ => {}
            }
            Action::None
        }
        Mode::Prompt(open, buf) => {
            match k {
                Key::Char(c) => buf.push(c),
                Key::Backspace => {
                    buf.pop();
                }
                Key::Ctrl('u') => buf.clear(),
                Key::Enter | Key::Esc => {
                    if k == Key::Enter {
                        match (resolve(buf), *open) {
                            (Some(url), Open::Here) => {
                                say(&format!("開く: {url}"));
                                eng.load(url);
                            }
                            (Some(url), Open::NewTab) => {
                                say(&format!("新しいタブで開く: {url}"));
                                eng.open_tab(url);
                            }
                            (None, _) if buf.trim().is_empty() => {}
                            (None, _) => say(&format!("URL にできません: {buf}")),
                        }
                    }
                    ui.mode = Mode::Normal;
                    term::show_cursor(false);
                    return Action::Redraw;
                }
                _ => {}
            }
            show_prompt(*open, buf);
            Action::None
        }
    }
}

/// ヒントモードで、ラベルが 2 文字そろい、ヒントも届いていたらクリックしてノーマルモードへ戻る。
/// クリックしたのが入力欄なら、ノーマルモードではなく入力モードへ入る。
/// ヒントが届く前にラベルを打ち終えることもあるので、キーを押したときと spin のあとの両方で呼ぶ。
fn finish_hint(eng: &Engine, ui: &mut Ui) {
    let Mode::Hint(h) = &mut ui.mode else {
        return;
    };
    let typed = h.typed.clone();
    let Some(hints) = h.hints() else {
        return;
    };
    let mut editable = false;
    if hints.is_empty() {
        say("クリックできるものがありません");
    } else if typed.chars().count() < 2 {
        return;
    } else {
        eng.run_js(hints::CLEAR_JS);
        match hints.iter().find(|x| x.label == typed) {
            Some(hint) => {
                say(&format!(
                    "ヒント {typed} をクリック ({:.0}, {:.0}) {}",
                    hint.x,
                    hint.y,
                    hint.href.as_deref().unwrap_or("")
                ));
                eng.click(hint.x, hint.y);
                editable = hint.editable;
            }
            None => say(&format!("ヒント不一致: {typed}")),
        }
    }
    if editable {
        enter_insert(ui);
    } else {
        ui.mode = Mode::Normal;
    }
}

/// 入力モードへ入る。ヒントで入力欄を選んだときと、開いたページが入力欄にフォーカスを
/// 当てていたとき。
fn enter_insert(ui: &mut Ui) {
    ui.mode = Mode::Insert;
    ui.typed = 0;
    say("入力モード (Enter で確定、Esc で戻る)");
}

fn run_cmd(eng: &mut Engine, ui: &mut Ui, cmd: Cmd) -> Action {
    match cmd {
        Cmd::Scroll(dx, dy) => eng.scroll_by(dx, dy),
        Cmd::Hints => {
            let (h, slot) = HintMode::new();
            eng.show_hints(slot);
            ui.mode = Mode::Hint(h);
        }
        Cmd::Top => eng.scroll_to(false),
        Cmd::Bottom => eng.scroll_to(true),
        Cmd::Page(k) => eng.send_key(&k),
        Cmd::Back => eng.back(),
        Cmd::Forward => eng.forward(),
        Cmd::Reload => eng.reload(),
        Cmd::Redraw => eng.request_frame(),
        Cmd::DeleteTyped => {
            say(&format!("入力削除 ({} 文字)", ui.typed));
            for _ in 0..std::mem::take(&mut ui.typed) {
                eng.send_key(&Key::Backspace);
            }
        }
        Cmd::NewTab | Cmd::EditUrl => {
            let (open, init) = if cmd == Cmd::NewTab {
                (Open::NewTab, String::new())
            } else {
                (Open::Here, eng.view().url().map(|u| u.to_string()).unwrap_or_default())
            };
            show_prompt(open, &init);
            ui.mode = Mode::Prompt(open, init);
        }
        Cmd::Tab(step) => eng.switch_tab(step),
        Cmd::CloseTab => {
            if !eng.close_tab() {
                say("最後のタブは閉じません");
            }
        }
        Cmd::CopyUrl => {
            let url = eng.view().url().map(|u| u.to_string()).unwrap_or_default();
            if tmux::copy_buffer(&url) {
                say(&format!("tmux バッファへコピー: {url}"));
            } else {
                say(&format!("tmux の外なのでコピーできません: {url}"));
            }
        }
        Cmd::Zoom(d) => {
            let z = eng.zoom(d);
            say(&format!("ズーム {}%", (z * 100.0).round()));
        }
        Cmd::Help => {
            term::show_cursor(false);
            term::show_text(&format!("{USAGE}\n\n(何かキーを押すと戻ります)"));
            ui.mode = Mode::Help;
        }
        Cmd::Quit => return Action::Quit,
        Cmd::NotYet(what, item) => say(&format!("{what} はまだありません (ROADMAP.md {item})")),
        Cmd::NotNeeded(what) => say(&format!("{what}: fbservo では不要")),
        Cmd::None => {}
    }
    Action::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_follow_fbrowse() {
        let mut g = false;
        let mut cmd = |c: char| normal_cmd(&Key::Char(c), &mut g);
        assert_eq!(cmd('j'), Cmd::Scroll(0.0, LINE));
        assert_eq!(cmd('b'), Cmd::Back);
        assert_eq!(cmd('t'), Cmd::NewTab);
        assert_eq!(cmd('Q'), Cmd::Quit);
        assert_eq!(cmd('f'), Cmd::Hints);
        assert_eq!(cmd('q'), Cmd::None);
        // 入力モードはヒントで入力欄を選んで入るので、i は使わない
        assert!(matches!(cmd('i'), Cmd::NotNeeded(_)));
        // gg で先頭。間に別のキーが入ったら数え直す
        assert_eq!(cmd('g'), Cmd::None);
        assert_eq!(cmd('g'), Cmd::Top);
        assert_eq!(cmd('g'), Cmd::None);
        assert_eq!(cmd('j'), Cmd::Scroll(0.0, LINE));
        assert_eq!(cmd('g'), Cmd::None);
        assert_eq!(cmd('G'), Cmd::Bottom);
        assert_eq!(normal_cmd(&Key::Enter, &mut g), Cmd::Page(Key::Enter));
    }

    #[test]
    fn resolves_inputs() {
        assert_eq!(resolve("https://a.b/C").unwrap().as_str(), "https://a.b/C");
        assert_eq!(resolve("example.com/Path").unwrap().as_str(), "https://example.com/Path");
        assert_eq!(resolve("rust 言語").unwrap().as_str(), "https://duckduckgo.com/?q=rust%20%E8%A8%80%E8%AA%9E");
        assert_eq!(resolve("/").unwrap().as_str(), "file:///");
        assert!(resolve("  ").is_none());
    }
}
