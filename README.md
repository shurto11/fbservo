# fbservo

[Servo](https://servo.org/) (Rust 製のブラウザエンジン) をプロセスに組み込み、ページの絵を
フレームバッファ (`/dev/fb0`) へ直接書く試作。

[fbrowse](../fbrowse) は Chromium の画面を CDP 経由で JPEG として受け取って描くため、
文字がにじみ、遅延も大きい。fbservo は Servo のソフトウェア描画 (Mesa の llvmpipe) の
結果を無圧縮の RGBA で読み出し、そのまま fb0 に書く。X も Chromium も要らない。
JavaScript は Servo 内蔵の SpiderMonkey が動かす。

いまは「Servo で普段のサイトがどこまで見られるか・どれくらい速いか」を測るための試作で、
fb-server 連携、tmux ペイン追従、リンクヒント、動画再生は無い。
fbrowse に追いつくまでの順番は [ROADMAP.md](ROADMAP.md) にまとめてある。

## 必要なもの

- Rust 1.88 以上
- clang / libclang (bindgen 用)、cmake、pkg-config
- `libfontconfig1-dev` (Servo のフォント探索が fontconfig を直接リンクする。freetype は同梱版を使う)
- Mesa (EGL・ソフトウェア GL。DISPLAY が無い TTY では EGL surfaceless で動く)
- `/dev/fb0` への書き込み権限 (video グループ)

## ビルドと起動

```bash
cargo build --release      # 初回は依存 (Servo 一式・約 800 crate) のビルドに数時間かかる
./target/release/fbservo https://ja.wikipedia.org/
./target/release/fbservo page.html          # ローカルのファイル
./target/release/fbservo rust 言語           # 検索語は DuckDuckGo で検索
./target/release/fbservo                     # 起動してすぐ URL を入力
./target/release/fbservo --scale 1.25 URL    # HiDPI 倍率
```

`.cargo/config.toml` で並列数を 2 に抑え、リンクには lld を使っている
(メモリ 3.7GB のマシンでもビルドできるように)。

## キー操作

fbrowse と同じ配置。

| キー | 動作 |
|---|---|
| `h` `j` `k` `l` | スクロール (60px ずつ) |
| `gg` / `G` | 先頭 / 末尾 |
| `f` | リンクヒント。クリックできる要素に 2 文字のラベルが出るので、それを打つとクリックする。1 文字目で合わないラベルは消える。`f` / `Esc` で取消、`Backspace` で打ち直し |
| `i` | 入力モード。キーをそのままページへ送る (`Tab` でリンクやフォームへ移動)。`Enter` はページへ送ってから、`Esc` はそのまま、ノーマルモードへ戻る |
| `d` | 直前の入力モードで打った文字を消す |
| `b` / `r` | 戻る / 再読込 |
| `Enter` `Space` 矢印 `PageUp` `PageDown` `Home` `End` | そのままページへ送る (入力欄にフォーカスが無ければ、Space で 1 画面、矢印で少しスクロールする) |
| `s` | 描き直し |
| `+` `-` `0` | ズームイン / アウト / リセット (10% 刻み) |
| `t` | 新しいタブ (端末の最下行に URL・ファイル・検索語を入れる) |
| `]` / `[` | 次 / 前のタブ |
| `x` | タブを閉じる |
| `y` | 今の URL を tmux のペーストバッファへコピー (`prefix+]` で貼り付け) |
| `?` | キー一覧 (何かキーを押すと戻る) |
| `Q` / `Ctrl-C` | 終了。`Ctrl-C` は入力モードやプロンプトの途中でも効く |

fbservo だけのキー: `L` で進む、`O` で今の URL を編集して開く。

`m` (カーソル)、`o` (お気に入り)、`p` (クリップボード)、`T` (タブを別ペインへ)、
`M` `P` (mpv) はまだ無い。押すとログに「まだありません」と出る ([ROADMAP.md](ROADMAP.md))。
キー操作の結果は、いまはログに `[ui] ...` として出る。

## ログと計測

動作中は端末に何も出さない (出すとページの絵を上書きするため)。Servo のログ、
URL・タイトル・読み込み状態、1 フレームごとの時間は `$XDG_RUNTIME_DIR/fbservo.log` に書く。

終了すると、フレーム時間 (paint / readback / fb 転送 の平均と最大) と最大メモリを表示する
(下は表示の形式。数値は例ではない)。

```
フレーム数: N
  paint     平均   xx.x ms  最大   xx.x ms
  readback  平均   xx.x ms  最大   xx.x ms
  fb 転送   平均   xx.x ms  最大   xx.x ms
  合計      平均   xx.x ms
キー→画面 平均   xx.x ms  最大   xx.x ms (N 回)
最大メモリ (RSS): NNN MB
```

「キー→画面」は、スクロールキーを押してからその結果が fb0 に出るまでの時間。
先頭で `k` を押したときのように画面が変わらないキーは、500ms 経ったら数えずに捨てる。
ページの JS コンソール出力もログに `[console Error] ...` のように残る。

## 仕組み

```
Servo (DOM / Stylo / SpiderMonkey / WebRender)
   │  SoftwareRenderingContext (Mesa llvmpipe, EGL surfaceless)
   ▼
read_to_image → RGBA (上下反転済み)
   ▼
RGBA → BGRX に並べ替えて 1 行ずつ /dev/fb0 へ (stride は sysfs から)
```

- Servo の型は `Rc` なので、すべて 1 スレッドで扱う。Servo が別スレッドから起こしたいときは
  `EventLoopWaker` が pipe に 1 バイト書き、メインループは標準入力とその pipe を `poll` する。
- `notify_new_frame_ready` が来たら `paint` → `read_to_image` → `present` → fb0 に書く。
- 端末 (tmux の時計など) に上書きされても戻るよう、変化が無くても 1 秒ごとに直前のフレームを描き直す。
- 書き込み先は `FBSERVO_FB` で差し替えられる (動作確認用。サイズは fb0 と同じ前提)。

## Servo への修正 (`vendor/servo-paint`)

`servo-paint` 0.7.0 を `vendor/` に置き、`Cargo.toml` の `[patch.crates-io]` で差し替えている。
変えたのは `webview_renderer.rs` の `scroll_node_or_ancestor_skipping_subpixel` だけ。

- 元の Servo は、スクロールを画面上のその位置にある最も内側のスクロール領域へ先に渡す。
  そのとき、はみ出しが 1px 未満の `overflow: auto` の箱 (ja.wikipedia トップの「ようこそ」の枠など)
  もスクロールできる領域として扱う。
- すると `j` 1 回分がその箱を 0.00002px ほど動かすだけで使い切られ、画面は変わらない。
  先頭で `k` を押すと箱が元に戻るので、次の `j` もまた吸われる。
- 修正版は、スクロールした軸の方向に 1px 以上動ける領域だけを対象にする。
- `testpages/subpixel-overflow.html` で確かめられる。中央の箱の上で `j` `k` を押したとき、
  1 回目から毎回動けば正しい。Servo を上げるときは、パッチを外してこのページで再現するか見る。

## ファイル

| ファイル | 内容 |
|---|---|
| `src/main.rs` | 引数、メインループ、キー操作、計測 |
| `src/engine.rs` | Servo / WebView の生成と操作、waker、delegate |
| `src/fb.rs` | fb0 への書き込み |
| `src/term.rs` | 端末の raw モード、キー列の解釈 |
| `src/hints.rs` | リンクヒントの JS と結果の読み取り |
| `src/tmux.rs` | tmux のペーストバッファへのコピー |
| `vendor/servo-paint/` | 上の修正を入れた servo-paint 0.7.0 (MPL-2.0) |
| `testpages/subpixel-overflow.html` | その修正の確認用ページ |
