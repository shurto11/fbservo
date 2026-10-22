//! フレームバッファ (/dev/fb0) への書き込み。
//!
//! Servo から受け取る RGBA (上が先頭の行) を、fb0 の XRGB8888 (メモリ上は B,G,R,X) に
//! 並べ替えて 1 行ずつ書く。サイズと行幅は sysfs から読む。

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;

pub struct Fb {
    file: File,
    pub width: u32,
    pub height: u32,
    /// 1 行のバイト数 (表示幅 × 4 より大きいことがある)
    stride: usize,
    /// 1 行ぶんの変換用バッファ
    row: Vec<u8>,
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
        let stride: usize = read_sysfs("stride")?.parse()?;
        let path = std::env::var("FBSERVO_FB").unwrap_or_else(|_| "/dev/fb0".to_string());
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("{path} を開けません (video グループに入っていますか)"))?;
        Ok(Fb { file, width: w.parse()?, height: h.parse()?, stride, row: Vec::new() })
    }

    /// RGBA 画像 (幅 w・高さ h) を画面の (x, y) に描く。画面外ははみ出し分を切る。
    pub fn blit(&mut self, rgba: &[u8], w: u32, h: u32, x: u32, y: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let cw = w.min(self.width - x) as usize;
        let ch = h.min(self.height - y) as usize;
        self.row.resize(cw * 4, 0);
        for r in 0..ch {
            let src = &rgba[r * w as usize * 4..][..cw * 4];
            for (d, s) in self.row.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 0xff;
            }
            let off = (y as usize + r) * self.stride + x as usize * 4;
            let _ = self.file.write_at(&self.row, off as u64);
        }
    }

    /// 画面全体を黒で塗る。
    pub fn clear(&mut self) {
        let line = vec![0u8; self.stride];
        for r in 0..self.height as usize {
            let _ = self.file.write_at(&line, (r * self.stride) as u64);
        }
    }
}
