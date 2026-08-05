(ns com.blockether.imaging-test
  "Tests run against the REAL imaging-c cdylib (build it first:
   `scripts/build-natives.sh <platform>`), so they also prove the FFM binding,
   the JSON contracts and the memory ownership rules — not just the Clojure."
  (:require [clojure.java.io :as io]
            [clojure.string :as str]
            [clojure.test :refer [deftest is testing]]
            [com.blockether.imaging :as img]
            [com.blockether.imaging.plot :as plot])
  (:import [java.awt.image BufferedImage ConvolveOp Kernel]
           [java.io ByteArrayInputStream ByteArrayOutputStream]
           [javax.imageio ImageIO]))

(defn- png-bytes
  "A small opaque red PNG, produced by the library itself."
  ^bytes []
  (with-open [im (img/blank 16 12 :red)]
    (img/encode im :png)))

(deftest version-and-fonts
  (is (string? (img/version)))
  (is (not (str/blank? (img/version))))
  (testing "the four Noto faces are embedded — no host fonts, no fontconfig"
    (let [fams (img/fonts)]
      (is (vector? fams))
      (is (some #(str/includes? (str/lower-case %) "noto") fams)))))

(deftest colors
  (is (= 0x000000ff (img/color :black)))
  (is (= 0xffffffff (img/color "#fff")))
  (is (= 0x102030ff (img/color "#102030")))
  (is (= 0x10203040 (img/color "#10203040")))
  (is (= 0xff0000ff (img/color [255 0 0])))
  (is (= 0xff000080 (img/color [255 0 0 128])))
  (is (= 0x00000000 (img/color nil)))
  (is (thrown? clojure.lang.ExceptionInfo (img/color "not-a-colour"))))

(deftest blank-info-and-pixels
  (with-open [im (img/blank 8 4 :blue)]
    (is (= {:width 8 :height 4 :is-opaque true :is-grayscale false} (img/info im)))
    (is (= 8 (img/width im)))
    (is (= 4 (img/height im)))
    (is (= (* 8 4 4) (alength (img/pixels im))))
    (is (= 0x0000ffff (img/get-pixel im 0 0)))
    (is (nil? (img/get-pixel im 99 99)))))

(deftest round-trip-encode-decode
  (doseq [fmt [:png :jpeg :webp :bmp :tiff :gif :qoi]]
    (testing (str "round trip through " (name fmt))
      (with-open [src (img/blank 20 10 :red)]
        (let [bs (img/encode src fmt 90)]
          (is (pos? (alength bs)) (str fmt " encoded to nothing"))
          (with-open [back (img/decode bs)]
            (is (= 20 (img/width back))))
          (with-open [back (img/decode bs)]
            (is (= 20 (img/width back)))
            (is (= 10 (img/height back)))))))))

(defn- solid-rgba
  "A straight RGBA8 buffer for a `w`x`h` solid fill of `rgba` (0xRRGGBBAA,
   the library's int colour layout)."
  ^bytes [w h rgba]
  (let [bs (byte-array (* w h 4))
        r (unchecked-byte (bit-and 0xff (bit-shift-right rgba 24)))
        g (unchecked-byte (bit-and 0xff (bit-shift-right rgba 16)))
        b (unchecked-byte (bit-and 0xff (bit-shift-right rgba 8)))
        a (unchecked-byte (bit-and 0xff rgba))]
    (dotimes [i (* w h)]
      (aset bs (+ 0 (* 4 i)) r)
      (aset bs (+ 1 (* 4 i)) g)
      (aset bs (+ 2 (* 4 i)) b)
      (aset bs (+ 3 (* 4 i)) a))
    bs))

(deftest animated-gif-round-trip
  ;; The native multi-frame path lives in clj-imaging now: encode a 2-frame
  ;; animation, decode it back, and assert the frames, timings and colours
  ;; survive the round trip through the image crate's GIF codec.
  (let [spec {:width 4 :height 4 :loop-count -1
              :frames [{:delay-ms 100 :rgba (solid-rgba 4 4 0xff0000ff)}
                       {:delay-ms 200 :rgba (solid-rgba 4 4 0x0000ffff)}]}
        gif (img/encode-gif spec)
        out  (img/decode-gif gif)]
    (is (= 4 (:width out)))
    (is (= 4 (:height out)))
    (is (= 2 (count (:frames out))) "both frames decoded")
    (is (= [100 200] (mapv :delay-ms (:frames out))) "per-frame delays survive")
    (let [[f1 f2] (:frames out)
          ^bytes px1 (:rgba f1)
          ^bytes px2 (:rgba f2)
          u    #(bit-and 0xff (aget ^bytes %2 %1))]
      (is (= [0xff 0x00 0x00] [(u 0 px1) (u 1 px1) (u 2 px1)]) "frame 0 is red")
      (is (= [0x00 0x00 0xff] [(u 0 px2) (u 1 px2) (u 2 px2)]) "frame 1 is blue"))
    (testing "a still GIF decodes as a single-frame animation"
      (let [still (img/encode-gif {:width 2 :height 2
                                   :frames [{:delay-ms 0 :rgba (solid-rgba 2 2 0x00ff00ff)}]})
            back  (img/decode-gif still)]
        (is (= 1 (count (:frames back))))))))

(deftest probe-without-decoding
  (let [p (img/probe (png-bytes))]
    (is (= "png" (:format p)))
    (is (= 16 (:width p)))
    (is (= 12 (:height p)))
    (is (false? (:is-animated p)))))

(defn- be32 ^bytes [n]
  (byte-array (map unchecked-byte [(bit-shift-right n 24) (bit-shift-right n 16)
                                   (bit-shift-right n 8) n])))

(defn- write-chunk! [^java.io.ByteArrayOutputStream out ^String typ ^bytes data]
  (let [t (.getBytes typ "US-ASCII")
        crc (doto (java.util.zip.CRC32.) (.update t) (.update data))]
    (.write out (be32 (alength data)) 0 4)
    (.write out t 0 4)
    (.write out data 0 (alength data))
    (.write out (be32 (unchecked-int (.getValue crc))) 0 4)))

(defn- gray-png
  "A minimal all-black GRAYSCALE PNG (colour type 0) at `depth` bits. Written by
   hand because the library only ever ENCODES rgba8 — reading this colour type
   back out of a foreign file is the whole point of the colour keys."
  ^bytes [w h depth]
  (let [raw (byte-array (* h (inc (* w (quot depth 8)))))  ; filter 0 + zero samples
        idat (let [d (doto (java.util.zip.Deflater.) (.setInput raw) (.finish))
                   bos (java.io.ByteArrayOutputStream.)
                   buf (byte-array 4096)]
               (while (not (.finished d))
                 (.write bos buf 0 (.deflate d buf)))
               (.end d)
               (.toByteArray bos))
        ihdr (byte-array (map unchecked-byte (concat (be32 w) (be32 h) [depth 0 0 0 0])))
        out (java.io.ByteArrayOutputStream.)]
    (.write out (byte-array (map unchecked-byte [0x89 0x50 0x4e 0x47 0x0d 0x0a 0x1a 0x0a])) 0 8)
    (write-chunk! out "IHDR" ihdr)
    (write-chunk! out "IDAT" idat)
    (write-chunk! out "IEND" (byte-array 0))
    (.toByteArray out)))

(defn- colour-of [p] [(:color p) (:channels p) (:bits p) (:is-grayscale p) (:has-alpha p)])

(deftest probe-reports-source-colour-type
  (testing "grayscale sources, which a decoded RGBA8 handle can no longer name"
    (is (= ["l8" 1 8 true false] (colour-of (img/probe (gray-png 8 6 8)))))
    (is (= ["l16" 1 16 true false] (colour-of (img/probe (gray-png 8 6 16))))))
  (testing "the library's own PNG output is rgba8, a JPEG rgb8"
    (is (= ["rgba8" 4 8 false true] (colour-of (img/probe (png-bytes)))))
    (is (= ["rgb8" 3 8 false false]
           (colour-of (img/probe (with-open [im (img/blank 8 6 :red)] (img/encode im :jpeg)))))))
  (testing "a vector document has no stored colour type: it rasterises to rgba8"
    (let [p (img/probe (.getBytes "<svg xmlns='http://www.w3.org/2000/svg' width='4' height='2'/>"
                                  "UTF-8"))]
      (is (= "svg" (:format p)))
      (is (= ["rgba8" 4 8 false true] (colour-of p))))))

(deftest info-reports-grayscale-pixels
  (testing "info answers from the PIXELS, so a grey fill is grayscale whatever it came from"
    (with-open [im (img/blank 4 4 "#404040")]
      (is (true? (:is-grayscale (img/info im)))))
    (with-open [im (img/blank 4 4 :red)]
      (is (false? (:is-grayscale (img/info im)))))))

(deftest transforms
  (with-open [im (img/blank 40 20 :green)]
    (with-open [r (img/resize im 10 5)]
      (is (= [10 5] [(img/width r) (img/height r)])))
    (with-open [t (img/thumbnail im 20 20)]
      (testing "thumbnail preserves aspect ratio"
        (is (= [20 10] [(img/width t) (img/height t)]))))
    (with-open [c (img/crop im 5 5 10 10)]
      (is (= [10 10] [(img/width c) (img/height c)])))
    (with-open [rot (img/rotate im 90 {:expand true})]
      (is (= [20 40] [(img/width rot) (img/height rot)])))
    (with-open [f (img/flip im :vertical)]
      (is (= [40 20] [(img/width f) (img/height f)])))
    (with-open [g (img/grayscale im)]
      (let [px (img/get-pixel g 0 0)]
        (is (= (bit-and (bit-shift-right px 24) 0xff)
               (bit-and (bit-shift-right px 16) 0xff)))))
    (with-open [b (img/blur im 2.0)]
      (is (= 40 (img/width b))))
    (with-open [a (img/adjust im {:brightness 1.2 :contrast 1.1 :saturation 0.5})]
      (is (= 40 (img/width a))))))

(deftest flatten-alpha
  (with-open [im (img/blank 4 4 [0 0 0 0])]
    (is (false? (:is-opaque (img/info im))))
    (with-open [f (img/flatten im :white)]
      (is (true? (:is-opaque (img/info f))))
      (is (= 0xffffffff (img/get-pixel f 0 0))))))

(deftest mutation-and-paste
  (with-open [dst (img/blank 10 10 :white)
              src (img/blank 4 4 :red)]
    (img/put-pixel! dst 0 0 :black)
    (is (= 0x000000ff (img/get-pixel dst 0 0)))
    (img/paste! dst src 3 3)
    (is (= 0xff0000ff (img/get-pixel dst 4 4)))
    (is (= 0xffffffff (img/get-pixel dst 9 9)))))

(deftest drawing
  (with-open [im (img/blank 60 60 :white)]
    (img/draw! im [{:op :rect :x 0 :y 0 :w 20 :h 20 :fill :red}
                   {:op :circle :cx 40 :cy 40 :r 10 :fill :blue}
                   {:op :line :x1 0 :y1 59 :x2 59 :y2 59 :stroke :black :stroke-width 2}
                   {:op :text :text "hi" :x 25 :y 15 :size 12 :fill :black}])
    (is (= 0xff0000ff (img/get-pixel im 5 5)) "rect filled")
    (is (= 0x0000ffff (img/get-pixel im 40 40)) "circle filled")
    (is (not= 0xffffffff (img/get-pixel im 30 58)) "line drawn")))

(defn- ink-count
  "How many pixels the ops actually changed, on a black canvas."
  ^long [ops]
  (with-open [im (img/blank 200 80 :black)]
    (img/draw! im ops)
    (let [^bytes b (img/pixels im)]
      (count (filter (fn [^long i]
                       (pos? (+ (bit-and 0xff (aget b (* 4 i)))
                                (bit-and 0xff (aget b (+ 1 (* 4 i))))
                                (bit-and 0xff (aget b (+ 2 (* 4 i)))))))
                     (range (quot (alength b) 4)))))))

(deftest text-actually-inks-pixels
  ;; Regression: the layout viewport is centred on the origin, so usvg's root
  ;; transform shifted every glyph 50 000 px off-canvas and text rendered as
  ;; nothing at all — while every other op worked.
  (testing "alphabetic baseline"
    (is (pos? (ink-count [{:op :text :text "Hi" :x 10 :y 50 :size 40 :fill :white}]))))
  (testing "every baseline mode lands on the canvas"
    (doseq [b [:top :middle :bottom]]
      (is (pos? (ink-count [{:op :text :text "Hi" :x 10 :y 30 :baseline b
                             :size 40 :fill :white}]))
          (str "baseline " b))))
  (testing "nothing drawn without text"
    (is (zero? (ink-count [])))))

(deftest text-measurement
  (let [m (img/text-measure {:text "hello" :size 20})]
    (is (pos? (:width m)))
    (is (pos? (:height m)))
    (is (= 1 (:lines m)))
    (testing "the ink box is relative to the run origin, not the layout viewport"
      (is (< (abs (double (:x m))) 20.0))
      (is (neg? (:y m)))
      (is (< (abs (double (:y m))) 40.0)))
    (testing "a longer run is wider"
      (is (> (:width (img/text-measure {:text "hello world" :size 20}))
             (:width m))))))

(deftest svg-rasterisation
  (let [svg "<svg xmlns='http://www.w3.org/2000/svg' width='40' height='20'>
               <rect width='40' height='20' fill='#00ff00'/></svg>"]
    (with-open [im (img/render-svg svg)]
      (is (= [40 20] [(img/width im) (img/height im)]))
      (is (= 0x00ff00ff (img/get-pixel im 20 10))))
    (testing "explicit size overrides the document's"
      (with-open [im (img/render-svg svg {:width 80})]
        (is (= 80 (img/width im)))))
    (testing "decode/probe recognise SVG too"
      (is (= "svg" (:format (img/probe (.getBytes svg "UTF-8"))))))))

(deftest svg-canvas-browser-sizing
  (testing "a declared size is resvg's own answer and needs no repair"
    (let [c (img/svg-canvas "<svg xmlns='http://www.w3.org/2000/svg' width='40' height='20'>
                              <rect width='40' height='20' fill='#00ff00'/></svg>")]
      (is (= [40.0 20.0] [(:width c) (:height c)]))
      (is (= "declared" (:source c)))
      (is (false? (:is-repaired c)))
      (is (nil? (:svg c)))))
  (testing "a zero size falls back to the viewBox, as a browser does"
    (let [svg "<svg xmlns='http://www.w3.org/2000/svg' width='0' height='0' viewBox='0 0 30 10'>
                 <rect width='30' height='10' fill='#ff0000'/></svg>"
          c   (img/svg-canvas svg)]
      (is (= [30.0 10.0] [(:width c) (:height c)]))
      (is (= "view_box" (:source c)))
      (is (true? (:is-repaired c)))
      (is (bytes? (:svg c)))
      (testing "resvg alone refuses it; the repair is on by default"
        (is (thrown? clojure.lang.ExceptionInfo (img/render-svg svg {:repair false})))
        (with-open [im (img/render-svg (:svg c))]
          (is (= [30 10] [(img/width im) (img/height im)])))
        (with-open [im (img/render-svg svg)]
          (is (= [30 10] [(img/width im) (img/height im)])))
        (with-open [im (img/decode (.getBytes ^String svg "UTF-8"))]
          (is (= [30 10] [(img/width im) (img/height im)])
              "decode rasterises the repaired canvas too")))))
  (testing "a size-less document is framed by the ink it paints, not from the origin"
    (let [svg "<svg xmlns='http://www.w3.org/2000/svg'>
                 <rect x='50' y='30' width='100' height='40' fill='#0000ff'/></svg>"
          c   (img/svg-canvas svg)]
      (is (= "content" (:source c)))
      (is (true? (:is-repaired c)))
      (is (<= 512.0 (:width c) 4096.0))
      (is (< 2.4 (/ (:width c) (:height c)) 2.6) "keeps the ink's 100x40 aspect")
      (with-open [im (img/render-svg svg)]
        (is (= [(long (:width c)) (long (:height c))] [(img/width im) (img/height im)]))
        (testing "the ink fills the frame — no origin margin — but keeps a hairline of background"
          (is (= 0x0000ffff (img/get-pixel im 16 16)))
          (is (= 0x0000ffff (img/get-pixel im (- (img/width im) 17) (- (img/height im) 17))))
          (is (zero? (bit-and (long (img/get-pixel im 0 0)) 0xff)) "corner is background")
          (is (zero? (bit-and (long (img/get-pixel im (dec (img/width im)) (dec (img/height im)))) 0xff)))))))
  (testing "content at negative coordinates is found too"
    (let [c (img/svg-canvas "<svg xmlns='http://www.w3.org/2000/svg'>
                              <rect x='-300' y='-120' width='60' height='30' fill='#0000ff'/></svg>")]
      (is (= "content" (:source c)))
      (is (< 1.9 (/ (:width c) (:height c)) 2.1))))
  (testing "the ink band is configurable"
    (let [c (img/svg-canvas "<svg xmlns='http://www.w3.org/2000/svg'>
                              <rect x='50' y='30' width='100' height='40' fill='#0000ff'/></svg>"
                            {:min-dimension 64 :max-dimension 128})]
      (is (<= 64.0 (:width c) 128.0)))))

(deftest convert-one-shot
  (let [out (img/convert (png-bytes) {:format :jpeg :max-width 8 :quality 80})]
    (is (pos? (alength out)))
    (let [p (img/probe out)]
      (is (= "jpeg" (:format p)))
      (is (= 8 (:width p)))))
  (testing ":max-bytes retries until the payload fits"
    (with-open [big (img/blank 400 400 :red)]
      (let [out (img/convert (img/encode big :png) {:format :jpeg :max-bytes 2000})]
        (is (<= (alength out) 2000))))))

(deftest plotting
  (testing "a line figure renders at the requested size"
    (with-open [im (plot/render (plot/figure {:title "t" :width 320 :height 200}
                                             (plot/lines {:x-label "x" :y-label "y"}
                                                         {:name "a" :x [0 1 2 3] :y [1 4 2 8]}
                                                         {:name "b" :y [3 3 3 3]})))]
      (is (= [320 200] [(img/width im) (img/height im)]))))
  (testing "every panel type renders"
    (doseq [p [(plot/bars {:labels ["a" "b" "c"]} [3 1 2])
               (plot/hbars {:labels ["a" "b"]} [1 2])
               (plot/stacked-bars {:labels ["a" "b"]} [1 2] [2 1])
               (plot/areas [1 2 3 2])
               (plot/steps [1 2 3 2])
               (plot/scatter {:x [1 2 3] :y [3 2 1]})
               (plot/histogram {:bins 5} [1 1 2 3 5 8 13 21])
               (plot/pie {:labels ["a" "b" "c"] :values [1 2 3]})
               (plot/donut {:hole 0.5 :labels ["a" "b"] :values [1 1]})
               (plot/heatmap {:z [[1 2] [3 4]]})]]
      (with-open [im (plot/render (plot/figure {:width 200 :height 150} p))]
        (is (= 200 (img/width im)) (str "panel " (:type p))))))
  (testing "themes and multi-panel grids"
    (doseq [theme [:light :dark :night]]
      (with-open [im (plot/render (plot/figure {:theme theme :cols 2 :width 400 :height 200}
                                               (plot/lines [1 2 3])
                                               (plot/bars [1 2 3])))]
        (is (= 400 (img/width im))))))
  (testing "bytes + save! shortcuts"
    (let [bs (plot/bytes (plot/lines [1 2 3]))]
      (is (= "png" (:format (img/probe bs)))))
    (let [f (java.io.File/createTempFile "plot" ".png")]
      (.deleteOnExit f)
      (plot/save! f (plot/lines [1 2 3]))
      (is (pos? (.length f)))
      (is (= "png" (:format (img/probe (with-open [in (io/input-stream f)]
                                         (.readAllBytes in)))))))))

(deftest closing-is-idempotent-and-gc-safe
  ;; Regression: `close` flipped a flag on the handle while the Cleaner action
  ;; freed unconditionally, so a closed-then-collected image freed the same Rust
  ;; allocation twice and aborted the JVM (SIGABRT, no exception to catch).
  (let [im (img/blank 8 8 :red)]
    (img/close! im)
    (is (nil? (img/close! im)) "second close is a no-op")
    (is (nil? (img/close! im))))
  (testing "closed handles survive collection"
    (dotimes [_ 200]
      (with-open [im (img/blank 64 64 :blue)]
        (img/pixels im)))
    (System/gc)
    (Thread/sleep 100)
    (System/gc)
    (is (= 8 (img/width (img/blank 8 8 :red))) "the process is still alive")))

(defn- gradient-rgba
  "`w`x`h` straight RGBA8 rows of a smooth three-channel gradient — thousands of
   distinct colours, so a quantiser has to actually choose a palette."
  ^bytes [w h]
  (let [b (byte-array (* w h 4))]
    (dotimes [y h]
      (dotimes [x w]
        (let [i (* 4 (+ x (* y w)))]
          (aset b i (unchecked-byte (* 4 x)))
          (aset b (+ i 1) (unchecked-byte (* 5 y)))
          (aset b (+ i 2) (unchecked-byte (+ (* 2 x) y)))
          (aset b (+ i 3) (unchecked-byte 255)))))
    b))

(defn- mean-abs-error
  "Mean per-channel |difference| between two straight-RGBA8 rasters of equal size."
  [^bytes a ^bytes b]
  (let [n (alength a)]
    (loop [i 0 sum 0]
      (if (>= i n)
        (/ (double sum) (* 3 (quot n 4)))
        (recur (+ i 4)
               (+ sum
                  (Math/abs (- (bit-and 0xff (aget a i)) (bit-and 0xff (aget b i))))
                  (Math/abs (- (bit-and 0xff (aget a (+ i 1))) (bit-and 0xff (aget b (+ i 1)))))
                  (Math/abs (- (bit-and 0xff (aget a (+ i 2))) (bit-and 0xff (aget b (+ i 2)))))))))))

(defn- jdk-gif-quantized
  "The SAME raster quantised by an INDEPENDENT engine: the JDK's own GIF writer
   picks a <=256 colour palette, and reading the GIF back yields its answer as
   straight RGBA8 rows. javax.imageio is only ever an oracle here — the library
   itself stays AWT-free."
  ^bytes [^bytes rgba w h]
  (let [src (BufferedImage. w h BufferedImage/TYPE_INT_RGB)]
    (dotimes [y h]
      (dotimes [x w]
        (let [i (* 4 (+ x (* y w)))]
          (.setRGB src x y (int (bit-or (bit-shift-left (bit-and 0xff (aget rgba i)) 16)
                                        (bit-shift-left (bit-and 0xff (aget rgba (+ i 1))) 8)
                                        (bit-and 0xff (aget rgba (+ i 2)))))))))
    (let [out (ByteArrayOutputStream.)
          _ (is (true? (ImageIO/write src "gif" out)) "the JVM can write GIF")
          back (ImageIO/read (ByteArrayInputStream. (.toByteArray out)))
          res (byte-array (* w h 4))]
      (dotimes [y h]
        (dotimes [x w]
          (let [argb (.getRGB back x y)
                i (* 4 (+ x (* y w)))]
            (aset res i (unchecked-byte (bit-shift-right argb 16)))
            (aset res (+ i 1) (unchecked-byte (bit-shift-right argb 8)))
            (aset res (+ i 2) (unchecked-byte argb))
            (aset res (+ i 3) (unchecked-byte 255)))))
      res)))

(deftest quantize-cross-validated-against-the-jdk-gif-quantizer
  (let [w 64 h 48
        rgba (gradient-rgba w h)]
    (with-open [im (img/from-pixels rgba w h)]
      (let [q (img/quantize im {:colors 256})
            ours (mean-abs-error rgba (:rgba q))
            theirs (mean-abs-error rgba (jdk-gif-quantized rgba w h))]
        (testing "the contract"
          (is (= [w h] [(:width q) (:height q)]))
          (is (= 256 (count (:palette q))))
          (is (every? #(<= 0 % 0xffffff) (:palette q)))
          (is (= (* w h) (alength ^bytes (:indices q))))
          (is (= (* w h 4) (alength ^bytes (:rgba q))))
          (is (nil? (:transparent q)) "an opaque source needs no transparent slot"))
        (testing "an independent quantiser agrees, and we are no worse than it"
          (is (< ours 6.0) (str "mean abs error per channel: " ours))
          (is (<= ours (+ theirs 0.5))
              (str "ours " ours " vs javax.imageio GIF " theirs)))
        (testing "fewer colours costs more error, and dithering keeps it bounded"
          (let [few (mean-abs-error rgba (:rgba (img/quantize im {:colors 16})))
                dithered (mean-abs-error rgba (:rgba (img/quantize im {:colors 256 :dither true})))]
            (is (= 16 (count (:palette (img/quantize im {:colors 16})))))
            (is (> few ours))
            (is (< dithered 8.0) (str "dithered mean abs error: " dithered))))))
    (testing "a fully transparent pixel gets its own palette slot"
      (let [holed (aclone rgba)]
        (aset holed 3 (unchecked-byte 0))
        (with-open [im (img/from-pixels holed w h)]
          (let [q (img/quantize im {:colors 16})]
            (is (integer? (:transparent q)))
            (is (< (:transparent q) (count (:palette q))))
            (is (= (:transparent q) (bit-and 0xff (aget ^bytes (:indices q) 0))))))))))

(defn- ->buffered-image
  "Straight RGBA8 rows -> an opaque TYPE_INT_RGB BufferedImage."
  ^BufferedImage [^bytes rgba w h]
  (let [bi (BufferedImage. w h BufferedImage/TYPE_INT_RGB)]
    (dotimes [y h]
      (dotimes [x w]
        (let [i (* 4 (+ x (* y w)))]
          (.setRGB bi x y (int (bit-or (bit-shift-left (bit-and 0xff (aget rgba i)) 16)
                                       (bit-shift-left (bit-and 0xff (aget rgba (+ i 1))) 8)
                                       (bit-and 0xff (aget rgba (+ i 2)))))))))
    bi))

(deftest convolve-cross-validated-against-awt-convolveop
  (let [w 24 h 18
        rgba (gradient-rgba w h)
        ;; java.awt.image.ConvolveOp is an INDEPENDENT implementation of the same
        ;; operation. It has no scale/offset, so the division is folded into the
        ;; weights, and it does not clamp at the edges — hence the interior-only
        ;; comparison. The library itself stays AWT-free; this is only an oracle.
        awt (.filter (ConvolveOp. (Kernel. 3 3 (float-array (repeat 9 (/ 1.0 9.0))))
                                  ConvolveOp/EDGE_NO_OP nil)
                     (->buffered-image rgba w h)
                     nil)]
    (with-open [im (img/from-pixels rgba w h)
                blur (img/convolve im 3 (repeat 9 1.0) {:scale 9.0})
                same (img/convolve im 3 [0 0 0 0 1 0 0 0 0])]
      (is (= [w h] [(img/width blur) (img/height blur)]))
      (testing "an identity kernel is the identity"
        (is (= (seq (img/pixels im)) (seq (img/pixels same)))))
      (testing "a 3x3 box blur matches AWT's ConvolveOp inside the border"
        (let [worst (atom 0)]
          (doseq [y (range 1 (dec h))
                  x (range 1 (dec w))]
            (let [ours (img/get-pixel blur x y)
                  theirs (.getRGB awt x y)]
              (doseq [[os ts] [[24 16] [16 8] [8 0]]]
                (swap! worst max (Math/abs (- (bit-and (bit-shift-right ours os) 0xff)
                                              (bit-and (bit-shift-right theirs ts) 0xff)))))))
          (is (<= @worst 1) (str "worst per-channel disagreement with AWT: " @worst))))
      (testing "scale and offset are Pillow's, and alpha survives"
        (with-open [tr (img/blank 4 4 [10 20 30 0])
                    lit (img/convolve tr 3 (repeat 9 1.0) {:scale 9.0 :offset 100})]
          (is (= 0x6e788200 (img/get-pixel lit 1 1))
              "offset shifts each channel, alpha 0 is carried through")))
      (testing "a bad kernel is a truthful error, not a crash"
        (is (thrown? clojure.lang.ExceptionInfo (img/convolve im 2 (repeat 4 1.0))))
        (is (thrown? clojure.lang.ExceptionInfo (img/convolve im 3 (repeat 4 1.0))))))))

(deftest rank-filter-removes-noise-the-way-pillow-does
  (with-open [im (img/blank 9 9 :white)]
    (img/put-pixel! im 4 4 :black)
    (img/put-pixel! im 1 1 :black)
    (with-open [med (img/rank-filter im 3 :median)
                mn (img/rank-filter im 3 :min)
                mx (img/rank-filter im 3 :max)]
      (testing "the median erases isolated specks"
        (is (= 0xffffffff (img/get-pixel med 4 4)))
        (is (= 0xffffffff (img/get-pixel med 1 1))))
      (testing "min dilates the dark speck, max erases it"
        (is (= 0x000000ff (img/get-pixel mn 3 3)))
        (is (= 0x000000ff (img/get-pixel mn 5 5)))
        (is (= 0xffffffff (img/get-pixel mx 4 4))))
      (testing "an explicit rank index is the sorted neighbourhood"
        (with-open [r0 (img/rank-filter im 3 0)
                    r8 (img/rank-filter im 3 8)]
          (is (= (seq (img/pixels mn)) (seq (img/pixels r0))))
          (is (= (seq (img/pixels mx)) (seq (img/pixels r8))))))
      (is (thrown? clojure.lang.ExceptionInfo (img/rank-filter im 4 :median))))))

(deftest errors-are-ex-info
  (is (thrown? clojure.lang.ExceptionInfo (img/decode (byte-array [1 2 3 4]))))
  (is (thrown? clojure.lang.ExceptionInfo (img/render-svg "<svg")))
  (is (thrown? clojure.lang.ExceptionInfo (img/plot {:panels [{:type "nope"}]}))))

;; ---------------------------------------------------------------------------
;; Size optimisation — real optimisers, measured, not "re-encode and hope"
;; ---------------------------------------------------------------------------

(defn- rgba-of
  "The straight RGBA8 rows an encoded image decodes to."
  ^bytes [^bytes data]
  (with-open [im (img/decode data)] (img/pixels im)))

(defn- flat-rgba
  "`w`x`h` of two solid colours in wide horizontal bands — the kind of artwork
   PNG's filters and palette reduction are built for."
  ^bytes [w h]
  (let [b (byte-array (* w h 4))]
    (dotimes [y h]
      (dotimes [x w]
        (let [i  (* 4 (+ x (* y w)))
              on (even? (quot y 20))]
          (aset b i (unchecked-byte (if on 220 25)))
          (aset b (+ i 1) (unchecked-byte (if on 40 25)))
          (aset b (+ i 2) (unchecked-byte (if on 90 30)))
          (aset b (+ i 3) (unchecked-byte 255)))))
    b))

(defn- noisy-rgba
  "`w`x`h` of high-entropy pixels — a photograph's worst case, where a lossless
   codec has almost nothing to exploit and a quantiser has to work."
  ^bytes [w h]
  (let [b (byte-array (* w h 4))]
    (dotimes [y h]
      (dotimes [x w]
        (let [i (* 4 (+ x (* y w)))
              n (fn ^long [^long k]
                  (mod (+ (* 3 x) (* 5 y) (* 37 k) (bit-and (hash [x y k]) 31)) 256))]
          (aset b i (unchecked-byte (n 0)))
          (aset b (+ i 1) (unchecked-byte (n 1)))
          (aset b (+ i 2) (unchecked-byte (n 2)))
          (aset b (+ i 3) (unchecked-byte 255)))))
    b))

(defn- with-metadata
  "The same JPEG with a `n`-byte APP1 (EXIF/XMP) segment spliced in after SOI —
   what every phone camera actually hands you."
  ^bytes [^bytes jpeg n]
  (let [len (+ n 2)
        out (ByteArrayOutputStream.)]
    (.write out (int 0xFF))
    (.write out (int 0xD8))
    (.write out (int 0xFF))
    (.write out (int 0xE1))
    (.write out (int (bit-shift-right len 8)))
    (.write out (int (bit-and len 0xff)))
    (.write out (byte-array n (byte 0x41)))
    (.write out jpeg 2 (- (alength jpeg) 2))
    (.toByteArray out)))

(deftest optimize-png-is-lossless-and-smaller
  (let [flat  (with-open [im (img/from-pixels (flat-rgba 400 300) 400 300)]
                (img/encode im :png))
        photo (with-open [im (img/from-pixels (noisy-rgba 320 240) 320 240)]
                (img/encode im :png))]
    (testing "oxipng really shrinks the bytes"
      (let [o (img/optimize flat)]
        (is (< (alength o) (* 0.5 (alength flat)))
            (str "flat art: " (alength flat) " -> " (alength o)))
        (is (java.util.Arrays/equals (rgba-of flat) (rgba-of o))
            "and not one pixel moved"))
      (let [o (img/optimize photo)]
        (is (< (alength o) (alength photo)) (str (alength photo) " -> " (alength o)))
        (is (java.util.Arrays/equals (rgba-of photo) (rgba-of o)))))
    (testing "more effort is never bigger (level 6 adds zopfli)"
      (is (<= (alength (img/optimize flat {:level 6})) (alength (img/optimize flat {:level 3})))))
    (testing "lossy is the palette path, and pays for itself"
      (let [o (img/optimize photo {:lossy true :colors 64})]
        (is (< (alength o) (* 0.5 (alength (img/optimize photo)))))
        (is (not (java.util.Arrays/equals (rgba-of photo) (rgba-of o))))
        (is (< (mean-abs-error (rgba-of photo) (rgba-of o)) 12.0)
            "a 64-colour median cut still tracks the original")))
    (testing "a smooth gradient is what oxipng eats alive"
      (let [grad (with-open [im (img/from-pixels (gradient-rgba 320 240) 320 240)]
                   (img/encode im :png))
            o    (img/optimize grad)]
        (is (< (alength o) (* 0.5 (alength grad))))
        (is (java.util.Arrays/equals (rgba-of grad) (rgba-of o)))))))

(deftest optimize-jpeg-keeps-every-coefficient-until-asked
  (let [jpeg (with-open [im (img/from-pixels (gradient-rgba 320 240) 320 240)]
               (img/encode im :jpeg 98))
        fat  (with-metadata jpeg 20000)]
    (testing "metadata goes, the scan does not (jpegtran -copy none)"
      (let [o (img/optimize fat)]
        (is (< (alength o) (- (alength fat) 19000))
            (str "metadata stripped: " (alength fat) " -> " (alength o)))
        (is (java.util.Arrays/equals (rgba-of fat) (rgba-of o))
            "a lossless strip cannot change a pixel")))
    (testing "a JPEG with nothing to strip is handed back untouched"
      (is (java.util.Arrays/equals jpeg (img/optimize jpeg))))
    (testing ":strip false keeps the metadata"
      (is (= (alength fat) (alength (img/optimize fat {:strip false})))))
    (testing "an explicit quality (or :lossy) is what licenses a re-encode"
      (let [o (img/optimize jpeg {:quality 60})]
        (is (< (alength o) (* 0.5 (alength jpeg))))
        (is (not (java.util.Arrays/equals (rgba-of jpeg) (rgba-of o)))))
      (is (< (alength (img/optimize jpeg {:lossy true})) (alength jpeg))))))

(deftest optimize-gif-differences-frames-like-gifsicle
  (let [w 200 h 120
        frame (fn [i]
                (let [b (byte-array (* w h 4))]
                  (dotimes [y h]
                    (dotimes [x w]
                      (let [o  (* 4 (+ x (* y w)))
                            sq (and (<= (* i 20) x) (< x (+ (* i 20) 20)) (<= 40 y) (< y 60))]
                        (aset b o (unchecked-byte (if sq 255 12)))
                        (aset b (+ o 1) (unchecked-byte (if sq 200 90)))
                        (aset b (+ o 2) (unchecked-byte (if sq 0 160)))
                        (aset b (+ o 3) (unchecked-byte 255)))))
                  b))
        anim  (img/encode-gif {:width w :height h :loop-count -1
                               :frames (mapv (fn [i] {:delay-ms 80 :rgba (frame i)}) (range 6))})
        out   (img/optimize anim)
        back  (img/decode-gif out)]
    (is (< (alength out) (alength anim)) (str (alength anim) " -> " (alength out)))
    (is (= 6 (count (:frames back))) "an animation is never flattened to one frame")
    (is (every? #(= 80 (:delay-ms %)) (:frames back)) "and keeps its timing")))

(deftest optimize-honours-max-bytes-and-max-dimensions
  (let [png (with-open [im (img/from-pixels (noisy-rgba 320 240) 320 240)]
              (img/encode im :png))]
    (testing ":max-bytes walks the knobs down until it fits"
      (let [o (img/optimize png {:max-bytes 20000})]
        (is (<= (alength o) 20000) (str "got " (alength o)))
        (is (< 1000 (alength o)) "and stops as soon as it fits")))
    (testing ":max-width / :max-height bound the pixels, keeping the aspect"
      (with-open [im (img/decode (img/optimize png {:max-width 100}))]
        (is (= [100 75] [(img/width im) (img/height im)])))
      (with-open [im (img/decode (img/optimize png {:max-width 160 :max-height 60}))]
        (is (= [80 60] [(img/width im) (img/height im)])))
      (with-open [im (img/decode (img/optimize png {:max-width 9999}))]
        (is (= [320 240] [(img/width im) (img/height im)]) "never enlarges")))
    (testing "a format change is honoured even when the input was smaller"
      (with-open [im (img/decode (img/optimize png {:format :jpeg :quality 75}))]
        (is (= [320 240] [(img/width im) (img/height im)])))
      (is (= "jpeg" (:format (img/probe (img/optimize png {:format :jpeg :quality 75}))))))))

;; ---------------------------------------------------------------------------
;; Video (MP4 / H.264)
;;
;; `test/fixtures/bars.mp4` is a real H.264 clip: 64x48, 10 frames at 8 fps,
;; each frame an 8px-wide white bar in block `index mod 8` on black. That makes
;; the bar's position a CONTENT check and an ORDER check at once — a decoder
;; that reuses one buffer, drops B-frames or emits coded order fails it.
;; ---------------------------------------------------------------------------

(defn- clip
  ^java.io.File [] (io/file "test/fixtures/bars.mp4"))

(defn- brightest-block
  "Index of the 8px column block holding the white bar."
  [^bytes rgba w h]
  (apply max-key
         (fn [b]
           (reduce + (for [y (range h)
                           x (range (* b 8) (* (inc b) 8))]
                       (bit-and (aget rgba (* 4 (+ (* y w) x))) 0xff))))
         (range (quot w 8))))

(deftest mp4-sniffing
  (is (img/mp4? (clip)))
  (is (not (img/mp4? (png-bytes))))
  (is (not (img/mp4? (byte-array 3)))))

(deftest probe-video-without-decoding
  (let [p (img/probe-video (clip))]
    (is (= "mp4" (:format p)))
    (is (= "h264" (:codec-kind p)))
    (is (str/starts-with? (:codec p) "avc1."))
    (is (true? (:is-decodable p)))
    (is (= [64 48] [(:width p) (:height p)]))
    (is (= 10 (:frames p)))
    (is (false? (:has-audio p)))
    (is (== 8.0 (:fps p)))
    (is (== 1.25 (:duration-s p)))))

(deftest decode-video-frames-are-rgba-in-display-order
  (let [d (img/decode-video (clip))
        {:keys [width height frames frame-count]} d]
    (is (= "mp4" (:format d)))
    (is (= "rgba" (:encoding d)))
    (is (= [64 48] [width height]))
    (is (= [64 48] [(:source-width d) (:source-height d)]))
    (is (= 10 frame-count (:total-frames d) (count frames)))
    (testing "straight RGBA8 rows, one buffer per frame — not N views of the last"
      (is (every? #(= (* width height 4) (alength ^bytes (:data %))) frames))
      (testing "no two neighbours share bytes — the decoder does not hand back
               N views of one reused Picture buffer (the bar repeats every 8
               frames, so only 8 of the 10 payloads are globally distinct)"
        (is (every? (fn [[a b]] (not= (seq (:data a)) (seq (:data b))))
                    (partition 2 1 frames)))
        (is (= 8 (count (distinct (map #(seq (:data %)) frames)))))))
    (testing "timestamps and the moving bar both prove display order"
      (is (= (range 10) (map :index frames)))
      (is (= (map #(/ % 8.0) (range 10)) (map :timestamp-s frames)))
      (is (= [0 1 2 3 4 5 6 7 0 1]
             (map #(brightest-block (:data %) width height) frames))))
    (testing "a frame's rows load straight back into an Img"
      (with-open [im (img/from-pixels (:data (first frames)) width height)]
        (is (= {:width 64 :height 48 :is-opaque true :is-grayscale true}
               (img/info im)))))))

;; `test/fixtures/refcycle27.mp4` is lossless (`-qp 0`) H.264, 27 frames of three
;; distinct noise images cycled A,B,C,A,B,C…, encoded `-refs 3 -bf 0 weightp=0`
;; in one GOP. Noise is expensive to intra-code, so the encoder predicts from the
;; matching picture three frames back — through `ref_idx_l0 > 0`. Lossless plus
;; cyclic means frame `i` must return byte-identical to frame `i mod 3`, an
;; invariant that needs no reference decoder to check. Past picture 16 `frame_num`
;; wraps, and ordering the reference list by the bitstream's picture order count
;; (which wraps with it when `pic_order_cnt_type` is 2) puts the OLDEST reference
;; at index 0 — every later frame then predicts from the wrong picture and drifts.
;; ---------------------------------------------------------------------------

(defn- refcycle
  ^java.io.File [] (io/file "test/fixtures/refcycle27.mp4"))

(deftest decode-video-picks-the-right-reference-past-the-frame-num-wrap
  (let [{:keys [frames frame-count]} (img/decode-video (refcycle))
        payloads (mapv #(seq (:data %)) frames)]
    (is (= 27 frame-count (count frames)))
    (testing "the three cycle members really differ, or the check is vacuous"
      (is (= 3 (count (distinct payloads)))))
    (testing "every frame is byte-identical to its cycle member"
      (is (= (mapv #(payloads (mod % 3)) (range 27)) payloads)))))

(deftest decode-video-bounds-the-work
  (let [d (img/decode-video (clip) {:max-frames 3 :stride 2 :max-dimension 32})]
    (is (= 3 (:frame-count d)))
    (is (= 10 (:total-frames d)))
    (is (= [0 2 4] (map :index (:frames d))))
    (testing "scaled down, aspect kept, and the payload follows the new size"
      (is (= [32 24] [(:width d) (:height d)]))
      (is (= [64 48] [(:source-width d) (:source-height d)]))
      (is (every? #(= (* 32 24 4) (alength ^bytes (:data %))) (:frames d))))))

(deftest decode-video-can-emit-encoded-stills
  (let [d (img/decode-video (clip) {:encoding "png" :max-frames 2})]
    (is (= "png" (:encoding d)))
    (is (= 2 (:frame-count d)))
    (doseq [f (:frames d)]
      (is (= {:format "png" :width 64 :height 48}
             (select-keys (img/probe (:data f)) [:format :width :height]))))))

(deftest video-to-gif-is-watchable-anywhere
  (let [gif (img/video->gif (clip))]
    (is (= "GIF89a" (String. (byte-array (take 6 gif)) "US-ASCII")))
    (is (= "gif" (:format (img/probe gif))))
    (is (true? (:is-animated (img/probe gif))))
    (let [back (img/decode-gif gif)]
      (is (= [64 48] [(:width back) (:height back)]))
      (is (= 10 (count (:frames back))))
      (is (= -1 (:loop-count back)))
      (testing "8 fps is 125 ms, quantised to GIF's centisecond delay field"
        (is (= 120 (:delay-ms (first (:frames back))))))
      (testing "the bar still marches — the transcode kept display order"
        (is (= [0 1 2 3 4 5 6 7 0 1]
               (map #(brightest-block (:rgba %) 64 48) (:frames back)))))))
  (testing "options thin and retime the animation"
    (let [back (img/decode-gif (img/video->gif (clip) {:stride 2 :max-dimension 32
                                                       :fps 10 :loop-count 3}))]
      (is (= [32 24] [(:width back) (:height back)]))
      (is (= 5 (count (:frames back))))
      (is (= 3 (:loop-count back)))
      (is (= 100 (:delay-ms (first (:frames back))))))))

(deftest video-errors-are-ex-info
  (testing "a still image is not a container"
    (is (thrown? clojure.lang.ExceptionInfo (img/probe-video (png-bytes))))
    (is (thrown? clojure.lang.ExceptionInfo (img/decode-video (png-bytes))))
    (is (thrown? clojure.lang.ExceptionInfo (img/video->gif (png-bytes)))))
  (testing "the failure names the operation and carries the native message"
    (let [e (try (img/probe-video (png-bytes)) (catch clojure.lang.ExceptionInfo e e))]
      (is (= :probe-video (:op (ex-data e))))
      (is (string? (ex-message e)))
      (is (not (str/blank? (ex-message e)))))))

(deftest office-build-and-read-round-trips
  (testing "xlsx values, formulae and sheet metadata"
    (let [data (img/xlsx {:properties {:title "Quarterly"}
                          :sheets [{:name "Summary"
                                    :freeze {:row 1 :col 1}
                                    :cells [{:row 0 :col 0 :value "Quarter"}
                                            {:row 1 :col 0 :value "Q1"}
                                            {:row 1 :col 1 :value 42}
                                            {:row 2 :col 1 :formula "SUM(B2:B2)"}]}]})
          out (img/read-office data)]
      (is (= "workbook" (:kind out)))
      (is (= "Summary" (get-in out [:sheets 0 :name])))
      (is (= 42.0 (get-in out [:sheets 0 :rows 1 1])))))
  (testing "docx paragraphs and tables"
    (let [data (img/docx {:properties {:title "Memo"}
                          :blocks [{:type :heading :level 1 :text "Status"}
                                   {:type :paragraph :text "All green"}
                                   {:type :table :rows [["A" "B"] ["1" "2"]]}]})
          out (img/read-office data)]
      (is (= "document" (:kind out)))
      (is (some #{"Status"} (:paragraphs out)))
      (is (some #{"All green"} (:paragraphs out)))))
  (testing "pptx rich shapes, notes and charts survive native read/write"
    (let [spec {:width 12192000
                :height 6858000
                :properties {:title "Native deck" :author "Vis"}
                :slides [{:layout "Blank"
                          :notes "Speaker notes"
                          :shapes [{:kind :textbox :left 100000 :top 100000
                                    :width 2000000 :height 500000
                                    :text-frame {:paragraphs [{:runs [{:text "Rust PPTX"
                                                                       :bold true
                                                                       :color "3366FF"}]}]}}
                                   {:kind :table :left 100000 :top 800000
                                    :width 3000000 :height 1000000
                                    :table {:col-widths [1500000 1500000]
                                            :rows [{:cells [{:text "A"} {:text "B"}]}
                                                   {:cells [{:text "1"} {:text "2"}]}]}}
                                   {:kind :chart :left 3400000 :top 800000
                                    :width 4500000 :height 3000000
                                    :chart {:type :column :title "Revenue"
                                            :categories ["Q1" "Q2"]
                                            :legend {:position :bottom}
                                            :series [{:name "Sales" :values [3 7]
                                                      :fill "4472C4"}]}}]}]}
          bytes (img/pptx spec)
          out (img/read-office bytes {:with-images true})
          shapes (get-in out [:slides 0 :shapes])
          chart (:chart (first (filter #(= "chart" (:kind %)) shapes)))]
      (is (> (alength ^bytes bytes) 1000))
      (is (= "presentation" (:kind out)))
      (is (= "Native deck" (get-in out [:properties :title])))
      (is (= "Speaker notes" (get-in out [:slides 0 :notes])))
      (is (= #{"textbox" "table" "chart"} (set (map :kind shapes))))
      (is (= ["Q1" "Q2"] (:categories chart)))
      (is (= [3.0 7.0] (get-in chart [:series 0 :values])))
      (is (= "Revenue" (:title chart))))))

(deftest pptx-fill-and-line-survive-the-read-back
  (testing "spPr fill/outline come back as the very spec `pptx` was given"
    (let [spec {:slides [{:layout "Blank"
                          :shapes [{:kind :auto :preset "rect"
                                    :left 100000 :top 100000
                                    :width 2000000 :height 500000
                                    :fill {:type :solid :color "112233"}
                                    :line {:color "0000FF" :width 25400 :dash "dash"}}
                                   {:kind :auto :preset "ellipse"
                                    :left 100000 :top 900000
                                    :width 2000000 :height 500000
                                    :fill {:type :gradient :angle 90
                                           :stops [{:position 0 :color "FF0000"}
                                                   {:position 1 :color "0000FF"}]}}
                                   {:kind :auto :preset "rect"
                                    :left 100000 :top 1700000
                                    :width 2000000 :height 500000
                                    :fill {:type :none}
                                    :line {:type :none}}]}]}
          shapes (get-in (img/read-office (img/pptx spec)) [:slides 0 :shapes])
          [solid grad bare] shapes]
      (is (= {:type "solid" :color "112233"} (:fill solid)))
      (is (= {:color "0000FF" :width 25400 :dash "dash"} (:line solid)))
      (is (= "gradient" (get-in grad [:fill :type])))
      (is (= 90.0 (get-in grad [:fill :angle])))
      (is (= ["FF0000" "0000FF"] (mapv :color (get-in grad [:fill :stops]))))
      (is (= [0.0 1.0] (mapv :position (get-in grad [:fill :stops]))))
      (is (= {:type "none"} (:fill bare)))
      (is (= {:type "none"} (:line bare))))))

(deftest any-document-reads-as-markdown
  (testing "a Word document keeps its heading, prose and table"
    (let [data (img/docx {:properties {:title "Memo"}
                          :blocks [{:type :heading :level 1 :text "Status"}
                                   {:type :paragraph :text "All green"}
                                   {:type :table :header true
                                    :rows [{:cells [{:text "A"} {:text "B"}]}
                                           {:cells [{:text "1"} {:text "2"}]}]}]})
          md   (img/document->markdown data)]
      (is (str/includes? md "Status"))
      (is (str/includes? md "All green"))
      (is (str/includes? md "| 1 | 2 |") "the table survives as a GFM table, not as loose text")))
  (testing "a workbook and a presentation read the same way"
    (is (str/includes? (img/document->markdown
                        (img/xlsx {:sheets [{:name "Summary"
                                             :cells [{:row 0 :col 0 :value "Quarter"}
                                                     {:row 1 :col 0 :value "Q1"}
                                                     {:row 1 :col 1 :value 42}]}]}))
                       "| Q1 | 42 |"))
    (is (str/includes? (img/document->markdown
                        (img/pptx {:slides [{:shapes [{:kind :textbox :left 0 :top 0
                                                       :width 2000000 :height 500000
                                                       :text-frame {:paragraphs [{:runs [{:text "Rust PPTX"}]}]}}]}]}))
                       "Rust PPTX")))
  (testing "CSV carries no signature, so only a name (or an explicit format) reaches it"
    (let [csv (.getBytes "a,b\n1,2\n")]
      (is (nil? (:format (img/document-format csv))))
      (is (= :csv (:format (img/document-format csv {:name "quarter.csv"}))))
      (is (str/includes? (img/document->markdown csv {:name "quarter.csv"}) "| 1 | 2 |"))
      (is (str/includes? (img/document->markdown csv {:format :csv}) "| 1 | 2 |"))))
  (testing "the container's own signature identifies it, and says so"
    (let [out (img/document-format (img/docx {:blocks [{:type :paragraph :text "hi"}]}))]
      (is (= :docx (:format out)))
      (is (= "content" (:source out)))
      (is (some #{"pdf" "epub" "excel"} (:formats out)))))
  (testing "embedded binaries come back whole when asked for, and never otherwise"
    (let [png  (png-bytes)
          deck (img/pptx {:slides [{:shapes [{:kind :picture :left 0 :top 0
                                              :width 1000000 :height 750000
                                              :image {:data (.encodeToString (java.util.Base64/getEncoder) png)}}]}]})
          out  (img/read-document deck {:assets true})
          a    (first (:assets out))]
      (is (nil? (:assets (img/read-document deck))))
      (is (= 1 (count (:assets out))))
      (is (= "image/png" (:media-type a)))
      (is (= (alength png) (:size a)))
      (is (java.util.Arrays/equals png ^bytes (:bytes a))
          "the asset is the PNG that went in, not a re-encoding of it")))
  (testing "an unidentifiable document is an ex-info, not a silent empty string"
    (is (thrown? clojure.lang.ExceptionInfo (img/document->markdown (byte-array 32))))
    (is (thrown? clojure.lang.ExceptionInfo
                 (img/document->markdown (byte-array 32) {:format :quattro-pro})))))

(defn- pdf-bytes
  "The smallest real PDF that carries text: one uncompressed content stream per
   page, Helvetica, a correct xref. Handwritten because nothing in this library
   WRITES PDFs and page provenance can only be proved against a document that
   actually has pages."
  ^bytes [pages]
  (let [n (count pages)
        page-ids (mapv #(+ 3 (* 2 %)) (range n))
        cont-ids (mapv #(+ 4 (* 2 %)) (range n))
        font-id (+ 3 (* 2 n))
        stream (fn [lines]
                 (str "BT /F1 12 Tf 72 720 Td 16 TL\n"
                      (str/join "\n" (for [l lines]
                                       (str "(" (str/escape l {\\ "\\\\" \( "\\(" \) "\\)"}) ") Tj T*")))
                      "\nET"))
        objs (into (sorted-map)
                   (concat
                    [[1 "<< /Type /Catalog /Pages 2 0 R >>"]
                     [2 (str "<< /Type /Pages /Kids ["
                             (str/join " " (map #(str % " 0 R") page-ids))
                             "] /Count " n " >>")]
                     [font-id "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"]]
                    (for [i (range n)]
                      [(page-ids i)
                       (str "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents "
                            (cont-ids i) " 0 R /Resources << /Font << /F1 " font-id " 0 R >> >> >>")])
                    (for [i (range n)
                          :let [s (stream (nth pages i))]]
                      [(cont-ids i)
                       (str "<< /Length " (count s) " >>\nstream\n" s "\nendstream")])))
        out (java.io.ByteArrayOutputStream.)
        put! (fn [^String s] (.write out (.getBytes s "ISO-8859-1")))
        _ (put! "%PDF-1.4\n")
        offsets (reduce (fn [acc [num body]]
                          (let [at (.size out)]
                            (put! (str num " 0 obj\n" body "\nendobj\n"))
                            (assoc acc num at)))
                        {}
                        objs)
        xref-at (.size out)
        size (inc (apply max (keys objs)))]
    (put! (str "xref\n0 " size "\n0000000000 65535 f \n"))
    (doseq [num (range 1 size)]
      (put! (if-let [at (offsets num)]
              (format "%010d 00000 n \n" at)
              "0000000000 65535 f \n")))
    (put! (str "trailer\n<< /Size " size " /Root 1 0 R >>\nstartxref\n" xref-at "\n%%EOF\n"))
    (.toByteArray out)))

(deftest a-document-reports-its-own-structure
  (testing "`:blocks` turns a rendering back into a document: plain text, spans, sections"
    (let [doc (img/docx
               {:blocks [{:type :heading :level 1 :text "Quarterly Report"}
                         {:type :paragraph :text "Revenue in March 2024 rose."}
                         {:type :heading :level 2 :text "Regions"}
                         {:type :paragraph
                          :runs [{:text "The "} {:text "mar" :italic true} {:text "ket closed early."}]}
                         {:type :table :header true
                          :rows [{:cells [{:text "Month"} {:text "Revenue"}]}
                                 {:cells [{:text "March"} {:text "12.4"}]}]}]})
          {:keys [markdown text blocks pages]} (img/read-document doc {:blocks true})
          by-kind (group-by :kind blocks)]
      (is (nil? pages) "only a PDF has pages")
      (is (str/includes? markdown "# **Quarterly Report**") "the Markdown still carries its emphasis")
      (is (not (str/includes? text "**")) "the plain text carries none of it")
      (is (str/includes? text "The market closed early.")
          "a style boundary INSIDE a word does not split the word in the plain text")

      (testing "every span points back at the text it came from"
        (doseq [{:keys [text' start end line] :as b} (map #(assoc % :text' (:text %)) blocks)]
          (is (= text' (subs text start end)) (str "block " (:index b) " span"))
          (is (= line (inc (count (filter #{\newline} (subs text 0 start)))))
              (str "block " (:index b) " line"))))

      (testing "headings carry their level, everything below carries its section"
        (is (= [1 2] (mapv :level (by-kind "heading"))))
        (is (= [] (:path (first blocks))) "a top heading sits under nothing")
        (is (= ["Quarterly Report" "Regions"]
               (:path (first (filter #(= "The market closed early." (:text %)) blocks))))
            "a paragraph knows the breadcrumb of headings above it"))

      (testing "a table row is one block, and its cells are addressable"
        (let [rows (by-kind "table-row")
              body (first (remove :is-header rows))]
          (is (= 2 (count rows)))
          (is (true? (:is-header (first rows))))
          (is (= "March | 12.4" (:text body)) "the whole row is one searchable passage")
          (is (= [{:column 0 :name "Month" :text "March"} {:column 1 :name "Revenue" :text "12.4"}]
                 (mapv #(select-keys % [:column :name :text]) (:cells body)))
              "each cell names the header column it sits under")
          (is (= "12.4" (subs text (:start (second (:cells body))) (:end (second (:cells body)))))
              "a cell's span is absolute, into the document's own text")))))

  (testing "a PDF reports the PAGE every block sits on"
    (let [pdf (pdf-bytes [["Quarterly Report" "Revenue in March 2024 rose sharply."]
                          ["Regional Detail" "April was quieter than March."]])
          {:keys [format pages blocks text]} (img/read-document pdf {:blocks true})]
      (is (= :pdf format))
      (is (= 2 pages))
      (is (= [1 2] (distinct (map :page blocks))) "blocks are stamped with their page")
      (is (str/includes? (:text (first (filter #(= 2 (:page %)) blocks))) "April")
          "page 2's text is on page 2")
      (doseq [{:keys [start end] :as b} blocks]
        (is (= (:text b) (subs text start end)) "a PDF block's span points back too"))))

  (testing "no `:blocks` option, no structure — the old shape is untouched"
    (let [r (img/read-document (.getBytes "a,b\n1,2\n" "UTF-8") {:format :csv})]
      (is (contains? r :markdown))
      (is (not (contains? r :blocks)))
      (is (not (contains? r :text))))))
