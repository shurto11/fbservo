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

/// 自分のペイン ($TMUX_PANE) の位置をピクセルで返す。tmux の外や、問い合わせに
/// 失敗したときは None。セルの大きさは画面の大きさを端末の行数・列数で割って求める。
pub fn pane(fb_w: u32, fb_h: u32) -> Option<Pane> {
    if !inside() {
        return None;
    }
    let fmt = "#{window_active},#{pane_left},#{pane_top},#{pane_width},#{pane_height},\
               #{window_width},#{window_height},#{client_width},#{client_height},#{status-position},\
               #{session_attached},#{window_zoomed_flag},#{pane_active}";
    let mut args = vec!["display-message", "-p"];
    let own = std::env::var("TMUX_PANE").ok().filter(|s| !s.is_empty());
    if let Some(p) = own.as_deref() {
        args.extend(["-t", p]);
    }
    args.push(fmt);
    parse_pane(&run(&args)?, fb_w, fb_h)
}

fn parse_pane(out: &str, fb_w: u32, fb_h: u32) -> Option<Pane> {
    let parts: Vec<&str> = out.split(',').collect();
    let n = |i: usize| parts.get(i).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let (active, pl, pt, pw, ph, ww, wh, cw_cells, ch_cells) = (n(0), n(1), n(2), n(3), n(4), n(5), n(6), n(7), n(8));
    let status_pos = parts.get(9).copied().filter(|s| !s.is_empty()).unwrap_or("bottom");
    let attached = parts.get(10).and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
    let (zoomed, pane_active) = (n(11), n(12));
    if ww == 0 || wh == 0 {
        return None;
    }
    // セルの大きさは、ステータスバーを含む端末全体の行列数から求める
    // (window_height はステータスバーの分だけ小さいので、それで割るとセルが大きすぎる)。
    let cols = if cw_cells > 0 { cw_cells } else { ww };
    let rows = if ch_cells > 0 { ch_cells } else { wh };
    // 画面の下を別のもの (task-var のバーなど) が占めていると端末の行数だけが減り、
    // 画面の高さ / 行数ではセルが大きすぎる。セルの高さは幅の 2 倍までに抑える。
    let cw = fb_w / cols;
    let ch = (fb_h / rows).min(cw * 2);
    if cw == 0 || ch == 0 {
        return None;
    }
    // pane_top はウィンドウの中での位置なので、ステータスバーが上ならその分下へずらす
    let y_off = if status_pos == "top" { rows.saturating_sub(wh) * ch } else { 0 };
    Some(Pane {
        rect: Rect { x: pl * cw, y: y_off + pt * ch, w: pw * cw, h: ph * ch },
        shown: active == 1 && attached > 0 && (zoomed == 0 || pane_active == 1),
    })
}

/// 別スレッドで 500ms ごとに自分のペインを調べ、変わったら送って wake を呼ぶ
/// (tmux の起動に数 ms かかるので、メインループでは待たない)。
pub fn follow(fb_w: u32, fb_h: u32, wake: impl Fn() + Send + 'static) -> Receiver<Pane> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut last = None;
        loop {
            if let Some(p) = pane(fb_w, fb_h) {
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
        let p = parse_pane("1,85,0,85,47,170,47,170,48,bottom,1,0,1", FB.0, FB.1).unwrap();
        assert_eq!(p.rect, Rect { x: 680, y: 0, w: 680, h: 752 });
        assert!(p.shown);
        // ステータスバーが上なら 1 行下へずれる
        let p = parse_pane("1,0,0,170,47,170,47,170,48,top,1,0,1", FB.0, FB.1).unwrap();
        assert_eq!(p.rect.y, 16);
    }

    #[test]
    fn hidden_when_not_on_screen() {
        // 別のウィンドウを表示中
        assert!(!parse_pane("0,0,0,170,47,170,47,170,48,bottom,1,0,1", FB.0, FB.1).unwrap().shown);
        // どのクライアントにも表示されていないセッション
        assert!(!parse_pane("1,0,0,170,47,170,47,170,48,bottom,0,0,1", FB.0, FB.1).unwrap().shown);
        // 同じウィンドウの別のペインがズームされている
        assert!(!parse_pane("1,0,0,85,47,170,47,170,48,bottom,1,1,0", FB.0, FB.1).unwrap().shown);
        assert!(parse_pane("1,0,0,170,47,170,47,170,48,bottom,1,1,1", FB.0, FB.1).unwrap().shown);
    }
}
