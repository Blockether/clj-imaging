# clj-imaging

Images, SVG rasterization and plotting for Clojure — **without `java.desktop`**.

`com.blockether/imaging` is a thin Clojure FFM (Java 22+ `java.lang.foreign`) binding over a
small Rust cdylib built on [`image`](https://crates.io/crates/image),
[`resvg`/`usvg`](https://crates.io/crates/resvg) and [`tiny-skia`](https://crates.io/crates/tiny-skia).

Why: AWT / Java2D / ImageIO drag the whole `java.desktop` module and its JNI surface into a
GraalVM native image. This library replaces that with one self-contained native library, so
decoding, drawing, text, SVG and charts all work in a native binary with nothing on `PATH`.

* Pure-Rust codecs: PNG, JPEG, WebP (incl. **lossless** encode), GIF, BMP, TIFF, ICO, TGA, PNM, DDS, farbfeld, QOI
* SVG → raster via resvg, with the same font stack
* Four Noto faces embedded (sans, sans-bold, sans-italic, mono) — no system fonts required
* A declarative plotting DSL (lines, areas, steps, bars, hbars, stacked bars, scatter, histogram, pie, donut, heatmap)
* No AWT, no ImageIO, no `BufferedImage`, no service loaders

## Install

```clojure
;; deps.edn
com.blockether/imaging {:mvn/version "0.1.0"}
;; plus the native library for your platform
com.blockether/imaging-native-darwin-arm64 {:mvn/version "0.1.0"}
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

(img/probe bytes)                  ;; => {:format "png" :width 1200 :height 800 :frames 1 ...}
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
