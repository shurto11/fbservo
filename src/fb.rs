//! フレームバッファ (/dev/fb0) への書き込み。
//!
//! fb0 を mmap し、Servo から読み出した絵を行ごとにコピーする。fb0 は XRGB8888
//! (メモリ上は B,G,R,X) なので、GL から BGRA で読んだ絵なら並べ替えずにそのまま書ける。
//! サイズと行幅は sysfs から読む。描く場所は画面全体か、tmux ペインの矩形 (`Rect`)。
//! 書く時刻は、画面の走査と重ならないように選ぶ (`present`、src/scan.rs)。

use crate::scan::{decide, line_at, refresh_after, time_at, vblank_of, Clock, Decision, Span, Timing, FBIO_WAITFORVSYNC};
use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

/// 画面上の矩形 (ピクセル)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// 画面に描く絵 (1 画素 4 バイト)。
pub struct Image {
    pub data: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub order: Order,
}

pub enum Order {
    /// GL から BGRA で読んだまま。下の行が先頭
    BgraBottomUp,
    /// RGBA。上の行が先頭 (BGRA で読めない GL のとき)
    RgbaTopDown,
}

pub struct Fb {
    /// mmap の元。開いている間だけ mmap が有効
    file: File,
    map: *mut u8,
    len: usize,
    pub width: u32,
    pub height: u32,
    /// 1 行のバイト数 (表示幅 × 4 より大きいことがある)
    stride: usize,
    /// 垂直同期を待つか。FBSERVO_VSYNC=0 なら待たない (比べる用)。待てない書き込み先
    /// (FBSERVO_FB でファイルに描くときなど) なら、最初に失敗した時点でやめる
    /// (ほかのプロセスが DRM を握っていて待てない (EBUSY) ときは、その間だけ待たずに書く)
    vsync: bool,
    /// 走査のタイミング (EDID から) と、垂直帰線の時刻を測るスレッド。
    /// どちらも使えるときだけ、走査の位置を見積もって書く
    scan: Option<(Timing, Clock)>,
    /// 垂直帰線の間隔
    period: Duration,
    /// (書いた幅, 1 行書くのにかかった最短の時間, 書いた回数)。書いている間に走査がどれだけ
    /// 進むかの見積もりに使う。速く書けると見込むほど早く書き始めるので、最初の何回か
    /// (mmap した所へ初めて書くときなどは遅い) は使わない
    row_time: Option<(usize, Duration, u32)>,
}

/// `present` で絵を出した記録。
pub struct Shown {
    /// この絵が出る走査の前の垂直帰線の時刻 (走査の位置が分からなければ、書き始めた時刻)
    pub vblank: Instant,
    /// 書き始めた時刻
    pub start: Instant,
    /// 書くのにかかった時間
    pub took: Duration,
}

fn read_sysfs(name: &str) -> Result<String> {
    let p = format!("/sys/class/graphics/fb0/{name}");
    Ok(std::fs::read_to_string(&p).with_context(|| format!("{p} を読めません"))?.trim().to_string())
}

