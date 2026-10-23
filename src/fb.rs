//! フレームバッファ (/dev/fb0) への書き込み。
//!
//! fb0 を mmap し、Servo から読み出した絵を行ごとにコピーする。fb0 は XRGB8888
//! (メモリ上は B,G,R,X) なので、GL から BGRA で読んだ絵なら並べ替えずにそのまま書ける。
//! サイズと行幅は sysfs から読む。描く場所は画面全体か、tmux ペインの矩形 (`Rect`)。

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;

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
    vsync: bool,
}

fn read_sysfs(name: &str) -> Result<String> {
    let p = format!("/sys/class/graphics/fb0/{name}");
    Ok(std::fs::read_to_string(&p).with_context(|| format!("{p} を読めません"))?.trim().to_string())
}

/// linux/fb.h の FBIO_WAITFORVSYNC (_IOW('F', 0x20, __u32))
const FBIO_WAITFORVSYNC: libc::c_ulong = 0x4004_4620;

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
        Ok(Fb { file, map: map.cast(), len, width, height, stride, vsync })
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
        if unsafe { libc::ioctl(self.file.as_raw_fd(), FBIO_WAITFORVSYNC, &arg) } != 0 {
            self.vsync = false;
        }
    }

    /// 絵を area の左上から描く。area と画面からはみ出す分は切る。
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
        Fb { file, map: buf.as_mut_ptr(), len: buf.len(), width, height, stride, vsync: false }
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
