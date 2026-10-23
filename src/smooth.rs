//! j/k/h/l のスクロールを一定の速さで流す。
//!
//! 速さは Vimium で長押ししたときと同じくらい (毎秒 1000px) にし、動き出しも止まる前も
//! 変えない。1 コマで「速さ × そのコマを出すまでの間隔」だけ送る。重いページで 30 コマ/秒に
//! 落とすときは 1 コマで 2 コマ分進むので、見かけの速さは変わらない (間隔は `Pace` が決める)。
//!
//! キーを押している間 (キーボードから離上が分かるとき) は流し続け、離したら止まる。
//! ただし押してすぐ離しても 1 回分 (60px) は進む (Vimium と同じ)。
//! 離上が分からないときは、端末から文字が届くたびに 1 回分ずつ足す。キーリピートは毎秒 30 回
//! (1800px/秒) 届いて速さを上回るので、溜めるのは 2 回分までにし、離したあとに流れ続けるのを
//! 0.1 秒ほどに抑える。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 流す速さ (px/秒)
const SPEED: f32 = 1000.0;
/// 送った分がこれだけ経っても描かれなければ、ページの端に着いて動けなかったとみなす
const STUCK: Duration = Duration::from_millis(150);
/// 描かれるのを待っている間に、端に着いていないか確かめに来る間隔 (ms)
const CHECK_MS: i32 = 16;

#[derive(Default)]
pub struct Smooth {
    /// 押したままのキー。離すまで step の向きに流す
    held: Option<char>,
    /// 最後に押したキーの 1 回分 (向きと量)
    step: [f32; 2],
    /// 押したまま流し始めてから送った距離
    moved: f32,
    /// まだ送っていない距離 (px、整数)。押したまま流している間は使わない
    left: [f32; 2],
    /// 整数の px にしたときの端数。次のコマに回す
    carry: [f32; 2],
    /// 送った分がまだ描かれていなければ、(送った時刻, そのコマを出すまでの間隔)
    in_flight: Option<(Instant, Duration)>,
}

impl Smooth {
    /// キー key を押したまま流し始める。(dx, dy) は 1 回分。
    pub fn hold(&mut self, key: char, dx: f32, dy: f32) {
        self.turn([dx, dy]);
        self.held = Some(key);
        self.moved = 0.0;
        self.left = [0.0; 2];
    }

    /// 1 回分 (dx, dy) 進める。同じ向きなら足すが、溜めるのは 2 回分まで。
    pub fn add(&mut self, dx: f32, dy: f32) {
        self.turn([dx, dy]);
        self.held = None;
        for (l, d) in self.left.iter_mut().zip([dx, dy]) {
            *l = (*l + d).clamp(-2.0 * d.abs(), 2.0 * d.abs());
        }
    }

    /// 違うキーを押したら、前のキーの残りは捨てる (j のあとすぐ k を押したら、すぐ上へ向かう)。
    fn turn(&mut self, step: [f32; 2]) {
        if self.step != step {
            self.step = step;
            self.left = [0.0; 2];
            self.carry = [0.0; 2];
        }
    }

    /// 押したままのキーを離した。流し始めてから 1 回分に満たなければ、そこまでは進める。
    pub fn release(&mut self) {
        if self.held.take().is_some() {
            let len = hypot(self.step);
            let rest = (len - self.moved).max(0.0);
            self.left = self.step.map(|d| d / len * rest);
        }
    }

    pub fn held(&self) -> Option<char> {
        self.held
    }

    /// 残りを捨てる (gg や別のページへの移動など、ほかの操作をしたとき)。
    pub fn stop(&mut self) {
        *self = Smooth::default();
    }

    /// 送った分のコマが描かれた。(送った時刻, そのコマを出すまでの間隔) を返す (送っていなければ None)。
    pub fn shown(&mut self) -> Option<(Instant, Duration)> {
        self.in_flight.take()
    }

    /// まだ送る分があるか、送った分が描かれるのを待っている。
    pub fn active(&self) -> bool {
        self.held.is_some() || self.has_left() || self.in_flight.is_some()
    }

    fn has_left(&self) -> bool {
        self.left.iter().any(|v| v.abs() >= 0.5)
    }

    /// poll で待ってよい時間 (ms)。流していなければ None。
    pub fn timeout(&self) -> Option<i32> {
        if self.in_flight.is_some() {
            Some(CHECK_MS)
        } else if self.active() {
            Some(0)
        } else {
            None
        }
    }