impl Fb {
    /// FBSERVO_FB で書き込み先を差し替えられる (動作確認用。サイズは fb0 と同じ前提)。
    pub fn open() -> Result<Fb> {
        let bpp: u32 = read_sysfs("bits_per_pixel")?.parse()?;
        if bpp != 32 {
            bail!("32bpp 以外のフレームバッファには対応していません ({bpp}bpp)");
        }
        let vs = read_sysfs("virtual_size")?;
        let (w, h) = vs.split_once(',').context("virtual_size の形式が不正です")?;
        let (width, height): (u32, u32) = (w.parse()?, h.parse()?);
        let stride: usize = read_sysfs("stride")?.parse()?;
        let path = std::env::var("FBSERVO_FB").unwrap_or_else(|_| "/dev/fb0".to_string());
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("{path} を開けません (video グループに入っていますか)"))?;
        let len = stride * height as usize;
        let map = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, file.as_raw_fd(), 0)
        };
        if map == libc::MAP_FAILED {
            bail!("{path} を mmap できません: {}", std::io::Error::last_os_error());
        }
        let vsync = std::env::var("FBSERVO_VSYNC").map_or(true, |v| v != "0");
        let timing = if vsync { Timing::from_edid(width, height) } else { None };
        let scan = timing.and_then(|t| Some((t, Clock::start(file.try_clone().ok()?, t.period))));
        let mut fb = Fb { file, map: map.cast(), len, width, height, stride, vsync, scan, period: Duration::ZERO, row_time: None };
        fb.period = match timing {
            Some(t) => t.period,
            None => fb.measure_period(),
        };
        Ok(fb)
    }

    /// 書く時刻の決め方 (ログ用)
    pub fn describe(&self) -> String {
        match &self.scan {
            Some((t, clock)) => format!(
                "走査 {} 行 (表示 {} 行)、{:.3}ms (EDID)。{}",
                t.vtotal,
                t.vactive,
                t.period.as_secs_f64() * 1000.0,
                if clock.vblank(Instant::now()).is_some() {
                    "走査の位置を見積もって書く"
                } else {
                    "帰線の時刻を測れていないので、測れるまでは割り込みを待ってから書く"
                }
            ),
            None if self.vsync => "走査のタイミングが分からないので、垂直帰線の割り込みを待ってから書く".to_string(),
            None => "垂直帰線を待たない".to_string(),
        }
    }

    /// 垂直帰線の間隔
    pub fn period(&self) -> Duration {
        self.period
    }

    fn mem(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.map, self.len) }
    }

    /// 画面全体
    pub fn full(&self) -> Rect {
        Rect { x: 0, y: 0, w: self.width, h: self.height }
    }

    /// 幅 w・高さ h の絵を area に描くとき、画面と area からはみ出さない (列数, 行数)。
    /// ペインが縮んだ直後は前の大きさの絵が届くことがあるので、area でも切る。
    fn fit(&self, w: u32, h: u32, area: Rect) -> Option<(usize, usize)> {
        if area.x >= self.width || area.y >= self.height {
            return None;
        }
        let cw = w.min(area.w).min(self.width - area.x) as usize;
        let ch = h.min(area.h).min(self.height - area.y) as usize;
        Some((cw, ch))
    }

    /// 次の垂直帰線まで待つ (描いている途中で画面が更新されて絵が上下でずれるのを防ぐ)。
    /// 帰線の直後から上の行へ書けば、走査より先に書き終わる。待てない書き込み先なら何もしない。
    pub fn wait_vsync(&mut self) {
        if !self.vsync {
            return;
        }
        let arg: u32 = 0;
        if unsafe { libc::ioctl(self.file.as_raw_fd(), FBIO_WAITFORVSYNC, &arg) } != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(libc::EBUSY)
        {
            self.vsync = false;
        }
    }

    /// target の時刻 (の半コマ前) を過ぎるまで垂直帰線を待つ。少なくとも 1 回は待つ。
    /// 待てない書き込み先なら target まで眠る。
    pub fn wait_vsync_until(&mut self, target: Instant, period: Duration) {
        loop {
            if !self.vsync {
                std::thread::sleep(target.saturating_duration_since(Instant::now()));
                return;
            }
            self.wait_vsync();
            if Instant::now() + period / 2 >= target {
                return;
            }
        }
    }

    /// 垂直帰線の間隔。何回か待って測る (待てない書き込み先なら 60Hz とみなす)。
    fn measure_period(&mut self) -> Duration {
        const DEFAULT: Duration = Duration::from_micros(16_667);
        self.wait_vsync();
        let mut last = Instant::now();
        let mut d = Vec::new();
        for _ in 0..4 {
            self.wait_vsync();
            let now = Instant::now();
            d.push(now - last);
            last = now;
        }
        d.sort();
        let p = d[d.len() / 2];
        if self.vsync && (Duration::from_millis(5)..Duration::from_millis(50)).contains(&p) {
            p
        } else {
            DEFAULT
        }
    }

    /// 走査の位置が分かるなら、(走査のタイミング, 直近の帰線の時刻, 書く範囲)。
    fn scan_span(&self, img: &Image, area: Rect, now: Instant) -> Option<(Timing, Instant, Span)> {
        let (t, clock) = self.scan.as_ref()?;
        let anchor = clock.vblank(now)?;
        let (cw, ch) = self.fit(img.w, img.h, area).filter(|&(_, ch)| ch > 0)?;
        let speed = match self.row_time {
            Some((w, d, n)) if w == cw && n >= 10 => d.as_secs_f64() * t.vtotal as f64 / t.period.as_secs_f64(),
            _ => 0.0,
        };
        Some((*t, anchor, Span { y0: area.y as f64, y1: (area.y as usize + ch) as f64, speed }))
    }

    /// `present` が書き始める予定の時刻 (待たずに書けるなら None)。それまでに別のことをして
    /// おきたいときに使う (main.rs は Servo のイベントループを回して、次のコマができた時刻を測る)。
    pub fn write_at(&self, img: &Image, area: Rect, target: Option<Instant>) -> Option<Instant> {
        let now = Instant::now();
        match self.scan_span(img, area, now) {
            Some((t, anchor, span)) => {
                let want = target.map_or(i64::MIN, |v| refresh_after(t, anchor, v));
                match decide(line_at(t, anchor, now), want, span, t) {
                    Decision::Wait(line) => Some(time_at(t, anchor, line)),
                    Decision::Write(_) => None,
                }
            }
            // 割り込みを待つときは、target の半コマ前を過ぎてから最後の 1 回を待つ (wait_vsync_until)。
            // 待てない書き込み先なら target まで
            None if self.vsync => target.and_then(|v| v.checked_sub(self.period / 2)),
            None => target,
        }
    }

    /// 絵を area に描く。target が Some なら、その時刻の垂直帰線のあとの走査より前には出さない。
    /// 走査している途中の範囲に書くと上と下で別のコマが混ざる (ティアリング) ので、走査の位置が
    /// 分かるときは、走査が範囲の外にある間に書く (src/scan.rs)。分からなければ垂直帰線を待ってから書く。
    pub fn present(&mut self, img: &Image, area: Rect, target: Option<Instant>) -> Shown {
        let vblank = match self.scan_span(img, area, Instant::now()) {
            Some((t, anchor, span)) => {
                let want = target.map_or(i64::MIN, |v| refresh_after(t, anchor, v));
                let refresh = loop {
                    match decide(line_at(t, anchor, Instant::now()), want, span, t) {
                        Decision::Wait(line) => {
                            std::thread::sleep(time_at(t, anchor, line).saturating_duration_since(Instant::now()))
                        }
                        Decision::Write(k) => break k,
                    }
                };
                Some(vblank_of(t, anchor, refresh))
            }
            None => {
                match target {
                    Some(v) => self.wait_vsync_until(v, self.period),
                    None => self.wait_vsync(),
                }
                None
            }
        };
        let start = Instant::now();
        self.draw(img, area);
        let took = start.elapsed();
        if let Some((cw, ch)) = self.fit(img.w, img.h, area).filter(|&(_, ch)| ch > 0) {
            let per_row = took / ch as u32;
            self.row_time = match self.row_time {
                Some((w, d, n)) if w == cw => Some((w, d.min(per_row), n.saturating_add(1))),
                _ => Some((cw, per_row, 1)),
            };
        }
        Shown { vblank: vblank.unwrap_or(start), start, took }
    }

    /// 絵を area の左上から、いますぐ描く。area と画面からはみ出す分は切る。
    pub fn draw(&mut self, img: &Image, area: Rect) {
        let Some((cw, ch)) = self.fit(img.w, img.h, area) else {
            return;
        };
        let (stride, w) = (self.stride, img.w as usize);
        let base = area.y as usize * stride + area.x as usize * 4;
        let mem = self.mem();
        for r in 0..ch {
            let dst = &mut mem[base + r * stride..][..cw * 4];
            match img.order {
                Order::BgraBottomUp => {
                    let src_row = img.h as usize - 1 - r;
                    dst.copy_from_slice(&img.data[src_row * w * 4..][..cw * 4]);
                }
                Order::RgbaTopDown => {
                    let src = &img.data[r * w * 4..][..cw * 4];
                    for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                        d.copy_from_slice(&[s[2], s[1], s[0], 0xff]);
                    }
                }
            }
        }
    }

    /// 矩形を黒で塗る。
    pub fn clear(&mut self, area: Rect) {
        let Some((cw, ch)) = self.fit(area.w, area.h, area) else {
            return;
        };
        let stride = self.stride;
        let base = area.y as usize * stride + area.x as usize * 4;
        let mem = self.mem();
        for r in 0..ch {
            mem[base + r * stride..][..cw * 4].fill(0);
        }
    }
}

