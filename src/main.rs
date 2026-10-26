//! fbservo: Servo (Rust 製ブラウザエンジン) でページを描き、/dev/fb0 (drmterm の中なら
//! その共有メモリ) へ直接書く試作。
//!
//! 動作中は端末に何も出さない (出すとページの絵を上書きする)。ログは
//! $XDG_RUNTIME_DIR/fbservo.log に書き、終了時にそのパスを表示する。
//! cookie などは $XDG_DATA_HOME/fbservo (既定は ~/.local/share/fbservo) に残す。

mod drmterm;
mod engine;
mod fb;
mod hints;
mod kbd;
mod scan;
mod smooth;
mod term;
mod tmux;

use anyhow::{bail, Context, Result};
use engine::Engine;
use fb::{Fb, Image, Rect};
use hints::HintMode;
use kbd::{Kbd, Press};
use smooth::{Pace, Smooth};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use term::{Key, RawMode};
use url::Url;

const USAGE: &str = "\
使い方: fbservo [--full] [URL | ファイル | 検索語]

tmux の中では自分のペインに描き、ペインの移動・大きさの変化・ウィンドウの切り替えに
ついていく (fbrowse の --auto と同じ)。--full を付けると画面全体に描く。
drmterm の中では、drmterm の画面のペインに描く (--full でもペインを追う)。

キー (fbrowse と同じ配置):
  hjkl          スクロール            gg / G       先頭 / 末尾
  f             リンクヒント (ラベルの 2 文字でクリック。f / Esc で取消)
                入力欄を選ぶとそのまま入力 (Enter で確定、Esc で戻る)
  Ctrl-d/Ctrl-u 1 画面 下 / 上        Space        そのままページへ送る (矢印も)
  b / r         戻る / 再読込
  + - 0         ズームイン / アウト / リセット
  t             新しいタブ            ] / [        次 / 前のタブ
  x             タブを閉じる          y            URL を tmux バッファへコピー
  ?             このヘルプ            Q            終了 (Ctrl-C はどのモードでも)
fbservo だけのキー:
  L             進む                  O            今の URL を編集して開く
まだ無いもの (ROADMAP.md): m カーソル, o お気に入り, p クリップボード,
  T タブを別ペインへ";

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

/// cookie・HSTS・Web Storage を残す場所。次に起動したときも、cookie の同意や
/// DuckDuckGo の設定が残っている。$XDG_DATA_HOME/fbservo か ~/.local/share/fbservo。
/// 相対パスは使わない (HOME が無いときに、起動したディレクトリへ cookie を書いてしまう)。
fn profile_dir(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let abs = |v: Option<OsString>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    let data = abs(xdg_data_home).or_else(|| abs(home).map(|h| h.join(".local/share")))?;
    Some(data.join("fbservo"))
}

/// 保存先を用意する。cookie はログインの印そのもので、読めればパスワード無しでなりすませるので、
/// ディレクトリには自分しか入れないようにする (前からあるディレクトリも毎回直す)。
fn prepare_profile(profile: &Path) -> Result<()> {
    std::fs::create_dir_all(profile).with_context(|| format!("{} を作れません", profile.display()))?;
    std::fs::set_permissions(profile, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("{} を自分だけのものにできません", profile.display()))?;
    forget_passwords(profile)
}

/// Basic 認証の ID とパスワードは残さない。Servo は終了するときに auth_cache.json へ平文で
/// 書き出すので、そこを /dev/null へのリンクにして、書いても捨てられるようにする
/// (動いている間はメモリに持つので、認証はその回の間だけ効く)。
fn forget_passwords(profile: &Path) -> Result<()> {
    let path = profile.join("auth_cache.json");
    if std::fs::read_link(&path).is_ok_and(|t| t == Path::new("/dev/null")) {
        return Ok(());
    }
    let _ = std::fs::remove_file(&path);
    std::os::unix::fs::symlink("/dev/null", &path).with_context(|| format!("{} を /dev/null に向けられません", path.display()))
}

/// 終わりの合図 (SIGHUP: ペインやウィンドウを閉じた、SIGTERM) を poll で受け取る fd。
/// 合図でそのまま死ぬと、Servo が cookie を書き出さない (書くのは終了するときだけ) ので、
/// Q と同じく後片付けをして終わる。Servo などのスレッドを作る前に呼ぶ (合図を止めた
/// マスクがスレッドに引き継がれ、どのスレッドにも配られなくなる)。
fn quit_signals() -> RawFd {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGHUP);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC)
    }
}

