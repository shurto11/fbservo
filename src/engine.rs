//! Servo の起動と WebView の操作。
//!
//! ウィンドウも GPU も使わず、SoftwareRenderingContext (Mesa のソフトウェア GL。
//! DISPLAY が無ければ surfman が EGL surfaceless を選ぶ) に描かせて、
//! read_to_image で RGBA を受け取る。Servo の型は Rc なので、すべて 1 スレッドで扱う。

use crate::fb::{Image, Order};
use crate::hints::{self, Hint};
use crate::term::Key as TermKey;
use anyhow::{anyhow, Result};
use dpi::PhysicalSize;
use euclid::{Point2D, Scale};
use servo::{
    Code, ConsoleLogLevel, CreateNewWebViewRequest, DeviceIntRect, DeviceIntSize, DevicePoint, DeviceVector2D,
    EventLoopWaker, InputEvent, JSValue, Key, KeyState, KeyboardEvent, LoadStatus, Location, Modifiers, MouseButton,
    MouseButtonAction, MouseButtonEvent, MouseMoveEvent, NamedKey, RenderingContext, Scroll, Servo,
    ServoBuilder, SoftwareRenderingContext, WebView, WebViewBuilder, WebViewDelegate, WebViewPoint, WebViewVector,
};
use std::cell::{Cell, RefCell};
use std::os::fd::RawFd;
use std::rc::Rc;
use url::Url;

/// Servo が別スレッドから「イベントループを回して」と頼んできたら、pipe に 1 バイト書く。
/// メインループはこの pipe を標準入力と一緒に poll する。
#[derive(Clone)]
struct PipeWaker(RawFd);

impl EventLoopWaker for PipeWaker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        // pipe が詰まっている (EAGAIN) ならもう起こしてあるので無視してよい
        unsafe { libc::write(self.0, b"x".as_ptr().cast(), 1) };
    }
}

/// (読み出し側 fd, Servo に渡す waker)
pub fn pipe_waker() -> Result<(RawFd, Box<dyn EventLoopWaker>)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
        return Err(anyhow!("pipe2: {}", std::io::Error::last_os_error()));
    }
    Ok((fds[0], Box::new(PipeWaker(fds[1]))))
}

/// WebView からの通知を受け取る。ログはそのまま stderr (= ログファイル) へ。
#[derive(Default)]
pub struct Delegate {
    frame_ready: Cell<bool>,
    /// window.open / target=_blank で作られた新しい WebView の作成要求
    new_view: RefCell<Option<CreateNewWebViewRequest>>,
    /// 読み込みを終えたタブ。spin で今のタブか確かめ、入力欄にフォーカスがあるか調べる
    loaded: RefCell<Vec<WebView>>,
}

impl WebViewDelegate for Delegate {
    fn notify_new_frame_ready(&self, _: WebView) {
        self.frame_ready.set(true);
    }

    fn notify_url_changed(&self, _: WebView, url: Url) {
        eprintln!("[url] {url}");
    }

    fn notify_page_title_changed(&self, _: WebView, title: Option<String>) {
        eprintln!("[title] {}", title.unwrap_or_default());
    }

    fn notify_load_status_changed(&self, view: WebView, status: LoadStatus) {
        eprintln!("[load] {status:?}");
        if status == LoadStatus::Complete {
            self.loaded.borrow_mut().push(view);
        }
    }

    fn notify_crashed(&self, _: WebView, reason: String, _backtrace: Option<String>) {
        eprintln!("[crash] {reason}");
    }

    fn show_console_message(&self, _: WebView, level: ConsoleLogLevel, message: String) {
        eprintln!("[console {level:?}] {message}");
    }

    fn request_create_new(&self, _parent: WebView, request: CreateNewWebViewRequest) {
        *self.new_view.borrow_mut() = Some(request);
    }
}

