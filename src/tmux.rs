//! tmux への問い合わせ (fbrowse の tmux.rs から必要な分を写したもの)。
//!
//! ペイン追従: 自分のペインが画面のどこにあり、いま表示されているかを調べる。

use crate::fb::Rect;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

/// tmux を実行して標準出力 (末尾の改行を除く) を返す。失敗なら None。
pub fn run(args: &[&str]) -> Option<String> {
    let out = Command::new("tmux").args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

pub fn inside() -> bool {
    std::env::var("TMUX").map(|v| !v.is_empty()).unwrap_or(false)
}

/// 自分のペイン ($TMUX_PANE) を対象にした display-message。client を渡すと、そのクライアントから見た値になる。
fn display(fmt: &str, client: Option<&str>) -> Option<String> {
    if !inside() {
        return None;
    }
    let mut args = vec!["display-message", "-p"];
    if let Some(c) = client {
        args.extend(["-c", c]);
    }
    let own = std::env::var("TMUX_PANE").ok().filter(|s| !s.is_empty());
    if let Some(p) = own.as_deref() {
        args.extend(["-t", p]);
    }
    args.push(fmt);
    run(&args)
}

/// 自分のペインのセッション名 (tmux の外なら空)。
pub fn own_session_name() -> String {
    display("#{session_name}", None).unwrap_or_default()
}

/// tty のクライアントが、いま session を表示しているか。
pub fn shows_session(tty: &str, session: &str) -> bool {
    if tty.is_empty() || session.is_empty() {
        return false;
    }
    let Some(out) = run(&["list-clients", "-F", "#{client_tty}\t#{client_session}"]) else {
        return false;
    };
    out.lines().any(|l| l.split_once('\t') == Some((tty, session)))
}

/// pid のプロセスが内蔵画面の端末で動いているか。fbterm の子孫か、コンソール (/dev/ttyN) にいれば真。
/// tmux のサーバはデーモンになって fbterm から切り離されるので、ペインの中のプロセスはこれでは
/// 決まらない (そのペインを映している tmux クライアントで決める、`console_client`)。
pub fn on_console(pid: u32, tty: &str) -> bool {
    let vt = tty.strip_prefix("/dev/tty").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    vt || ancestors(pid).iter().any(|comm| comm == "fbterm")
}

/// pid から init の手前までの、各プロセスの名前
fn ancestors(mut pid: u32) -> Vec<String> {
    let mut names = Vec::new();
    // 輪になることはないが、念のため打ち切る
    while pid > 1 && names.len() < 64 {
        let (Ok(comm), Ok(stat)) =
            (std::fs::read_to_string(format!("/proc/{pid}/comm")), std::fs::read_to_string(format!("/proc/{pid}/stat")))
        else {
            break;
        };
        names.push(comm.trim_end().to_string());
        let Some(ppid) = parent_of(&stat) else { break };
        pid = ppid;
    }
    names
}

/// /proc/<pid>/stat の親の pid。名前 (括弧の中) に空白や括弧が入っていても読めるよう、最後の ')' の後ろから読む
fn parent_of(stat: &str) -> Option<u32> {
    stat.get(stat.rfind(')')? + 1..)?.split_whitespace().nth(1)?.parse().ok()
}

/// 内蔵画面に映っている tmux クライアント (fbterm の中か、コンソールで動いているもの) の tty。
/// この tmux サーバにつながっていなければ None
pub fn console_client() -> Option<String> {
    let out = run(&["list-clients", "-F", "#{client_pid}\t#{client_tty}"])?;
    out.lines()
        .filter_map(|l| l.split_once('\t'))
        .find(|(pid, tty)| pid.parse().is_ok_and(|p| on_console(p, tty)))
        .map(|(_, tty)| tty.to_string())
}

/// 文字列を tmux のペーストバッファへ入れる (prefix+] で貼り付けられる)。
/// `-w` で set-clipboard 経由のシステムクリップボード (OSC 52) にも送る。
pub fn copy_buffer(text: &str) -> bool {
    if !inside() {
        return false;
    }
    run(&["set-buffer", "-w", "--", text]).is_some() || run(&["set-buffer", "--", text]).is_some()
}

/// 接続中の全 tmux クライアントに画面全体の再描画を頼む
/// (ペインが動いたあとや終了時に、自分が描いた絵の跡を端末の文字で上書きしてもらう)。
pub fn refresh_clients() {
    if let Some(out) = run(&["list-clients", "-F", "#{client_name}"]) {
        for name in out.lines().filter(|l| !l.is_empty()) {
            let _ = run(&["refresh-client", "-t", name]);
        }
    }
}

/// 自分のペインの画面上の位置。
#[derive(Clone, Debug, PartialEq)]
pub struct Pane {
    pub rect: Rect,
    /// いま画面に出ているか。別のウィンドウやセッションに切り替わっている、
    /// 同じウィンドウの別のペインがズームされている、のどれかなら false
    pub shown: bool,
}

/// drmterm に映っているときの端末の情報 (src/drmterm.rs)。
#[derive(Clone, Debug)]
pub struct Term {
    /// 升目の大きさ (px)。drmterm は実際の値を教えてくれる
    pub cell: (u32, u32),
    /// drmterm 上の tmux クライアントの tty (分からなければ None)
    pub client: Option<String>,
}

/// 自分のペイン ($TMUX_PANE) の位置をピクセルで返す。tmux の外や、問い合わせに
/// 失敗したときは None。セルの大きさは、drmterm に映っていればその値を使い、
/// そうでなければ画面の大きさを端末の行数・列数で割って求める。
/// console なら (/dev/fb0 へ描くとき)、内蔵画面に映っているクライアントから見る。そのクライアントが
/// いなければ (別の tmux サーバや ssh のペイン)、どこかに映っていても内蔵画面には映っていないので隠れているとする。
pub fn pane(fb_w: u32, fb_h: u32, term: Option<&Term>, console: bool) -> Option<Pane> {
    let fmt = "#{window_active},#{pane_left},#{pane_top},#{pane_width},#{pane_height},\
               #{window_width},#{window_height},#{client_width},#{client_height},#{status-position},\
               #{session_attached},#{window_zoomed_flag},#{pane_active},#{client_session},#{session_name}";
    let client = match term {
        Some(t) => t.client.clone(),
        None if console => console_client(),
        None => None,
    };
    let mut p = parse_pane(&display(fmt, client.as_deref())?, fb_w, fb_h, term.map(|t| t.cell), client.is_some())?;
    if console && term.is_none() && client.is_none() {
        p.shown = false;
    }
    Some(p)
}

/// own_client: 値が drmterm 上のクライアントから見たものか (そのクライアントが自分の
/// セッションを映していなければ、ペインは隠れている)。
fn parse_pane(out: &str, fb_w: u32, fb_h: u32, cell: Option<(u32, u32)>, own_client: bool) -> Option<Pane> {
    let parts: Vec<&str> = out.split(',').collect();
    let n = |i: usize| parts.get(i).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let (active, pl, pt, pw, ph, ww, wh, cw_cells, ch_cells) = (n(0), n(1), n(2), n(3), n(4), n(5), n(6), n(7), n(8));
    let status_pos = parts.get(9).copied().filter(|s| !s.is_empty()).unwrap_or("bottom");
    let attached = parts.get(10).and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
    let (zoomed, pane_active) = (n(11), n(12));
    let on_client = !own_client || parts.get(13).is_some_and(|c| Some(c) == parts.get(14));
    if ww == 0 || wh == 0 {
        return None;
    }
    // セルの大きさは、ステータスバーを含む端末全体の行列数から求める
    // (window_height はステータスバーの分だけ小さいので、それで割るとセルが大きすぎる)。
    let cols = if cw_cells > 0 { cw_cells } else { ww };
    let rows = if ch_cells > 0 { ch_cells } else { wh };
    let (cw, ch) = match cell {
        Some(c) => c,
        // 画面の下を別のもの (task-var のバーなど) が占めていると端末の行数だけが減り、
        // 画面の高さ / 行数ではセルが大きすぎる。セルの高さは幅の 2 倍までに抑える。
        None => {
            let cw = fb_w / cols;
            (cw, (fb_h / rows).min(cw * 2))
        }
    };
    if cw == 0 || ch == 0 {
        return None;
    }
    // drmterm では uim-fep が最下行を使うので、端末の行数は画面の高さ / セルより少ない。
    // drmterm 上のクライアントの値が無ければ、画面から割り出す
    let rows = if cell.is_some() && !(own_client && ch_cells > 0) { fb_h / ch } else { rows };
    // pane_top はウィンドウの中での位置なので、ステータスバーが上ならその分下へずらす
    let y_off = if status_pos == "top" { rows.saturating_sub(wh) * ch } else { 0 };
    Some(Pane {
        rect: Rect { x: pl * cw, y: y_off + pt * ch, w: pw * cw, h: ph * ch },
        shown: active == 1 && attached > 0 && (zoomed == 0 || pane_active == 1) && on_client,
    })
}

/// 別スレッドで 500ms ごとに自分のペインを調べ、変わったら送って wake を呼ぶ
/// (tmux の起動に数 ms かかるので、メインループでは待たない)。
pub fn follow(fb_w: u32, fb_h: u32, term: Option<Term>, console: bool, wake: impl Fn() + Send + 'static) -> Receiver<Pane> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut last = None;
        loop {
            if let Some(p) = pane(fb_w, fb_h, term.as_ref(), console) {
                if last.as_ref() != Some(&p) {
                    if tx.send(p.clone()).is_err() {
                        return;
                    }
                    wake();
                    last = Some(p);
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    // fbterm 1366x768、セル 8x16 (170 列 x 48 行)、ステータスバーは下に 1 行
    const FB: (u32, u32) = (1366, 768);

    #[test]
    fn computes_pane_rect() {
        // 右半分のペイン (左端 85 列目、幅 85 列、高さ 47 行)
        let p = parse_pane("1,85,0,85,47,170,47,170,48,bottom,1,0,1", FB.0, FB.1, None, false).unwrap();
        assert_eq!(p.rect, Rect { x: 680, y: 0, w: 680, h: 752 });
        assert!(p.shown);
        // ステータスバーが上なら 1 行下へずれる
        let p = parse_pane("1,0,0,170,47,170,47,170,48,top,1,0,1", FB.0, FB.1, None, false).unwrap();
        assert_eq!(p.rect.y, 16);
    }

    #[test]
    fn uses_drmterm_cells_and_client() {
        // drmterm 1280x720、セル 8x16 (160 列 x 45 行。最下行は uim-fep が使うので tmux は 44 行)、
        // ステータスバーは上に 1 行
        let line = |client_session: &str| format!("1,80,0,80,43,160,43,160,44,top,1,0,1,{client_session},ext");
        let p = parse_pane(&line("ext"), 1280, 720, Some((8, 16)), true).unwrap();
        assert_eq!(p.rect, Rect { x: 640, y: 16, w: 640, h: 688 });
        assert!(p.shown);
        // drmterm のクライアントが別のセッションを映している
        assert!(!parse_pane(&line("0"), 1280, 720, Some((8, 16)), true).unwrap().shown);
    }

    #[test]
    fn reads_parent_from_stat() {
        assert_eq!(parent_of("8059 (tmux: client) S 8020 8059 8020 34817"), Some(8020));
        // 名前に括弧が入っていても
        assert_eq!(parent_of("42 (a) b (c)) R 7 42"), Some(7));
        assert_eq!(parent_of("garbage"), None);
    }

    #[test]
    fn console_is_a_vt_or_under_fbterm() {
        assert!(on_console(u32::MAX, "/dev/tty1"));
        assert!(!on_console(u32::MAX, "/dev/tty"));
        assert!(!on_console(u32::MAX, "/dev/ttyS0"));
        // このテスト自身は fbterm の子孫ではない (cargo test は tmux のペインや ssh から動かす)
        assert!(!on_console(std::process::id(), "/dev/pts/4"));
    }

    #[test]
    fn hidden_when_not_on_screen() {
        // 別のウィンドウを表示中
        let hidden = |s: &str| !parse_pane(s, FB.0, FB.1, None, false).unwrap().shown;
        assert!(hidden("0,0,0,170,47,170,47,170,48,bottom,1,0,1"));
        // どのクライアントにも表示されていないセッション
        assert!(hidden("1,0,0,170,47,170,47,170,48,bottom,0,0,1"));
        // 同じウィンドウの別のペインがズームされている
        assert!(hidden("1,0,0,85,47,170,47,170,48,bottom,1,1,0"));
        assert!(!hidden("1,0,0,170,47,170,47,170,48,bottom,1,1,1"));
    }
}
