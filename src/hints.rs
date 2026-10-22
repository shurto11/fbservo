//! リンクヒント (f)。fbrowse と同じく、クリックできる要素に 2 文字のラベルをページの
//! DOM として差し込み、ラベルを打つとその要素の中心をクリックする。
//!
//! ラベルはページの中に描かれるので、自前で絵に文字を重ねる仕組みは要らない。

use servo::JSValue;
use std::cell::RefCell;
use std::rc::Rc;

/// ラベルに使う文字。ヒントモードで f を取消に使うため f は入れない (fbrowse と同じ)。
pub const LABEL_CHARS: &str = "asdjklghqwertuiopzxcvbnm";

/// クリックできる要素に 2 文字のラベルを出し、その中心の座標 (CSS px) を返す。
/// fbrowse の HINTS_JS から、CSS zoom の補正を外したもの (fbservo は Servo のページズームを
/// 使うので、getBoundingClientRect の値をそのままクリック位置にできる)。
/// ラベルは pointer-events: none にして、小さい要素の中心を覆ってもクリックが下へ届くようにする。
pub const SHOW_JS: &str = r#"(() => {
  document.querySelectorAll('.__hint__').forEach(el => el.remove());
  const chars = 'asdjklghqwertuiopzxcvbnm'.split('');
  const keys = [];
  for (const a of chars) for (const b of chars) keys.push(a + b);
  const clickable = document.querySelectorAll('a, button, input, select, textarea, [onclick], [role="button"], [role="link"]');
  const hints = [];
  let i = 0;
  clickable.forEach(el => {
    if (i >= keys.length) return;
    const r = el.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) return;
    if (r.top < 0 || r.top > window.innerHeight) return;
    if (r.left < 0 || r.left > window.innerWidth) return;
    const key = keys[i++];
    const hint = document.createElement('div');
    hint.className = '__hint__';
    hint.dataset.key = key;
    hint.textContent = key.toUpperCase();
    hint.style.cssText = `position: fixed; left: ${r.left}px; top: ${r.top}px;
      background: #ffcc00; color: #000; font-size: 12px; font-weight: bold; padding: 2px 5px;
      border-radius: 3px; z-index: 2147483647; font-family: monospace; pointer-events: none;
      box-shadow: 0 1px 3px rgba(0,0,0,0.5);`;
    document.body.appendChild(hint);
    const a = el.closest('a');
    hints.push({ key, x: r.left + r.width / 2, y: r.top + r.height / 2, href: a ? a.href : null });
  });
  return hints;
})()"#;

pub const CLEAR_JS: &str = "document.querySelectorAll('.__hint__').forEach(el => el.remove())";

/// 打ち始めたラベルに合わないものを隠す JS。prefix は LABEL_CHARS の文字だけであること。
pub fn narrow_js(prefix: &str) -> String {
    debug_assert!(prefix.chars().all(|c| LABEL_CHARS.contains(c)));
    format!(
        "document.querySelectorAll('.__hint__').forEach(el => \
         el.style.display = el.dataset.key.startsWith('{prefix}') ? '' : 'none')"
    )
}

pub struct Hint {
    pub label: String,
    /// 要素の中心 (CSS px、ビューポート基準)
    pub x: f32,
    pub y: f32,
    /// リンクなら行き先
    pub href: Option<String>,
}

/// SHOW_JS の結果を読む。形の合わない要素は飛ばす。
pub fn parse(v: JSValue) -> Vec<Hint> {
    let JSValue::Array(items) = v else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|item| {
            let JSValue::Object(mut m) = item else {
                return None;
            };
            let num = |v: Option<JSValue>| match v {
                Some(JSValue::Number(n)) => Some(n as f32),
                _ => None,
            };
            let JSValue::String(label) = m.remove("key")? else {
                return None;
            };
            let x = num(m.remove("x"))?;
            let y = num(m.remove("y"))?;
            let href = match m.remove("href") {
                Some(JSValue::String(s)) => Some(s),
                _ => None,
            };
            Some(Hint { label, x, y, href })
        })
        .collect()
}

/// ヒントモードの状態。
pub struct HintMode {
    /// これまでに打ったラベルの文字
    pub typed: String,
    /// JS の結果の受け口。evaluate_javascript のコールバックが書く
    result: Rc<RefCell<Option<Vec<Hint>>>>,
    /// 届いたヒント。届くまでは None
    hints: Option<Vec<Hint>>,
}

impl HintMode {
    /// (状態, JS の結果を書き込む受け口)
    pub fn new() -> (HintMode, Rc<RefCell<Option<Vec<Hint>>>>) {
        let result = Rc::new(RefCell::new(None));
        (HintMode { typed: String::new(), result: result.clone(), hints: None }, result)
    }

    /// 届いたヒント。まだなら None。
    pub fn hints(&mut self) -> Option<&[Hint]> {
        if self.hints.is_none() {
            self.hints = self.result.borrow_mut().take();
        }
        self.hints.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn parses_hints() {
        let obj = |key: &str, href: JSValue| {
            JSValue::Object(HashMap::from([
                ("key".to_string(), JSValue::String(key.to_string())),
                ("x".to_string(), JSValue::Number(10.5)),
                ("y".to_string(), JSValue::Number(20.0)),
                ("href".to_string(), href),
            ]))
        };
        let v = JSValue::Array(vec![
            obj("aa", JSValue::String("https://example.com/".into())),
            obj("as", JSValue::Null),
            JSValue::Number(1.0),
        ]);
        let hints = parse(v);
        assert_eq!(hints.len(), 2);
        assert_eq!((hints[0].label.as_str(), hints[0].x, hints[0].y), ("aa", 10.5, 20.0));
        assert_eq!(hints[0].href.as_deref(), Some("https://example.com/"));
        assert_eq!(hints[1].href, None);
        assert!(parse(JSValue::Null).is_empty());
    }

    #[test]
    fn label_chars_match_js() {
        assert!(SHOW_JS.contains(&format!("'{LABEL_CHARS}'")));
        assert!(!LABEL_CHARS.contains('f'));
    }
}