pub struct Engine {
    /// 開いているタブ。Servo より先に drop させるため先頭に置く。
    views: Vec<WebView>,
    /// 表示しているタブの添字
    current: usize,
    delegate: Rc<Delegate>,
    servo: Servo,
    ctx: Rc<SoftwareRenderingContext>,
    /// GL から BGRA で読み出せるか。読めなければ RGBA (Servo の read_to_image) に切り替える
    bgra: Cell<bool>,
    /// 今のタブが読み込みを終えたとき、入力欄にフォーカスが当たっていた (FOCUS_JS の結果)
    input_focused: Rc<Cell<bool>>,
    pub width: u32,
    pub height: u32,
    scale: f32,
}

impl Engine {
    pub fn new(width: u32, height: u32, scale: f32, url: Url, waker: Box<dyn EventLoopWaker>) -> Result<Engine> {
        let ctx = Rc::new(
            SoftwareRenderingContext::new(PhysicalSize::new(width, height))
                .map_err(|e| anyhow!("ソフトウェア GL を初期化できません: {e:?}"))?,
        );
        ctx.make_current().map_err(|e| anyhow!("make_current: {e:?}"))?;
        let servo = ServoBuilder::default().event_loop_waker(waker).build();
        servo.setup_logging();
        let delegate = Rc::new(Delegate::default());
        let view = WebViewBuilder::new(&servo, ctx.clone())
            .url(url)
            .hidpi_scale_factor(Scale::new(scale))
            .delegate(delegate.clone())
            .build();
        view.set_focused(true);
        Ok(Engine {
            views: vec![view],
            current: 0,
            delegate,
            servo,
            ctx,
            bgra: Cell::new(true),
            input_focused: Rc::default(),
            width,
            height,
            scale,
        })
    }

    pub fn view(&self) -> &WebView {
        &self.views[self.current]
    }

    /// (表示中のタブの番号 (1 から), タブの数)
    pub fn tabs(&self) -> (usize, usize) {
        (self.current + 1, self.views.len())
    }

    /// Servo のイベントループを 1 回回す。ページが新しいウィンドウを開こうとしていたら
    /// (window.open, target=_blank)、今のタブの右に新しいタブとして開く。
    /// 今のタブが読み込みを終えていたら、入力欄にフォーカスがあるか調べる
    /// (結果は後の spin で届き、take_input_focused で受け取る)。
    pub fn spin(&mut self) {
        self.servo.spin_event_loop();
        let req = self.delegate.new_view.borrow_mut().take();
        if let Some(req) = req {
            let view = req
                .builder(self.ctx.clone())
                .hidpi_scale_factor(Scale::new(self.scale))
                .delegate(self.delegate.clone())
                .build();
            self.add_tab(view);
        }
        let loaded = std::mem::take(&mut *self.delegate.loaded.borrow_mut());
        if loaded.contains(self.view()) {
            let focused = self.input_focused.clone();
            self.view().evaluate_javascript(hints::FOCUS_JS, move |r| match r {
                Ok(JSValue::Boolean(true)) => focused.set(true),
                Ok(_) => {}
                Err(e) => eprintln!("[focus] JS が失敗: {e:?}"),
            });
        }
    }

    /// 読み込みを終えた今のタブで、入力欄にフォーカスが当たっていたら true を返す (1 回だけ)。
    pub fn take_input_focused(&self) -> bool {
        self.input_focused.replace(false)
    }

    /// URL を新しいタブで開き、そのタブを表示する。
    pub fn open_tab(&mut self, url: Url) {
        let view = WebViewBuilder::new(&self.servo, self.ctx.clone())
            .url(url)
            .hidpi_scale_factor(Scale::new(self.scale))
            .delegate(self.delegate.clone())
            .build();
        self.add_tab(view);
    }

    fn add_tab(&mut self, view: WebView) {
        self.views.insert(self.current + 1, view);
        self.show_tab(self.current + 1);
    }

    /// i 番目のタブを表示する。
    fn show_tab(&mut self, i: usize) {
        if i != self.current {
            self.view().hide();
        }
        self.current = i;
        self.view().show();
        self.view().set_focused(true);
        self.delegate.frame_ready.set(true);
        let (n, total) = self.tabs();
        eprintln!("[tab] {n}/{total}");
    }

