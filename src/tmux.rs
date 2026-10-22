//! tmux への問い合わせ (fbrowse の tmux.rs から必要な分だけ写したもの)。

use std::process::{Command, Stdio};

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