fn log_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    Path::new(&dir).join("fbservo.log")
}

enum Mode {
    Normal,
    /// キーをそのままページへ送る (リンクヒントで入力欄を選ぶと入る)
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

/// 操作の結果をログに残す (絵には何も重ねない。ROADMAP.md の「作らないもの」)。
fn say(msg: &str) {
    eprintln!("[ui] {msg}");
}

fn main() {
    let mut full = false;
    let mut target = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            "--full" => full = true,
            // fbrowse と同じ指定でも起動できるように受け付ける (tmux の中ならもともとペインを追う)
            "--auto" => full = false,
            _ => target = Some(target.map_or(a.clone(), |t: String| format!("{t} {a}"))),
        }
    }

    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let quit_fd = quit_signals();

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

    let result = run(&mut fb, target.as_deref(), full, quit_fd);

    unsafe { libc::dup2(orig_stderr, 2) };
    // ペインを閉じて終わったときは端末がもう無い (書けなくても止まらないよう、print! は使わない)
    let mut out = std::io::stdout();
    // 消してから出す (改行が無いとたまったままになり、あとで出すエラーまで消してしまう)
    let _ = write!(out, "\x1b[2J\x1b[H");
    let _ = out.flush();
    if let Err(e) = &result {
        let _ = writeln!(std::io::stderr(), "エラー: {e:#}");
    }
    let _ = writeln!(out, "ログ: {}", log.display());
    if result.is_err() {
        std::process::exit(1);
    }
}