    /// step 個先 (負なら前) のタブへ。端から端へは回り込む。
    pub fn switch_tab(&mut self, step: isize) {
        let n = self.views.len() as isize;
        if n > 1 {
            self.show_tab((self.current as isize + step).rem_euclid(n) as usize);
        }
    }

    /// 今のタブを閉じて右隣 (右端なら左隣) を表示する。最後の 1 枚は閉じない。
    pub fn close_tab(&mut self) -> bool {
        if self.views.len() < 2 {
            return false;
        }
        // 閉じたタブは drop で消える。残ったタブの添字に付け替えて表示する
        self.views.remove(self.current);
        self.current = self.current.min(self.views.len() - 1);
        self.show_tab(self.current);
        true
    }

    /// 描画領域の大きさを変える (tmux のペインの大きさが変わったとき)。タブはすべて同じ
    /// 描画コンテキストを使うので、1 回で全タブが新しい大きさになる。
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.view().resize(PhysicalSize::new(width, height));
        self.delegate.frame_ready.set(true);
    }

    /// 変化が無くても次のループで描き直させる。
    pub fn request_frame(&self) {
        self.delegate.frame_ready.set(true);
    }

    /// 新しいフレームが届いていれば true を返し、フラグを下ろす。
    pub fn take_frame(&self) -> bool {
        self.delegate.frame_ready.replace(false)
    }

    pub fn animating(&self) -> bool {
        self.view().animating()
    }

    /// WebRender に描かせる (ソフトウェア GL なのでここが CPU を食う)。
    pub fn paint(&self) {
        self.view().paint();
    }

    /// 描いた結果を読み出す。buf は前のフレームの入れ物を使い回す (毎回数 MB 確保しないため)。
    /// GL から fb0 と同じ並びの BGRA で直接読む。Servo の read_to_image は RGBA で読んで
    /// 全体を複製し上下を反転するので、そのあとの並べ替えも含めて倍以上かかる。
    pub fn read(&self, mut buf: Vec<u8>) -> Option<Image> {
        let (w, h) = (self.width, self.height);
        if self.bgra.get() {
            // 描画先のフレームバッファを結び付ける (Servo の read_to_image と同じ)
            self.ctx.prepare_for_rendering();
            let gl = self.ctx.gleam_gl_api();
            // WebRender が残したエラーを読み捨ててから読む (文脈が失われると出続けるので回数を限る)
            for _ in 0..8 {
                if gl.get_error() == gleam::gl::NO_ERROR {
                    break;
                }
            }
            buf.resize(w as usize * h as usize * 4, 0);
            gl.read_pixels_into_buffer(0, 0, w as i32, h as i32, gleam::gl::BGRA, gleam::gl::UNSIGNED_BYTE, &mut buf);
            if gl.get_error() == gleam::gl::NO_ERROR {
                self.ctx.present();
                return Some(Image { data: buf, w, h, order: Order::BgraBottomUp });
            }
            eprintln!("[read] GL から BGRA で読めないので RGBA で読みます");
            self.bgra.set(false);
        }
        let rect = DeviceIntRect::from_size(DeviceIntSize::new(w as i32, h as i32));
        let img = self.ctx.read_to_image(rect);
        self.ctx.present();
        img.map(|img| Image { w: img.width(), h: img.height(), data: img.into_raw(), order: Order::RgbaTopDown })
    }

    fn center(&self) -> WebViewPoint {
        WebViewPoint::Device(DevicePoint::new(self.width as f32 / 2.0, self.height as f32 / 2.0))
    }

    /// 画面中央にあるスクロール領域を (dx, dy) デバイスピクセル動かす。正の dy で下へ進む。
    pub fn scroll_by(&self, dx: f32, dy: f32) {
        let delta = WebViewVector::Device(DeviceVector2D::new(dx, dy));
        self.view().notify_scroll_event(Scroll::Delta(delta), self.center());
    }

    pub fn scroll_to(&self, end: bool) {
        self.view().notify_scroll_event(if end { Scroll::End } else { Scroll::Start }, self.center());
    }

    pub fn load(&self, url: Url) {
        self.view().load(url);
    }

    pub fn reload(&self) {
        self.view().reload();
    }

    pub fn back(&self) {
        if self.view().can_go_back() {
            self.view().go_back(1);
        }
    }

    pub fn forward(&self) {
        if self.view().can_go_forward() {
            self.view().go_forward(1);
        }
    }

    /// 倍率に d を足す (fbrowse と同じく 10% 刻み)。None なら等倍に戻す。新しい倍率を返す。
    pub fn zoom(&self, d: Option<f32>) -> f32 {
        let z = match d {
            Some(d) => ((self.view().page_zoom() + d) * 10.0).round() / 10.0,
            None => 1.0,
        };
        self.view().set_page_zoom(z.max(0.1));
        self.view().page_zoom()
    }

    /// ページで JS を実行する。結果は捨て、失敗したらログに残す。
    pub fn run_js(&self, js: &str) {
        self.view().evaluate_javascript(js, |r| {
            if let Err(e) = r {
                eprintln!("[js] 失敗: {e:?}");
            }
        });
    }

    /// クリックできる要素にヒントのラベルを出す。結果は届いたら slot に入る
    /// (evaluate_javascript は非同期で、spin の中でコールバックが呼ばれる)。
    pub fn show_hints(&self, slot: Rc<RefCell<Option<Vec<Hint>>>>) {
        self.view().evaluate_javascript(hints::SHOW_JS, move |r| {
            let found = match r {
                Ok(v) => hints::parse(v),
                Err(e) => {
                    eprintln!("[hint] JS が失敗: {e:?}");
                    Vec::new()
                }
            };
            *slot.borrow_mut() = Some(found);
        });
    }

    /// ページ上の (x, y) (CSS px、ビューポート基準) を左クリックする。
    /// ページズームと HiDPI の換算は Servo がする。
    pub fn click(&self, x: f32, y: f32) {
        let p = WebViewPoint::Page(Point2D::new(x, y));
        self.view().notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(p)));
        for action in [MouseButtonAction::Down, MouseButtonAction::Up] {
            let ev = MouseButtonEvent::new(action, MouseButton::Primary, p);
            self.view().notify_input_event(InputEvent::MouseButton(ev));
        }
    }

    /// 入力モードのキーをページへ送る (押して離す)。
    pub fn send_key(&self, k: &TermKey) {
        let (key, mods) = match k {
            TermKey::Char(c) => (Key::Character(c.to_string()), Modifiers::empty()),
            TermKey::Ctrl(c) => (Key::Character(c.to_string()), Modifiers::CONTROL),
            TermKey::Enter => (Key::Named(NamedKey::Enter), Modifiers::empty()),
            TermKey::Tab => (Key::Named(NamedKey::Tab), Modifiers::empty()),
            TermKey::Backspace => (Key::Named(NamedKey::Backspace), Modifiers::empty()),
            TermKey::Esc => (Key::Named(NamedKey::Escape), Modifiers::empty()),
            TermKey::Up => (Key::Named(NamedKey::ArrowUp), Modifiers::empty()),
            TermKey::Down => (Key::Named(NamedKey::ArrowDown), Modifiers::empty()),
            TermKey::Left => (Key::Named(NamedKey::ArrowLeft), Modifiers::empty()),
            TermKey::Right => (Key::Named(NamedKey::ArrowRight), Modifiers::empty()),
            TermKey::PageUp => (Key::Named(NamedKey::PageUp), Modifiers::empty()),
            TermKey::PageDown => (Key::Named(NamedKey::PageDown), Modifiers::empty()),
            TermKey::Home => (Key::Named(NamedKey::Home), Modifiers::empty()),
            TermKey::End => (Key::Named(NamedKey::End), Modifiers::empty()),
            TermKey::Delete => (Key::Named(NamedKey::Delete), Modifiers::empty()),
        };
        for state in [KeyState::Down, KeyState::Up] {
            let ev = KeyboardEvent::new_without_event(
                state,
                key.clone(),
                Code::Unidentified,
                Location::Standard,
                mods,
                false,
                false,
            );
            self.view().notify_input_event(InputEvent::Keyboard(ev));
        }
    }
}