    /// 次に送る量 (整数の px)。interval はこのコマを前のコマから何 ms あけて出すか。
    /// 前に送った分がまだ描かれていない、進む分が無い、のどちらかなら None。
    pub fn next(&mut self, now: Instant, interval: Duration) -> Option<(f32, f32)> {
        if let Some((t, _)) = self.in_flight {
            if now - t > STUCK {
                self.stop();
            }
            return None;
        }
        let budget = SPEED * interval.as_secs_f32();
        let mut out = [0.0; 2];
        if self.held.is_some() {
            let len = hypot(self.step);
            for i in 0..2 {
                // 文字がにじまないよう整数の px で送り、端数は次に回す
                let v = self.step[i] / len * budget + self.carry[i];
                out[i] = v.round();
                self.carry[i] = v - out[i];
            }
            self.moved += hypot(out);
        } else {
            if !self.has_left() {
                self.left = [0.0; 2];
                return None;
            }
            let len = hypot(self.left);
            for i in 0..2 {
                let v = self.left[i] / len * budget + self.carry[i];
                // 残りを超えては送らない (残りは整数なので、最後はぴったり 0 になる)
                out[i] = v.round().clamp(-self.left[i].abs(), self.left[i].abs());
                self.carry[i] = v - out[i];
                self.left[i] -= out[i];
            }
            if !self.has_left() {
                self.carry = [0.0; 2];
            }
        }
        self.in_flight = Some((now, interval));
        Some((out[0], out[1]))
    }
}

fn hypot(v: [f32; 2]) -> f32 {
    v[0].hypot(v[1])
}

/// スクロール中、1 コマを何回の垂直同期ごとに出すか (60 / 30 / 20 コマ/秒) を決める。
///
/// 長いページでは Servo (WebRender) が 1 コマ組むのに 20ms 以上かかり、16.7ms ごとには出せない。
/// 間に合ったり間に合わなかったりすると、コマの間隔が 17ms と 33ms で揺れてカクカクして見える
/// ので、直近のコマが間に合う間隔にそろえる。
///
/// 1 コマに要る時間は、「スクロールを送ってから Servo がコマを組み終え、paint するまで」と
/// 「paint・読み出し・fb0 への転送」の長いほう (前者は次のコマを組む間に後者をするので、
/// 足さなくてよい)。前のコマを出してから次のコマを読み終えるまでを測ると、間隔に余裕が
/// あるときは Servo を待つ時間が垂直同期を待つ間に隠れてしまい、軽く見えてしまう。
pub struct Pace {
    /// 垂直同期の間隔
    period: Duration,
    /// 直近のコマに要った時間
    costs: VecDeque<Duration>,
    /// いまの間隔 (垂直同期の何回分か)
    n: u32,
}

impl Pace {
    /// 間隔を決めるのに使うコマ数。このうち 3 コマが遅ければ間隔を広げ、1 コマを除いて
    /// 間に合っていれば詰める (新しく見えた文字を描くときなど、たまに遅いコマがあるので、
    /// 1 コマだけで決めると、遅いまま WINDOW コマ続いてしまう)
    const WINDOW: usize = 8;
    /// 間隔を詰めるとき、所要時間に足す余裕 (広げるときは足さない。所要時間は前のコマを
    /// 垂直同期ぴったりに出した場合で見積もっていて、ふつうはそれより余裕がある)
    const MARGIN: Duration = Duration::from_millis(2);

    pub fn new(period: Duration) -> Pace {
        Pace { period, costs: VecDeque::new(), n: 1 }
    }

    pub fn add(&mut self, cost: Duration) {
        if self.costs.len() == Self::WINDOW {
            self.costs.pop_front();
        }
        self.costs.push_back(cost);
        let mut sorted: Vec<Duration> = self.costs.iter().copied().collect();
        sorted.sort();
        let need = |c: Duration| (c.as_secs_f32() / self.period.as_secs_f32()).ceil().clamp(1.0, 3.0) as u32;
        let nth_slowest = |i: usize| sorted[sorted.len().saturating_sub(i)];
        let fast = need(nth_slowest(2) + Self::MARGIN);
        let slow = need(nth_slowest(3));
        if fast < self.n {
            self.n = fast;
        } else if slow > self.n {
            self.n = slow;
        }
    }