fn run(fb: &mut Fb, target: Option<&str>, full: bool, quit_fd: RawFd) -> Result<()> {
    let (wake_fd, waker) = engine::pipe_waker()?;
    let first = target.and_then(resolve);
    let mode = if first.is_some() { Mode::Normal } else { Mode::Prompt(Open::Here, String::new()) };
    let mut ui = Ui { mode, pending_g: false, smooth: Smooth::default(), kbd: Kbd::open() };
    if let Some(s) = fb.drmterm_keys() {
        ui.kbd.add_drmterm(s);
    }
    let url = first.unwrap_or_else(|| Url::parse("about:blank").unwrap());

    // drmterm の中なら、ペインの位置は drmterm が教える升目の大きさと、drmterm 上の tmux クライアントから求める
    let term = fb.drmterm().map(|i| tmux::Term { cell: (i.cell_w, i.cell_h), client: i.tty.clone() });
    if let Some(i) = fb.drmterm() {
        eprintln!(
            "[drmterm] {} へ描く ({}x{}、升目 {}x{}、tty {})",
            i.path,
            i.width,
            i.height,
            i.cell_w,
            i.cell_h,
            i.tty.as_deref().unwrap_or("不明")
        );
    }
    // tmux の中なら自分のペインに描き、ペインが動いたり隠れたりしたら追いかける。
    // drmterm では画面全体に描くと端末が隠れたままになるので、--full でもペインを追う (fbrowse と同じ)
    let follow = (term.is_some() || !full) && tmux::inside();
    // /dev/fb0 は内蔵画面そのもの。内蔵画面に映っていない所 (別の tmux サーバや ssh のペイン、
    // ほかのプログラムから起動されたとき) から描くと、内蔵画面に映っているものの上に絵を重ねて
    // しまう。ペインを追うなら、内蔵画面に映っている間だけ描く (tmux::pane)。追わないなら、
    // 起動した時点で内蔵画面に映っていなければ描かずに終わる
    let console = fb.is_console();
    if console && !follow && !on_console() {
        bail!("内蔵画面 (fbterm かコンソール) に映っていないので /dev/fb0 には描きません (ファイルへ描くなら FBSERVO_FB)");
    }
    let pane = if follow { tmux::pane(fb.width, fb.height, term.as_ref(), console) } else { None };
    if follow && pane.is_none() {
        eprintln!("[pane] ペインの位置が分からないので画面全体に描きます");
    }
    // 描く場所と、そこがいま画面に出ているか (内蔵画面に描くのにペインの様子が分からなければ、分かるまで描かない)
    let mut area = pane.as_ref().map_or(fb.full(), |p| p.rect);
    let mut shown = pane.as_ref().map_or(!(follow && console), |p| p.shown);
    let panes = follow.then(|| {
        let w = waker.clone_box();
        tmux::follow(fb.width, fb.height, term.clone(), console, move || w.wake())
    });
    // 端末 (tmux の時計など) に上書きされた絵を描き直すか。drmterm の共有メモリは端末に上書きされない
    let repaint = term.is_none();
    // 入力欄を出している間に drmterm で空けておく、端末の最下行の高さ
    let cell_h = term.as_ref().map_or(0, |t| t.cell.1);
    // 一度でも描いたか (それまでは drmterm の共有メモリに前のクライアントの絵が残っている)
    let mut drawn = false;
    eprintln!("[start] {url} ({}x{} at {},{})", area.w, area.h, area.x, area.y);

    let profile = profile_dir(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
        .context("HOME が無いので、cookie などを残す場所が決まりません")?;
    prepare_profile(&profile)?;
    eprintln!("[profile] {}", profile.display());

    let _raw = RawMode::enter();
    let mut eng = Engine::new(area.w, area.h, url, waker, profile)?;
    if let Mode::Prompt(open, buf) = &ui.mode {
        show_prompt(*open, buf);
    }

    // 最後に描いたフレーム。端末 (tmux の時計など) に上書きされても描き直せるよう取っておく。
    let mut last: Option<Image> = None;
    // 次のフレームを読み出す入れ物 (前の前のフレームのものを使い回す)
    let mut spare = Vec::new();
    let mut last_blit = Instant::now();
    eprintln!("[scan] {}", fb.describe());
    // スクロール中のコマの間隔 (垂直同期の何回分か) と、直前のコマを出した時刻
    let mut pace = Pace::new(fb.period());
    let mut presented: Option<Instant> = None;
    let mut blit_took = Duration::ZERO;
    // 次のフレームが届いたのに気づいた時刻 (前のフレームを fb0 に書くまで待っている間に気づいたとき)
    let mut ready_at: Option<Instant> = None;
    // まだ画面に反映していない最初のスクロールキーを押した時刻
    let mut key_at: Option<Instant> = None;
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 256];
    let start = Instant::now();
    let ms = |t: Instant| (t - start).as_secs_f64() * 1000.0;

    loop {
        let timeout = if !shown {
            1000
        } else if eng.frame_pending() {
            // 前のフレームを書くまで待つ間に (wait_spinning)、次のフレームがもう届いている。
            // その通知の pipe は読んでしまったので、待つと次の通知まで止まる
            0
        } else {
            let base = if eng.animating() { 16 } else { 1000 };
            ui.smooth.timeout().map_or(base, |t| t.min(base))
        };
        let mut fds: Vec<libc::pollfd> = [0, wake_fd, quit_fd]
            .into_iter()
            .chain(ui.kbd.fds())
            .map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 })
            .collect();
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if fds[1].revents & libc::POLLIN != 0 {
            drain(wake_fd);
        }
        // 端末の文字より先に読む (文字が届いたとき、そのキーがまだ押されているか分かるように)
        read_kbd(&mut ui);

        let mut redraw = false;
        if let Some(p) = panes.as_ref().and_then(|rx| rx.try_iter().last()) {
            if p.rect != area {
                eprintln!("[pane] {}x{} at {},{}", p.rect.w, p.rect.h, p.rect.x, p.rect.y);
                let inside = |a: Rect, b: Rect| a.x >= b.x && a.y >= b.y && a.x + a.w <= b.x + b.w && a.y + a.h <= b.y + b.h;
                let old = std::mem::replace(&mut area, p.rect);
                eng.resize(area.w, area.h);
                // 前の大きさの絵はもう描かない
                last = None;
                if !inside(old, area) && repaint {
                    // 縮んだり動いたりした跡は隣のペインのものになった。気づくまでの間に
                    // 上書きしてしまった分を、tmux に端末の文字で描き直してもらう
                    tmux::refresh_clients();
                }
            }
            if p.shown != shown {
                shown = p.shown;
                eprintln!("[pane] {}", if shown { "表示" } else { "非表示 (描画を止める)" });
                // ウィンドウを切り替えて戻ってきたら、tmux が描いた端末の文字の上にすぐ描き直す
                redraw = shown;
            }
        }
        // 終わりの合図を受けたら、端末はもう読まない (ペインを閉じたなら読めない)
        let mut quit = fds[2].revents & libc::POLLIN != 0;
        if quit {
            eprintln!("[quit] 終わりの合図 (SIGHUP / SIGTERM) を受けました");
        }
        if !quit && fds[0].revents & libc::POLLIN != 0 {
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
                        quit = true;
                        break;
                    }
                }
            }
        }
        // 戻ると eng が drop され、Servo が cookie などを書き出して終わる
        if quit {
            fb.set_rect(None);
            // 隠れている間は、自分の絵はもう画面に無い (消すと映っている別のものを塗りつぶす)
            if shown && drawn {
                fb.clear(area);
            }
            if follow && repaint {
                tmux::refresh_clients();
            }
            return Ok(());
        }
        // スクロールの流し始め (流している間は、コマを描いたところで次の分を送る)
        if let Some((dx, dy)) = ui.smooth.next(Instant::now(), pace.interval()) {
            eng.scroll_by(dx, dy);
        }
        // キー処理のあとに回す。スクロールは Servo 内のキューに積まれるだけで、
        // spin_event_loop まで処理されない (waker も呼ばれないので、先に回すと次の
        // poll のタイムアウトまで最大 1 秒待たされる)。
        eng.spin();
        // ヒントの JS の結果は spin の中で届く。先に打ったラベルがあればここでクリックする
        finish_hint(&eng, &mut ui);
        // 開いたページが入力欄にフォーカスを当てていたら (検索欄など)、外す。入力モードには
        // リンクヒントで入力欄を選んだときだけ入る (読み込みを終える前に選んで打っていたら外さない)
        if eng.take_loaded() && !matches!(ui.mode, Mode::Insert) {
            eng.run_js(hints::BLUR_JS);
        }
        fb.set_rect(drm_rect(&ui.mode, area, shown, drawn, cell_h));
        // 隠れている間は描かない。届いたフレームは表示に戻ったときに描く
        if ui.mode.uses_terminal() || !shown {
            continue;
        }

        if eng.take_frame() {
            let t0 = Instant::now();
            // Servo がこのコマを組み終えた時刻
            let built = ready_at.take().unwrap_or(t0);
            eng.paint();
            let t1 = Instant::now();
            // スクロールの次の分は paint のすぐあとに送り、読み出しと垂直同期を待つ間に Servo に
            // 次のコマを組ませる (長いページでは組むのに 20ms 以上かかり、読み出しのあとに
            // 送ると 1 コマが 33ms にも収まらない)
            let shown_step = ui.smooth.shown();
            // 垂直同期を待っている間にキーを離していたら、もう送らない
            read_kbd(&mut ui);
            if let Some((dx, dy)) = ui.smooth.next(Instant::now(), pace.interval()) {
                eng.scroll_by(dx, dy);
                eng.spin();
            }
            let t1b = Instant::now();
            let img = eng.read(std::mem::take(&mut spare));
            let t2 = Instant::now();
            if let Some(img) = img {
                // スクロール中は、前のコマからそのコマの分の間隔をあけて出す
                // (コマごとに 1 コマ分ずつ進めているので、出す間隔が揺れると速さも揺れて見える)
                if let Some((sent, _)) = shown_step {
                    let wait_servo = built.saturating_duration_since(sent) + (t1 - t0);
                    pace.add(wait_servo.max((t1 - t0) + (t2 - t1b) + blit_took));
                }
                let target = match (shown_step, presented) {
                    (Some((_, interval)), Some(p)) => Some(p + interval),
                    _ => None,
                };
                if let Some(at) = fb.write_at(&img, area, target) {
                    wait_spinning(&mut eng, wake_fd, at, &mut ready_at);
                }
                let out = fb.present(&img, area, target);
                drawn = true;
                // 間に合わず、決めた間隔より遅れて出た分 (ログ用)
                let late = target.map_or(Duration::ZERO, |v| out.vblank.saturating_duration_since(v));
                let t3 = out.start;
                last_blit = Instant::now();
                blit_took = out.took;
                presented = (shown_step.is_some() && ui.smooth.active()).then_some(out.vblank);
                if let Some(old) = last.replace(img) {
                    spare = old.data;
                }
                if let Some(t) = key_at.take() {
                    eprintln!("[key] 押してから {:.1}ms で表示", (last_blit - t).as_secs_f64() * 1000.0);
                }
                let d = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1000.0;
                eprintln!(
                    "[frame] t={:.0} paint {:.1}ms read {:.1}ms vsync {:.1}ms blit {:.1}ms{}{}",
                    ms(t0),
                    d(t0, t1),
                    d(t1b, t2),
                    d(t2, t3),
                    d(t3, last_blit),
                    shown_step.map_or(String::new(), |(_, i)| format!(" scroll {:.0}ms", i.as_secs_f64() * 1000.0)),
                    if late > fb.period() / 2 { format!(" late +{:.0}ms", late.as_secs_f64() * 1000.0) } else { String::new() }
                );
            }
        } else if redraw || (repaint && last_blit.elapsed() >= Duration::from_secs(1)) {
            if let Some(img) = &last {
                fb.draw(img, area);
            }
            last_blit = Instant::now();
        }
        // 初めて描いたら、drmterm に重ねてもらう
        fb.set_rect(drm_rect(&ui.mode, area, shown, drawn, cell_h));
    }
    Ok(())
}

