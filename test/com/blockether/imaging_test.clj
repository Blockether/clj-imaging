(ns com.blockether.imaging-test
  "Tests run against the REAL imaging-c cdylib (build it first:
   `scripts/build-natives.sh <platform>`), so they also prove the FFM binding,
   the JSON contracts and the memory ownership rules — not just the Clojure."
  (:require [clojure.java.io :as io]
            [clojure.string :as str]
            [clojure.test :refer [deftest is testing]]
            [com.blockether.imaging :as img]
            [com.blockether.imaging.plot :as plot]))

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
            (is (= 20 (img/width back)))
            (is (= 10 (img/height back)))))))))

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

(deftest errors-are-ex-info
  (is (thrown? clojure.lang.ExceptionInfo (img/decode (byte-array [1 2 3 4]))))
  (is (thrown? clojure.lang.ExceptionInfo (img/render-svg "<svg")))
  (is (thrown? clojure.lang.ExceptionInfo (img/plot {:panels [{:type "nope"}]}))))