impl Drop for Fb {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.map.cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 幅 width・高さ height の画面を模したメモリ (stride は幅より 1 画素ぶん広い)。
    /// テストの終わりに mem::forget して、Fb の drop で munmap させないこと。
    fn fake(width: u32, height: u32, buf: &mut Vec<u8>) -> Fb {
        let stride = (width as usize + 1) * 4;
        buf.resize(stride * height as usize, 0);
        let file = File::open("/dev/null").unwrap();
        Fb {
            file,
            map: buf.as_mut_ptr(),
            len: buf.len(),
            width,
            height,
            stride,
            vsync: false,
            scan: None,
            period: Duration::ZERO,
            row_time: None,
        }
    }

    #[test]
    fn flips_bgra_and_clips_to_area() {
        let mut buf = Vec::new();
        let mut fb = fake(4, 3, &mut buf);
        // 2x2 の絵。下の行 (画素 1, 2) が先頭
        let data = vec![1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4];
        let img = Image { data, w: 2, h: 2, order: Order::BgraBottomUp };
        fb.draw(&img, Rect { x: 1, y: 1, w: 1, h: 2 });
        std::mem::forget(fb);
        let px = |x: usize, y: usize| buf[y * 20 + x * 4];
        // 幅 1 に切られ、上下が入れ替わる
        assert_eq!((px(1, 1), px(1, 2), px(2, 1), px(0, 1)), (3, 1, 0, 0));
    }

    #[test]
    fn swaps_rgba_to_bgrx() {
        let mut buf = Vec::new();
        let mut fb = fake(2, 1, &mut buf);
        let img = Image { data: vec![10, 20, 30, 0], w: 1, h: 1, order: Order::RgbaTopDown };
        fb.draw(&img, Rect { x: 0, y: 0, w: 2, h: 1 });
        std::mem::forget(fb);
        assert_eq!(&buf[0..4], &[30, 20, 10, 0xff]);
    }
}