/// drmterm に申告する、絵を重ねてほしい矩形 (fb0 では使わない)。端末に文字を出している間は
/// そこを空け (入力欄は最下行、キー一覧は全体)、ペインが隠れている間と、まだ何も描いていない間
/// (共有メモリに前のクライアントの絵が残っている) は取り下げる。ペインの大きさが変わった直後は、
/// 次のコマが届くまで前の絵 (ずれている) を重ねたままにする (取り下げると端末の文字がちらつく)。
fn drm_rect(mode: &Mode, area: Rect, shown: bool, drawn: bool, cell_h: u32) -> Option<Rect> {
    match mode {
        _ if !shown || !drawn => None,
        Mode::Help => None,
        Mode::Prompt(..) => Some(Rect { h: area.h.saturating_sub(cell_h), ..area }),
        _ => Some(area),
    }
}

/// スクロールキーを押してからこれだけ経ってもフレームが来なければ、画面が変わらない
/// キーだったとみなす (いつもは 30〜50ms で出る)。
const KEY_STALE: Duration = Duration::from_millis(500);

/// until まで、Servo のイベントループを回しながら待つ。次のフレームが届いたら、気づいた時刻を
/// ready に残す。fb0 に書く時刻まで眠ってしまうと、スクロールを送ってから次のフレームが
/// できるまでの時間が分からない (Servo の waker が呼ばれた時刻では代わりにならない。
/// アニメーションのあるページではタイマーで 8ms ごとに呼ばれる)。
fn wait_spinning(eng: &mut Engine, wake_fd: RawFd, until: Instant, ready: &mut Option<Instant>) {
    // 回すのに時間がかかって書く時刻に遅れないよう、最後の 1ms は回さずに眠る
    const LAST: Duration = Duration::from_millis(1);
    loop {
        let now = Instant::now();
        if ready.is_none() && eng.frame_pending() {
            *ready = Some(now);
        }
        let left = until.saturating_duration_since(now);
        if left <= LAST {
            std::thread::sleep(left);
            return;
        }
        let mut fd = libc::pollfd { fd: wake_fd, events: libc::POLLIN, revents: 0 };
        let wait = libc::timespec { tv_sec: 0, tv_nsec: (left - LAST).as_nanos().min(999_999_999) as libc::c_long };
        unsafe { libc::ppoll(&mut fd, 1, &wait, std::ptr::null()) };
        if fd.revents & libc::POLLIN != 0 {
            drain(wake_fd);
            eng.spin();
        }
    }
}