    /// 次のコマを前のコマから何 ms あけて出すか (垂直同期の 1〜3 回分)
    pub fn interval(&self) -> Duration {
        self.period * self.n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F60: Duration = Duration::from_micros(16_667);

    /// 1 コマごとに描かれたことにして、送った量を全部集める
    fn run(s: &mut Smooth, frames: usize, interval: Duration) -> Vec<f32> {
        let mut out = Vec::new();
        let mut now = Instant::now();
        for _ in 0..frames {
            now += interval;
            if let Some((_, dy)) = s.next(now, interval) {
                out.push(dy);
                s.shown();
            }
        }
        out
    }

    #[test]
    fn tap_moves_one_step_at_constant_speed() {
        let mut s = Smooth::default();
        s.add(0.0, 60.0);
        let steps = run(&mut s, 20, F60);
        // 1000px/秒 なら 1 コマ 16〜17px。最後だけ残りの分
        assert_eq!(steps, vec![17.0, 16.0, 17.0, 10.0]);
        assert!(!s.active());
        // 30 コマ/秒なら 1 コマで 2 コマ分進む
        s.add(0.0, -60.0);
        assert_eq!(run(&mut s, 20, F60 * 2), vec![-33.0, -27.0]);
    }

    #[test]
    fn hold_flows_until_release() {
        let mut s = Smooth::default();
        s.hold('j', 0.0, 60.0);
        let steps = run(&mut s, 60, F60);
        assert_eq!(steps.len(), 60);
        // 1 秒で 1000px。どのコマも 16〜17px
        assert_eq!(steps.iter().sum::<f32>(), 1000.0);
        assert!(steps.iter().all(|&v| v == 16.0 || v == 17.0));
        assert_eq!(s.held(), Some('j'));
        // 離したらもう進まない
        s.release();
        assert!(run(&mut s, 10, F60).is_empty());
        assert!(!s.active());
    }

    #[test]
    fn short_hold_still_moves_one_step() {
        let mut s = Smooth::default();
        s.hold('j', 0.0, 60.0);
        let first = run(&mut s, 1, F60);
        s.release();
        let rest = run(&mut s, 10, F60);
        assert_eq!(first.iter().chain(&rest).sum::<f32>(), 60.0);
    }

    #[test]
    fn repeats_are_capped_and_turning_drops_the_rest() {
        let mut s = Smooth::default();
        for _ in 0..10 {
            s.add(0.0, 60.0);
        }
        assert_eq!(run(&mut s, 30, F60).iter().sum::<f32>(), 120.0);
        // j の残りがあっても、k を押したらすぐ上へ
        s.add(0.0, 60.0);
        s.add(0.0, -60.0);
        assert_eq!(run(&mut s, 30, F60).iter().sum::<f32>(), -60.0);
    }

    #[test]
    fn waits_for_frame_and_gives_up_at_page_end() {
        let t0 = Instant::now();
        let mut s = Smooth::default();
        s.add(0.0, 60.0);
        assert!(s.next(t0, F60).is_some());
        // 描かれるまでは次を送らない
        assert!(s.next(t0 + Duration::from_millis(40), F60).is_none());
        // 描かれないまま時間が経ったら、端に着いたとみなして残りを捨てる
        assert!(s.next(t0 + Duration::from_millis(200), F60).is_none());
        assert_eq!(s.timeout(), None);
    }

    #[test]
    fn pace_follows_slow_frames() {
        let ms = Duration::from_millis;
        let mut p = Pace::new(F60);
        assert_eq!(p.interval(), F60);
        p.add(ms(10));
        assert_eq!(p.interval(), F60);
        // 2 コマ遅くても落とさないが、3 コマ遅ければ 30 コマ/秒に落とす
        p.add(ms(24));
        p.add(ms(24));
        assert_eq!(p.interval(), F60);
        p.add(ms(24));
        assert_eq!(p.interval(), F60 * 2);
        // 30 コマ/秒の間にたまに 40ms かかっても、20 コマ/秒には落とさない
        p.add(ms(40));
        p.add(ms(57));
        assert_eq!(p.interval(), F60 * 2);
        // 33.3ms にぎりぎり収まるコマが続いても落とさない
        p.add(ms(32));
        p.add(ms(32));
        assert_eq!(p.interval(), F60 * 2);
        // 1 コマを除いて速くなったら戻す
        for _ in 0..Pace::WINDOW - 1 {
            p.add(ms(10));
        }
        assert_eq!(p.interval(), F60);
        // 最初から重いページなら、1 コマ目で落とす
        let mut p = Pace::new(F60);
        p.add(ms(24));
        assert_eq!(p.interval(), F60 * 2);
    }
}
