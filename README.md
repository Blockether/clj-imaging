# clj-imaging

Images, SVG rasterization and plotting for Clojure — **without `java.desktop`**.

`com.blockether/imaging` is a thin Clojure FFM (Java 22+ `java.lang.foreign`) binding over a
small Rust cdylib built on [`image`](https://crates.io/crates/image),
[`resvg`/`usvg`](https://crates.io/crates/resvg), [`tiny-skia`](https://crates.io/crates/tiny-skia),
[`re_mp4`](https://crates.io/crates/re_mp4) and [`rust_h264`](https://crates.io/crates/rust_h264).

Why: AWT / Java2D / ImageIO drag the whole `java.desktop` module and its JNI surface into a
GraalVM native image. This library replaces that with one self-contained native library, so
decoding, drawing, text, SVG and charts all work in a native binary with nothing on `PATH`.

* Pure-Rust codecs: PNG, JPEG, WebP (incl. **lossless** encode), GIF, BMP, TIFF, ICO, TGA, PNM, DDS, farbfeld, QOI
* SVG → raster via resvg, with the same font stack
* Four Noto faces embedded (sans, sans-bold, sans-italic, mono) — no system fonts required
* A declarative plotting DSL (lines, areas, steps, bars, hbars, stacked bars, scatter, histogram, pie, donut, heatmap)
* **MP4 video**: demux + H.264 decode to frames, or transcode straight to an animated GIF — no `ffmpeg`, no system codec
* No AWT, no ImageIO, no `BufferedImage`, no service loaders

## Install

```clojure
;; deps.edn
com.blockether/imaging {:mvn/version "0.1.2"}
;; plus the native library for your platform
com.blockether/imaging-native-darwin-arm64 {:mvn/version "0.1.2"}
```

Native artifacts are published per platform: `darwin-arm64`, `darwin-x64`,
`linux-x64`, `linux-arm64`, `windows-x64`.

Requires JDK 22+ (FFM). Run with `--enable-native-access=ALL-UNNAMED` to silence the JDK 24+ warning.

## Usage

```clojure
(require '[com.blockether.imaging :as img])

(with-open [im (img/decode (io/file "photo.jpg"))]
  (-> im
      (img/thumbnail 800 800)
      (img/adjust {:brightness 4 :contrast 8})
      (img/save! "out.webp")))

(img/probe bytes)                  ;; => {:format "png" :width 1200 :height 800 :frames 1
                                   ;;     :color "l8" :channels 1 :bits 8 :is-grayscale true ...}
                                   ;; the colour keys describe the SOURCE; a decoded Img is RGBA8
(img/convert bytes {:format "webp" :max-width 1600 :max-bytes 200000})  ;; => byte[]
(img/resize im 400 300)            ;; exact size    (img/thumbnail im 256 256) fits a box

;; drawing + text, on a fresh canvas
(with-open [im (img/blank 640 240 "#101014")]
  (img/draw! im [{:op :rect :x 20 :y 20 :w 200 :h 80 :fill "#3b82f6" :radius 12}
                 {:op :line :x1 0 :y1 0 :x2 640 :y2 240 :stroke "#ffffff40" :stroke-width 2}
                 {:op :text :x 40 :y 160 :text "hello" :size 32 :fill "#fff" :weight 700}])
  (img/encode im :png))

(img/text-measure {:text "hello" :size 32 :weight 700}) ;; => {:width .. :height .. :x .. :y .. :lines 1}
(img/render-svg svg-string {:width 512})                ;; => Img
```

Every op is a map with `:op`; batch style keys (`:fill` `:stroke` `:stroke-width` `:size`
`:family` `:weight` `:italic`) can be set per batch or per op. WebP encoding is lossless.

### Video (MP4 / H.264)

```clojure
(img/mp4? "clip.mp4")                                   ;; => true (ftyp sniff)
(img/probe-video "clip.mp4")
;; => {:format "mp4" :codec "avc1.640028" :codec-kind "h264" :is-decodable true
;;     :width 1920 :height 1080 :frames 300 :duration-s 10.0 :fps 30.0 :has-audio true}

;; Frames, bounded — every frame is a full uncompressed canvas.
(img/decode-video "clip.mp4" {:max-frames 30 :stride 2 :max-dimension 640})
;; => {:width 640 :height 360 :fps 30.0 :frame-count 30 :total-frames 300
;;     :frames [{:index 0 :timestamp-s 0.0 :data #bytes} ...]}   ; :data = straight RGBA8

;; ...or PNG/JPEG stills instead of raw rows:
(img/decode-video "clip.mp4" {:encoding "png" :max-frames 1})

;; Whole clip → one animated GIF, without the frames ever crossing the FFI:
(img/video->gif "clip.mp4" {:max-dimension 480 :stride 2})   ;; => byte[]
```

Frames come back in **display** order (B-frames reordered), display-cropped (H.264 codes
1080p as 1920×1088) and as straight RGBA8, so they drop into `from-pixels` unchanged.
H.264 Baseline/Main/High decode; HEVC, AV1 and VP9 are demuxed but report
`:is-decodable false` instead of a corrupt picture.

### Plotting

```clojure
(require '[com.blockether.imaging.plot :as plot])

(-> (plot/figure {:width 900 :height 500 :title "Latency" :theme :dark
                  :x {:label "run"} :y {:label "ms"}})
    (plot/lines {:name "p50" :y [12 14 11 15 13]})
    (plot/areas {:name "p99" :y [40 52 48 61 55] :fill "#ef444455"})
    (plot/save! "latency.png"))

(plot/bytes (-> (plot/figure {:title "share"})
                (plot/donut {:values [30 25 45] :labels ["a" "b" "c"]})))
```

Every series takes `:name`, `:color` and either `:y` (implicit x) or `:x`/`:y` pairs;
`histogram` takes `:values` + `:bins`, `heatmap` takes a row-major `:z` matrix.

## Memory model

`Img` handles are `AutoCloseable` and also registered with a `Cleaner`, guarded by one
shared CAS flag — closing twice, or closing and then letting the GC collect, frees exactly
once. Prefer `with-open`; forgetting it leaks nothing. Hot paths use `Arena.ofConfined`; long-lived handles use `Arena.ofAuto`.

## Building

```bash
scripts/build-natives.sh darwin-arm64     # cargo build --release, stage into resources/prebuilds/<platform>
clojure -X:test                           # 16 tests against the real cdylib
clojure -T:build jar                      # the pure-Clojure jar
clojure -T:build native-jar :platform '"darwin-arm64"'
```

CI builds all five platforms; pushing a `v*` tag deploys `imaging` and every
`imaging-native-<platform>` to Clojars.

## License

MIT. Bundled Noto fonts are SIL OFL 1.1.