/// キーボードを読み、押したまま流していたキーを離していたら止める。
fn read_kbd(ui: &mut Ui) {
    ui.kbd.read(Instant::now());
    if let Some(c) = ui.smooth.held() {
        if !ui.kbd.is_down(c) {
            ui.smooth.release();
        }
    }
}

/// 起動した時点で、自分が内蔵画面に映っているか (tmux の中なら、自分のセッションを内蔵画面の
/// クライアントが映しているか)
fn on_console() -> bool {
    if tmux::inside() {
        return tmux::console_client().is_some_and(|c| tmux::shows_session(&c, &tmux::own_session_name()));
    }
    let tty = std::fs::read_link("/proc/self/fd/0").map(|p| p.display().to_string()).unwrap_or_default();
    tmux::on_console(std::process::id(), &tty)
}

fn drain(fd: RawFd) {
    let mut b = [0u8; 64];
    while unsafe { libc::read(fd, b.as_mut_ptr().cast(), b.len()) } > 0 {}
}

/// 押してから画面に出るまでを計る、ページを動かすキー
fn is_scroll_key(k: &Key) -> bool {
    matches!(
        k,
        Key::Char('j' | 'k' | 'h' | 'l' | ' ')
            | Key::Ctrl('d' | 'u')
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
    /// hjkl のスクロールの残り
    smooth: Smooth,
    /// hjkl を押したままか (キーボードから読む)
    kbd: Kbd,
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
    /// hjkl (押したキー, 1 回分の量)
    Scroll(char, f32, f32),
    Hints,
    Top,
    Bottom,
    /// 1 画面 下 (1) / 上 (-1) へ流す
    Screen(f32),
    /// Space・矢印などをそのままページへ送る
    Page(Key),
    Back,
    Forward,
    Reload,
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
        Key::Char('h') => Cmd::Scroll('h', -LINE, 0.0),
        Key::Char('j') => Cmd::Scroll('j', 0.0, LINE),
        Key::Char('k') => Cmd::Scroll('k', 0.0, -LINE),
        Key::Char('l') => Cmd::Scroll('l', LINE, 0.0),
        Key::Char('g') if after_g => Cmd::Top,
        Key::Char('g') => {
            *pending_g = true;
            Cmd::None
        }
        Key::Char('G') => Cmd::Bottom,
        Key::Ctrl('d') => Cmd::Screen(1.0),
        Key::Ctrl('u') => Cmd::Screen(-1.0),
        // 入力欄の外では使い道が無い (入力欄のフォーカスはヒントで選んだときにしか無い)
        Key::Enter => Cmd::NotNeeded("Enter (スクロールは Ctrl-d / Ctrl-u)"),
        Key::Char(' ')
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
        Key::Char('m') => Cmd::NotYet("カーソルモード", "4-1"),
        Key::Char('o') => Cmd::NotYet("お気に入りに追加", "2-4"),
        Key::Char('p') => Cmd::NotYet("クリップボード表示", "2-6"),
        Key::Char('T') => Cmd::NotYet("タブを別ペインへ移す", "3-2"),
        Key::Char('d') => Cmd::NotNeeded("直前の入力の削除 (入力モードの Backspace で消す)"),
        Key::Char('s') => Cmd::NotNeeded("撮り直し (絵は自分で描き直す。崩れたら r で読み直す)"),
        Key::Char('c' | 'C') => Cmd::NotNeeded("動画モード (fbservo は常に動く絵を描いている)"),
        Key::Char('M' | 'P') => Cmd::NotNeeded("mpv で再生 (動画は見ない)"),
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
                    eng.run_js(hints::BLUR_JS);
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

/// 入力モードへ入る (ヒントで入力欄を選んだとき)。
fn enter_insert(ui: &mut Ui) {
    ui.mode = Mode::Insert;
    say("入力モード (Enter で確定、Esc で戻る)");
}

fn run_cmd(eng: &mut Engine, ui: &mut Ui, cmd: Cmd) -> Action {
    // hjkl・Ctrl-d・Ctrl-u 以外の操作をしたら、まだ送っていないスクロールは捨てる (gg のあとに流れ続けないように)
    if !matches!(cmd, Cmd::Scroll(..) | Cmd::Screen(_) | Cmd::None) {
        ui.smooth.stop();
    }
    match cmd {
        // 押したままなら離すまで流し、すぐ離していたら 1 回分進める。キーリピートの文字は、
        // キーボードから押したままと分かっていれば無視する (分からなければ 1 回分ずつ足す)
        Cmd::Scroll(key, dx, dy) => match ui.kbd.press(key, Instant::now()) {
            Press::Held => ui.smooth.hold(key, dx, dy),
            Press::Tap | Press::Unknown => ui.smooth.add(dx, dy),
            Press::Repeat => {}
        },
        Cmd::Screen(dir) => ui.smooth.page(0.0, dir * eng.height as f32),
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
        assert_eq!(cmd('j'), Cmd::Scroll('j', 0.0, LINE));
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
        assert_eq!(cmd('j'), Cmd::Scroll('j', 0.0, LINE));
        assert_eq!(cmd('g'), Cmd::None);
        assert_eq!(cmd('G'), Cmd::Bottom);
        assert_eq!(normal_cmd(&Key::Ctrl('d'), &mut g), Cmd::Screen(1.0));
        assert_eq!(normal_cmd(&Key::Ctrl('u'), &mut g), Cmd::Screen(-1.0));
        assert!(matches!(normal_cmd(&Key::Enter, &mut g), Cmd::NotNeeded(_)));
        assert_eq!(normal_cmd(&Key::Char(' '), &mut g), Cmd::Page(Key::Char(' ')));
    }

    #[test]
    fn profile_dir_is_absolute() {
        let p = |x: &str, h: &str| profile_dir(Some(x.into()), Some(h.into()));
        assert_eq!(p("/x", "/h"), Some(PathBuf::from("/x/fbservo")));
        // 空や相対の XDG_DATA_HOME は使わない (XDG の決まり)
        assert_eq!(p("", "/h"), Some(PathBuf::from("/h/.local/share/fbservo")));
        assert_eq!(p("x", "/h"), Some(PathBuf::from("/h/.local/share/fbservo")));
        // HOME も無ければ決めない (起動したディレクトリに書かない)
        assert_eq!(p("", ""), None);
        assert_eq!(profile_dir(None, None), None);
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
