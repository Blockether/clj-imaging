(ns com.blockether.imaging
  "Clojure imaging WITHOUT `java.desktop` — decoding, encoding, transforms,
   vector drawing, text, SVG rasterisation and plotting, all in-process through
   the JDK Foreign Function & Memory API over a first-party Rust cdylib
   (`native/imaging-c`: the `image` crate + `resvg`/`usvg`/`tiny-skia`).

   Why it exists: `javax.imageio`, `java.awt.image.BufferedImage` and
   `Graphics2D` drag the whole AWT/Java2D/ImageIO subgraph — service loaders,
   `sun.java2d.loops` JNI blit primitives, the font manager — into a GraalVM
   native image, which is both huge and, on a headless server, pointless. This
   library replaces that surface with a ~5 MB cdylib and zero JNI: pure
   downcalls, no subprocess, no `-Djava.awt.headless`, no fontconfig.

   Everything is bytes in, bytes out. An `Img` is an opaque handle on the Rust
   side; it is `AutoCloseable` (use `with-open` for tight loops) AND registered
   with a `java.lang.ref.Cleaner`, so forgetting to close leaks nothing
   permanently.

   Run the JVM with `--enable-native-access=ALL-UNNAMED` so the foreign linker
   may load the library without a restricted-method warning.

   The library is resolved ONCE, lazily, the first time it's needed:
     1. IMAGING_NATIVE_PATH env / `com.blockether.imaging.native.path` system
        property — an explicit path to the cdylib (used verbatim).
     2. A bundled classpath resource `prebuilds/<platform>/<lib>`, shipped by
        `com.blockether/imaging-native-<platform>`. This is the native-image path.
     3. A runtime download: the `imaging-native-<platform>` jar resolved through
        `clojure.tools.deps` (honouring Maven repos/mirrors/settings.xml),
        extracted + cached. Disable with IMAGING_DISABLE_DOWNLOAD=1.

   `<platform>` in { linux-x64 linux-arm64 darwin-arm64 darwin-x64 windows-x64 }."
  (:refer-clojure :exclude [flatten])
  (:require [clojure.data.json :as json]
            [clojure.java.io :as io]
            [clojure.string :as str]
            [clojure.walk :as walk]
            [com.blockether.nativeresolver :as nr]
            [com.blockether.nativeresolver.ffm :as ffm])
  (:import [java.io ByteArrayOutputStream File InputStream]
           [java.lang.foreign Arena MemorySegment ValueLayout]
           [java.lang.ref Cleaner]
           [java.net URL]
           [java.nio.file Files Path]
           [java.util.concurrent.atomic AtomicBoolean]))

(set! *warn-on-reflection* true)

;; ---------------------------------------------------------------------------
;; Native library + FFM binding (com.blockether/nativeresolver)
;; ---------------------------------------------------------------------------

(def ^:private native-spec
  "How libimaging_c is found. Everything derives mechanically from `:lib`:
   IMAGING_NATIVE_PATH / `com.blockether.imaging.native.path` (verbatim), the
   bundled `prebuilds/<platform>/<lib>` resource shipped by
   `com.blockether/imaging-native-<platform>`, then the tools.deps download
   cached under ~/.cache/clj-imaging (IMAGING_DISABLE_DOWNLOAD=1 to disable).
   The version comes from the NAMESPACED `imaging/VERSION` resource."
  (nr/spec {:lib "imaging"}))

(defonce ^:private handles
  (delay
    (nr/bind! native-spec
              {:version       [:ptr "imaging_version"]
               :last-error    [:ptr "imaging_last_error"]
               :free-string   [nil  "imaging_free_string" :ptr]
               :buf-ptr       [:ptr "imaging_buf_ptr" :ptr]
               :buf-len       [:i64 "imaging_buf_len" :ptr]
               :buf-free      [nil  "imaging_buf_free" :ptr]
               :image-free    [nil  "imaging_image_free" :ptr]
               :probe         [:ptr "imaging_probe" :ptr :i64]
               :decode        [:ptr "imaging_decode" :ptr :i64]
               :new           [:ptr "imaging_new" :i32 :i32 :i32]
               :from-pixels   [:ptr "imaging_from_pixels" :ptr :i64 :i32 :i32]
               :pixels        [:ptr "imaging_pixels" :ptr]
               :info          [:ptr "imaging_info" :ptr]
               :encode        [:ptr "imaging_encode" :ptr :ptr :i32]
               :decode-gif    [:ptr "imaging_decode_gif" :ptr :i64]
               :encode-gif    [:ptr "imaging_encode_gif" :ptr]
               :probe-video   [:ptr "imaging_probe_video" :ptr :i64]
               :decode-video  [:ptr "imaging_decode_video" :ptr :i64 :ptr]
               :video->gif    [:ptr "imaging_video_to_gif" :ptr :i64 :ptr]
               :convert       [:ptr "imaging_convert" :ptr :i64 :ptr]
               :resize        [:ptr "imaging_resize" :ptr :i32 :i32 :ptr]
               :thumbnail     [:ptr "imaging_thumbnail" :ptr :i32 :i32]
               :crop          [:ptr "imaging_crop" :ptr :i64 :i64 :i32 :i32]
               :rotate        [:ptr "imaging_rotate" :ptr :f64 :i32 :i32]
               :flip          [:ptr "imaging_flip" :ptr :ptr]
               :grayscale     [:ptr "imaging_grayscale" :ptr]
               :invert        [:ptr "imaging_invert" :ptr]
               :blur          [:ptr "imaging_blur" :ptr :f32]
               :flatten       [:ptr "imaging_flatten" :ptr :i32]
               :adjust        [:ptr "imaging_adjust" :ptr :ptr]
               :paste         [:i32 "imaging_paste" :ptr :ptr :i64 :i64 :i32]
               :get-pixel     [:i64 "imaging_get_pixel" :ptr :i32 :i32]
               :put-pixel     [:i32 "imaging_put_pixel" :ptr :i32 :i32 :i32]
               :draw          [:i32 "imaging_draw" :ptr :ptr]
               :text-measure  [:ptr "imaging_text_measure" :ptr]
               :svg-render    [:ptr "imaging_svg_render" :ptr :i64 :ptr]
               :svg-canvas    [:ptr "imaging_svg_canvas" :ptr :i64 :ptr]
               :plot          [:ptr "imaging_plot" :ptr]
               :register-font [:ptr "imaging_register_font" :ptr :i64]
               :fonts         [:ptr "imaging_fonts"]
               :optimize      [:ptr "imaging_optimize" :ptr :i64 :ptr]
               :quantize      [:ptr "imaging_quantize" :ptr :i32 :i32]
               :filter        [:ptr "imaging_filter" :ptr :ptr]
               :office-build  [:ptr "imaging_office_build" :ptr :ptr]
               :office-read   [:ptr "imaging_office_read" :ptr :i64 :ptr]
               :doc-markdown  [:ptr "imaging_doc_markdown" :ptr :i64 :ptr]
               :doc-detect    [:ptr "imaging_doc_detect" :ptr :i64 :ptr]})))

(declare img? seg)

(defn- invoke
  "Call the cdylib. An `Img` argument is resolved to its native pointer and then
   held REACHABLE across the call: without the fence the handle's last Clojure
   reference can die at the `seg` read, the `Cleaner` can run on another thread
   and free the Rust allocation while the native call is still reading it. The
   handle stays lock-free (a single CAS decides who frees); the fence only stops
   the collector, it never blocks."
  [k & args]
  (let [args' (mapv #(if (img? %) (seg %) %) args)]
    (try
      (apply nr/invoke @handles k args')
      (finally
        (run! #(when (img? %) (java.lang.ref.Reference/reachabilityFence %)) args)))))

(defn- keep-live!
  "Fence handles whose pointers travelled into a call some other way (as an
   address inside a JSON payload, say) rather than as an argument."
  [xs] (run! #(java.lang.ref.Reference/reachabilityFence %) xs) nil)

(defn- null? [^MemorySegment p] (ffm/null? p))

(defn- take-string!
  "Read an owned `char*` result and free it. nil when the call returned NULL."
  [^MemorySegment p]
  (ffm/take-string! #(invoke :free-string %) p))

(defn- fail! [op]
  (throw (ex-info (str "imaging/" (name op) " failed: "
                       (or (take-string! (invoke :last-error)) "unknown error"))
                  {:op op})))

(defn- take-bytes!
  "Read an owned `Buf*` result into a byte[] and free it."
  ^bytes [^MemorySegment p op]
  (when (null? p) (fail! op))
  (let [len  (long (invoke :buf-len p))
        data ^MemorySegment (invoke :buf-ptr p)
        out  (.toArray (.reinterpret ^MemorySegment data len) ValueLayout/JAVA_BYTE)]
    (invoke :buf-free p)
    out))

;; ---------------------------------------------------------------------------
;; JSON + colours
;; ---------------------------------------------------------------------------

(defn- wire-key
  "Clojure kebab-case keyword -> the cdylib's snake_case JSON key."
  [k]
  (if (keyword? k) (str/replace (name k) \- \_) (str k)))

(defn- ->json ^String [x]
  (json/write-str x :key-fn wire-key :escape-slash false :escape-unicode false))

(defn- json-> [^String s]
  (when s (json/read-str s :key-fn (fn [k] (keyword (str/replace k \_ \-))))))

(defn- b64-encode
  ^String [^bytes b]
  (.encodeToString (java.util.Base64/getEncoder) b))

(defn- b64-decode
  ^bytes [^String s]
  (.decode (java.util.Base64/getDecoder) s))

(def ^:private named-colors
  {"transparent" 0x00000000 "black" 0x000000ff "white" 0xffffffff "red" 0xff0000ff
   "green" 0x008000ff "lime" 0x00ff00ff "blue" 0x0000ffff "yellow" 0xffff00ff
   "cyan" 0x00ffffff "aqua" 0x00ffffff "magenta" 0xff00ffff "fuchsia" 0xff00ffff
   "gray" 0x808080ff "grey" 0x808080ff "silver" 0xc0c0c0ff "maroon" 0x800000ff
   "olive" 0x808000ff "navy" 0x000080ff "purple" 0x800080ff "teal" 0x008080ff
   "orange" 0xffa500ff})

(defn color
  "A colour as the packed `0xRRGGBBAA` long the cdylib takes. Accepts a packed
   integer, `\"#rgb\"` / `\"#rrggbb\"` / `\"#rrggbbaa\"`, a basic CSS colour name
   (string or keyword), or `[r g b]` / `[r g b a]` with 0-255 channels (alpha may
   also be a 0.0-1.0 double)."
  ^long [c]
  (cond
    (nil? c) 0x00000000
    (integer? c) (bit-and (long c) 0xffffffff)
    (sequential? c)
    (let [[r g b a] c
          a (cond (nil? a) 255
                  (and (float? a) (<= 0.0 (double a) 1.0)) (Math/round (* 255.0 (double a)))
                  :else (long a))]
      (bit-or (bit-shift-left (bit-and (long r) 0xff) 24)
              (bit-shift-left (bit-and (long g) 0xff) 16)
              (bit-shift-left (bit-and (long b) 0xff) 8)
              (bit-and (long a) 0xff)))
    :else
    (let [s (str/lower-case (str/trim (if (keyword? c) (name c) (str c))))]
      (or (named-colors s)
          (let [h (str/replace s #"^#" "")
                h (case (count h)
                    3 (str (apply str (mapcat #(list % %) h)) "ff")
                    4 (apply str (mapcat #(list % %) h))
                    6 (str h "ff")
                    8 h
                    (throw (ex-info (str "Unparseable colour: " c) {:color c})))]
            (Long/parseLong h 16))))))

(defn- ^:private color-opt [m ks]
  (reduce (fn [m k] (if (contains? m k) (update m k #(format "#%08x" (color %))) m)) m ks))

;; ---------------------------------------------------------------------------
;; Image handle
;; ---------------------------------------------------------------------------

;; LAZY on purpose: a `Cleaner` spawns a daemon thread at construction, and a
;; native-image build that initializes this namespace at BUILD time would drag
;; that live Cleaner into the image heap — GraalVM fails the build outright
;; ("Detected a java.lang.ref.Cleaner object in the image heap"). Behind a delay
;; the thread is created on the first handle instead, at run time.
(def ^:private cleaner (delay (Cleaner/create)))

(defn- free-image! [^MemorySegment p]
  (when-not (null? p) (invoke :image-free p)))

(deftype Img [^MemorySegment seg ^AtomicBoolean freed]
  java.lang.AutoCloseable
  ;; The SAME flag the Cleaner action holds: whoever wins the CAS frees, the
  ;; other one is a no-op. Anything less is a double free — the Rust allocator
  ;; aborts the process, it does not throw.
  (close [_]
    (when (.compareAndSet freed false true)
      (free-image! seg))))

(defn img? [x] (instance? Img x))

(defn- seg ^MemorySegment [x]
  (if (img? x)
    (.-seg ^Img x)
    (throw (ex-info "Not an imaging image handle" {:value (type x)}))))

(defn- img! [^MemorySegment p op]
  (when (null? p) (fail! op))
  (let [freed (AtomicBoolean. false)
        i     (Img. p freed)]
    ;; Belt and braces: `with-open`/`close` is the intended lifecycle, but a
    ;; dropped handle must not leak the Rust allocation for the whole process.
    ;; The action must NOT capture `i` (that would keep it reachable forever).
    (.register ^Cleaner @cleaner i (reify Runnable
                                     (run [_]
                                       (when (.compareAndSet freed false true)
                                         (free-image! p)))))
    i))

(defn close!
  "Free an image handle now. Idempotent; `Img` is also `AutoCloseable`."
  [img] (.close ^java.lang.AutoCloseable img) nil)

;; ---------------------------------------------------------------------------
;; Input coercion
;; ---------------------------------------------------------------------------

(defn- ->bytes
  "Bytes from a byte[], String path/URL/data-URI, File, Path, InputStream, URL or
   ByteBuffer. A String that isn't an existing path is treated as document source
   (SVG markup) and encoded UTF-8."
  [x]
  (cond
    (bytes? x) x
    (instance? java.nio.ByteBuffer x)
    (let [^java.nio.ByteBuffer b (.duplicate ^java.nio.ByteBuffer x)
          a (byte-array (.remaining b))] (.get b a) a)
    (instance? InputStream x)
    (let [out (ByteArrayOutputStream.)] (io/copy x out) (.toByteArray out))
    (instance? Path x) (Files/readAllBytes ^Path x)
    (instance? File x) (Files/readAllBytes (.toPath ^File x))
    (instance? URL x) (with-open [in (io/input-stream x)]
                        (let [out (ByteArrayOutputStream.)] (io/copy in out) (.toByteArray out)))
    (string? x)
    (let [s ^String x]
      (cond
        (str/starts-with? s "data:")
        (.decode (java.util.Base64/getDecoder)
                 ^String (subs s (inc (long (str/index-of s ",")))))
        (.exists (io/file s)) (Files/readAllBytes (.toPath (io/file s)))
        (re-find #"(?i)^https?://" s) (with-open [in (io/input-stream s)]
                                        (let [out (ByteArrayOutputStream.)] (io/copy in out) (.toByteArray out)))
        :else (.getBytes s "UTF-8")))
    :else (throw (ex-info "Cannot read image bytes from value" {:type (type x)}))))

(defn- alloc-bytes ^MemorySegment [^Arena arena ^bytes b]
  (let [s (.allocate arena ValueLayout/JAVA_BYTE (alength b))]
    (MemorySegment/copy b 0 s ValueLayout/JAVA_BYTE 0 (alength b))
    s))

(defn- cs ^MemorySegment [^Arena arena x]
  (if (nil? x) MemorySegment/NULL (.allocateFrom arena (str x))))

(defn- json-arg ^MemorySegment [^Arena arena m]
  (if (or (nil? m) (and (map? m) (empty? m))) MemorySegment/NULL (.allocateFrom arena (->json m))))

;; ---------------------------------------------------------------------------
;; Public API — library
;; ---------------------------------------------------------------------------

(defn version
  "The cdylib's version string (crate version + backing engines)."
  ^String [] (.getString (.reinterpret ^MemorySegment (invoke :version) Long/MAX_VALUE) 0))

(defn fonts
  "Every font family known to the shared font database, as a vector of strings.
   The four Noto faces (sans regular/bold/italic + mono) are embedded, so text
   and SVG rendering never depend on system fonts."
  [] (vec (json-> (take-string! (invoke :fonts)))))

(defn register-font!
  "Add a TTF/OTF/TTC (bytes/File/path/stream) to the font database for this
   process; returns the vector of families it provided."
  [font]
  (with-open [arena (Arena/ofConfined)]
    (let [^bytes b (->bytes font)
          r (take-string! (invoke :register-font (alloc-bytes arena b) (long (alength b))))]
      (or (some-> r json-> vec) (fail! :register-font)))))

;; ---------------------------------------------------------------------------
;; Public API — decode / create / encode
;; ---------------------------------------------------------------------------

(defn probe
  "Identify encoded bytes without a full decode:
   `{:format :width :height :bytes :is-animated :frames :color :channels :bits
   :is-grayscale :has-alpha}`. The colour keys describe the SOURCE file — a
   decoded `Img` is always RGBA8, so this is the only way to learn that a PNG
   was 8-bit grayscale (`:color \"l8\"`) or 16-bit. `:color` is one of `\"l8\"`
   `\"la8\"` `\"rgb8\"` `\"rgba8\"` `\"l16\"` `\"la16\"` `\"rgb16\"` `\"rgba16\"`
   `\"rgb32f\"` `\"rgba32f\"`; indexed/palette sources report what they expand
   into. SVG documents report `:format \"svg\"` and rasterise as `\"rgba8\"`."
  [data]
  (with-open [arena (Arena/ofConfined)]
    (let [^bytes b (->bytes data)]
      (or (some-> (take-string! (invoke :probe (alloc-bytes arena b) (long (alength b)))) json->)
          (fail! :probe)))))

(defn decode
  "Decode PNG/JPEG/WebP/GIF/BMP/TIFF/ICO/QOI/PNM/TGA/HDR/EXR/HEIC/HEIF/AVIF bytes —
   or an SVG/SVGZ document, sized the way a browser would (see `svg-canvas`) —
   into an `Img` handle. A HEIF file (HEVC, AV1, H.264, JPEG or uncompressed
   items) comes back display-ready: grid tiles joined, rotation and mirroring
   applied, alpha kept. Close it (`with-open`) when done."
  ^Img [data]
  (with-open [arena (Arena/ofConfined)]
    (let [^bytes b (->bytes data)]
      (img! (invoke :decode (alloc-bytes arena b) (long (alength b))) :decode))))

(defn blank
  "A new `w` x `h` image filled with `c` (default transparent)."
  (^Img [w h] (blank w h 0x00000000))
  (^Img [w h c] (img! (invoke :new (int w) (int h) (unchecked-int (color c))) :new)))

(defn from-pixels
  "An image from straight (non-premultiplied) RGBA8 rows — `w*h*4` bytes."
  ^Img [^bytes rgba w h]
  (with-open [arena (Arena/ofConfined)]
    (img! (invoke :from-pixels (alloc-bytes arena rgba) (long (alength rgba)) (int w) (int h))
          :from-pixels)))

(defn pixels
  "The image's straight RGBA8 rows as a byte[]."
  ^bytes [img] (take-bytes! (invoke :pixels img) :pixels))

(defn info
  "`{:width :height :is-opaque :is-grayscale}` — read off the handle's own RGBA8
   pixels (`:is-grayscale` = every pixel has r = g = b), not off the file it came
   from; `probe` answers that one."
  [img] (or (some-> (take-string! (invoke :info img)) json->) (fail! :info)))

(defn width ^long [img] (long (:width (info img))))
(defn height ^long [img] (long (:height (info img))))

(defn encode
  "Encode to `fmt` (`:png` `:jpeg` `:webp` `:gif` `:bmp` `:tiff` `:ico` `:qoi`
   `:pnm` `:tga`); returns a byte[]. `quality` (1-100, default 85) applies to the
   lossy formats."
  (^bytes [img] (encode img :png nil))
  (^bytes [img fmt] (encode img fmt nil))
  (^bytes [img fmt quality]
   (with-open [arena (Arena/ofConfined)]
     (take-bytes! (invoke :encode img (cs arena (name (or fmt :png))) (int (or quality 0)))
                  :encode))))

(defn decode-gif
  "Decode an animated (or still) GIF into `{:width :height :loop-count :frames}`,
   where each frame is `{:delay-ms :rgba}` with `:rgba` the straight RGBA8 rows
   as a byte[] (`width*height*4`). `:loop-count` is the iteration count, -1 for
   loop forever. The native multi-frame path — an animated GIF no longer needs a
   hand-rolled Clojure codec."
  [data]
  (with-open [arena (Arena/ofConfined)]
    (let [^bytes b (->bytes data)
          m        (or (some-> (take-string!
                                (invoke :decode-gif (alloc-bytes arena b) (long (alength b))))
                               json->)
                       (fail! :decode-gif))]
      (-> m
          (update :frames (fn [fs] (mapv #(update % :rgba b64-decode) fs)))
          (assoc :loop-count (long (:loop-count m)))))))

(defn encode-gif
  "Encode an animated GIF from `{:width :height :loop-count :frames}`, each frame
   `{:delay-ms :rgba}` (straight RGBA8 byte[]). `:loop-count` -1/omitted = loop
   forever. Returns a byte[]."
  ^bytes [spec]
  (let [wire (-> spec
                 (assoc :width     (long (:width spec))
                        :height    (long (:height spec))
                        :loop-count (long (or (:loop-count spec) -1)))
                 (update :frames (fn [fs]
                                   (mapv (fn [f]
                                           {:delay-ms (long (or (:delay-ms f) 0))
                                            :rgba    (b64-encode (:rgba f))})
                                         fs))))]
    (with-open [arena (Arena/ofConfined)]
      (take-bytes! (invoke :encode-gif (json-arg arena wire)) :encode-gif))))

;; ---------------------------------------------------------------------------
;; Public API — video (MP4 / H.264)
;; ---------------------------------------------------------------------------

(defn mp4?
  "True when `data` looks like an ISO-BMFF/MP4 container (a `ftyp` box at byte
   4). A sniff, not a validation — `probe-video` is the one that reads tracks."
  [data]
  (let [^bytes b (->bytes data)]
    (and (>= (alength b) 12)
         (= 102 (aget b 4)) (= 116 (aget b 5))
         (= 121 (aget b 6)) (= 112 (aget b 7)))))

(defn probe-video
  "Identify an MP4 without decoding a frame:
   `{:format :codec :codec-kind :is-decodable :width :height :frames :duration-s
   :fps :timescale :has-audio}`. `:frames` is the sample count and `:width`
   /`:height` the container's declared track size — the bitstream's own size is
   what `decode-video` reports. `:is-decodable` is false for a video track this
   build cannot decode (HEVC, AV1, VP9); only H.264/AVC decodes."
  [data]
  (with-open [arena (Arena/ofConfined)]
    (let [^bytes b (->bytes data)]
      (or (some-> (take-string! (invoke :probe-video (alloc-bytes arena b) (long (alength b))))
                  json->)
          (fail! :probe-video)))))

(defn decode-video
  "Decode an MP4's H.264 track into
   `{:format :codec :encoding :width :height :source-width :source-height
   :frame-count :total-frames :duration-s :fps :frames}`, where each frame is
   `{:index :timestamp-s :data}` and `:data` is a byte[].

   `:encoding` decides what those bytes are: `\"rgba\"` (default) is straight
   RGBA8 rows, `width*height*4`, ready for `from-pixels`; anything `encode`
   understands (`\"png\"`, `\"jpeg\"`, `\"webp\"`, ...) is an ENCODED still, which is
   far smaller and what a terminal or browser wants anyway.

   Frames come out in DISPLAY order (B-frames are reordered), and RGBA is
   converted with the BT.601 limited-range matrix.

   Options bound the work — the whole clip is decoded into memory otherwise:
   `:max-frames` (stop after N), `:stride` (keep every Nth frame),
   `:max-dimension` (downscale to fit), `:filter`, plus `:quality` for lossy
   `:encoding`."
  ([data] (decode-video data nil))
  ([data opts]
   (with-open [arena (Arena/ofConfined)]
     (let [^bytes b (->bytes data)
           m        (or (some-> (take-string!
                                 (invoke :decode-video (alloc-bytes arena b) (long (alength b))
                                         (json-arg arena opts)))
                                json->)
                        (fail! :decode-video))]
       (-> m
           (assoc :frame-count (long (:frames m)))
           (dissoc :frames-data)
           (assoc :frames (mapv #(update % :data b64-decode) (:frames-data m))))))))

(defn video->gif
  "Transcode an MP4 straight to an animated GIF byte[] — the frames never cross
   the FFI boundary, so this is the cheap way to make a clip watchable anywhere
   a GIF is (a terminal, a chat wire that allows `image/gif`, a browser).

   Options: the `decode-video` ones (`:max-frames` `:stride` `:max-dimension`
   `:filter`) plus `:fps` (playback rate; default the source rate divided by
   `:stride`) and `:loop-count` (-1/omitted = forever). GIF stores its delay in
   CENTIseconds, so the frame rate is quantised to 1/100 s."
  (^bytes [data] (video->gif data nil))
  (^bytes [data opts]
   (with-open [arena (Arena/ofConfined)]
     (let [^bytes b (->bytes data)]
       (take-bytes! (invoke :video->gif (alloc-bytes arena b) (long (alength b))
                            (json-arg arena opts))
                    :video->gif)))))

(defn convert
  "One-shot decode -> normalise -> re-encode: the \"make this attachment safe to
   send\" path, without ever materialising an image handle.

   Options: `:format` `:quality` `:max-width` `:max-height` `:width` `:height`
   `:background` (flatten alpha onto it) `:filter` `:max-bytes` (retry at lower
   quality/scale until the output fits). Returns a byte[]."
  (^bytes [data] (convert data nil))
  (^bytes [data opts]
   (with-open [arena (Arena/ofConfined)]
     (let [^bytes b (->bytes data)]
       (take-bytes! (invoke :convert (alloc-bytes arena b) (long (alength b))
                            (json-arg arena (color-opt opts [:background])))
                    :convert)))))

(defn save!
  "Encode `img` and write it to `file`; the format comes from the extension
   unless `:format` says otherwise. Returns the file."
  ([img file] (save! img file nil))
  ([img file {:keys [format quality] :as _opts}]
   (let [f   (io/file file)
         ext (str/lower-case (or (some-> format name) (last (str/split (.getName f) #"\.")) "png"))]
     (io/copy (encode img (keyword ext) quality) f)
     f)))

;; ---------------------------------------------------------------------------
;; Public API — transforms (each returns a NEW image)
;; ---------------------------------------------------------------------------

(defn resize
  "Resample to exactly `w` x `h`. `filter`: `:nearest` `:triangle`/`:bilinear`
   `:catmullrom`/`:bicubic` `:gaussian` `:lanczos3` (default), passed through."
  (^Img [img w h] (resize img w h nil))
  (^Img [img w h filter]
   (with-open [arena (Arena/ofConfined)]
     (img! (invoke :resize img (int w) (int h) (cs arena (some-> filter name))) :resize))))

(defn thumbnail
  "Scale DOWN to fit inside `max-w` x `max-h`, preserving aspect ratio."
  ^Img [img max-w max-h]
  (img! (invoke :thumbnail img (int max-w) (int max-h)) :thumbnail))

(defn crop ^Img [img x y w h]
  (img! (invoke :crop img (long x) (long y) (int w) (int h)) :crop))

(defn rotate
  "Rotate counter-clockwise by `degrees`. `:expand` true grows the canvas to hold
   the rotated image; `:background` fills behind it."
  (^Img [img degrees] (rotate img degrees nil))
  (^Img [img degrees {:keys [expand background]}]
   (img! (invoke :rotate img (double degrees) (int (if expand 1 0))
                 (unchecked-int (color background)))
         :rotate)))

(defn flip
  "`:horizontal`/`:h` or `:vertical`/`:v`."
  ^Img [img mode]
  (with-open [arena (Arena/ofConfined)]
    (img! (invoke :flip img (cs arena (name mode))) :flip)))

(defn grayscale ^Img [img] (img! (invoke :grayscale img) :grayscale))
(defn invert ^Img [img] (img! (invoke :invert img) :invert))
(defn blur ^Img [img sigma] (img! (invoke :blur img (float sigma)) :blur))

(defn flatten
  "Composite onto an opaque background (default white)."
  (^Img [img] (flatten img "#ffffffff"))
  (^Img [img bg] (img! (invoke :flatten img (unchecked-int (color bg))) :flatten)))

(defn adjust
  "Photometric adjustment: `:brightness` `:contrast` `:saturation` `:opacity`
   (1.0 = unchanged)."
  ^Img [img opts]
  (with-open [arena (Arena/ofConfined)]
    (img! (invoke :adjust img (json-arg arena opts)) :adjust)))

;; ---------------------------------------------------------------------------
;; Public API — mutation
;; ---------------------------------------------------------------------------

(defn paste!
  "Draw `src` into `dst` at (x,y), IN PLACE. `:blend` false replaces the
   destination pixels instead of alpha-compositing."
  ([dst src x y] (paste! dst src x y nil))
  ([dst src x y {:keys [blend] :or {blend true}}]
   (when (neg? (int (invoke :paste dst src (long x) (long y) (int (if blend 1 0)))))
     (fail! :paste))
   dst))

(defn get-pixel
  "Packed `0xRRGGBBAA` of one pixel, or nil when out of bounds."
  [img x y]
  (let [v (long (invoke :get-pixel img (int x) (int y)))]
    (when-not (neg? v) v)))

(defn put-pixel! [img x y c]
  (when (neg? (int (invoke :put-pixel img (int x) (int y) (unchecked-int (color c)))))
    (fail! :put-pixel))
  img)

(defn draw!
  "Run a batch of vector drawing ops on `img`, IN PLACE — this is the
   `Graphics2D` replacement. One call per batch keeps FFI chatter out of drawing
   loops; ops are applied in order and inherit the batch's style defaults.

   Style keys (batch level or per op): `:fill` `:stroke` `:stroke-width` `:dash`
   `:cap` (`:butt` `:round` `:square`) `:join` (`:miter` `:round` `:bevel`)
   `:size` `:family` `:weight` `:italic` `:letter-spacing` `:line-height`.

   Ops (`:op`):
     `:clear`     — fill everything with `:fill`
     `:clip`      — clip to `:x :y :w :h` (`:op :reset-clip` to drop it)
     `:line`      — `:x1 :y1 :x2 :y2` (or `:points`)
     `:polyline` / `:polygon` — `:points [[x y] ...]`, `:close`
     `:rect`      — `:x :y :w :h`, optional `:radius` (rounded)
     `:circle`    — `:cx :cy :r`     `:ellipse` — `:cx :cy :rx :ry`
     `:arc` / `:wedge` — `:cx :cy :r :r-inner :start :end` (degrees)
     `:path`      — `:d` (SVG path data), `:fill-rule`
     `:text`      — `:text :x :y :anchor (:start|:middle|:end) :baseline :rotate`
     `:image`     — `:image` (an `Img`), `:x :y :w :h :opacity`

   Returns `img`."
  [img ops]
  (let [ops     (if (map? ops) [ops] (vec ops))
        ;; `:image` ops carry a live handle; send its address, the cdylib reads
        ;; the pixels synchronously inside this call.
        prepare (fn [op]
                  (-> (reduce (fn [o k] (if (contains? o k) (update o k #(format "#%08x" (color %))) o))
                              op [:fill :stroke :background])
                      (cond-> (img? (:image op))
                        (assoc :image (str "handle:" (.address ^MemorySegment (seg (:image op))))))))
        ;; The pasted handles reach the cdylib as bare addresses inside the JSON,
        ;; so `invoke` cannot see them: fence them here or the Cleaner may free a
        ;; source image mid-draw.
        live    (into [] (keep :image) ops)]
    (with-open [arena (Arena/ofConfined)]
      (try
        (when (neg? (int (invoke :draw img
                                 (.allocateFrom arena (->json {:ops (mapv prepare ops)})))))
          (fail! :draw))
        (finally (keep-live! live))))
    img))

(defn text-measure
  "Measure a text run without drawing it — same `:text :size :family :weight
   :italic :letter-spacing :line-height` keys as the `:text` draw op. Returns
   `{:width :height :x :y :lines}`, the ink box relative to the first baseline."
  [spec]
  (with-open [arena (Arena/ofConfined)]
    (or (some-> (take-string! (invoke :text-measure (.allocateFrom arena (->json spec)))) json->)
        (fail! :text-measure))))

;; ---------------------------------------------------------------------------
;; Public API — SVG + plotting
;; ---------------------------------------------------------------------------

(defn render-svg
  "Rasterise an SVG/SVGZ document (bytes, markup String, File, path, stream) with
   resvg. Options: `:width` `:height` `:scale` `:max-width` `:max-height`
   `:background` `:dpi` `:font-family` `:font-size` `:base-url`.

   The canvas is REPAIRED by default: a document with a zero, negative or absent
   size is sized the way a BROWSER would (see `svg-canvas`) instead of failing
   resvg's size check. Pass `:repair false` for resvg's literal answer."
  (^Img [svg] (render-svg svg nil))
  (^Img [svg opts]
   (with-open [arena (Arena/ofConfined)]
     (let [^bytes b (->bytes svg)]
       (img! (invoke :svg-render (alloc-bytes arena b) (long (alength b))
                     (json-arg arena (color-opt opts [:background])))
             :svg-render)))))

(defn svg-canvas
  "How a BROWSER would size an SVG/SVGZ document (bytes, markup String, File,
   path, stream): `{:width :height :source :is-repaired :svg}`.

   resvg answers only for documents that declare a usable size: it refuses a zero
   or negative one, and measures a size-less document from the ORIGIN, so a
   figure drawn at x=50 gets 50px of dead margin. A browser falls back to the
   `viewBox`, then to the ink actually painted. `:source` says which rule won —
   `\"declared\"` (resvg's own resolved size), `\"view_box\"`, `\"content\"` or
   `\"default\"` — and `:svg` is the REPAIRED markup (a byte[], nil when the
   document needed no repair), which `render-svg`/`decode` rasterise as drawn.

   `render-svg` with `:repair true` does exactly this internally. Options:
   `:min-dimension` (512) and `:max-dimension` (4096) bound an ink-framed canvas,
   plus the parse options of `render-svg`."
  ([svg] (svg-canvas svg nil))
  ([svg opts]
   (with-open [arena (Arena/ofConfined)]
     (let [^bytes b (->bytes svg)
           m        (or (some-> (take-string!
                                 (invoke :svg-canvas (alloc-bytes arena b) (long (alength b))
                                         (json-arg arena (color-opt opts [:background]))))
                                json->)
                        (fail! :svg-canvas))]
       (cond-> m (:svg m) (update :svg b64-decode))))))

(defn plot
  "Render a plot specification to an image. See `com.blockether.imaging.plot` for
   the spec builders (`line`, `bar`, `scatter`, `hist`, `pie`, `heatmap`, …) and
   the full vocabulary."
  ^Img [spec]
  (with-open [arena (Arena/ofConfined)]
    (img! (invoke :plot (.allocateFrom arena (->json spec))) :plot)))

;; ---------------------------------------------------------------------------
;; Public API — size optimisation
;; ---------------------------------------------------------------------------

(defn optimize
  "Shrink an ENCODED image with the real, format-specific optimisers — oxipng
   (filter/heuristic search, colour-type and palette reduction, optional zopfli)
   for PNG, jpegtran-style lossless metadata stripping for JPEG, and a
   differencing re-encoder (gifsicle's algorithm) for GIF. Takes and returns
   encoded bytes.

   LOSSLESS unless you ask otherwise: the pixels of a PNG or a JPEG come back
   bit-identical unless `:lossy`, a `:quality`, a different `:format`, a
   `:max-width`/`:max-height` or an unmeetable `:max-bytes` licenses a re-encode.

   Options: `:format` (default: keep the input's) `:level` 0-6 effort
   `:lossy` (palette-quantise PNG / re-encode JPEG) `:colors` `:dither`
   `:quality` `:progressive` `:strip` (default true) `:background`
   `:max-width` `:max-height` (downscale to fit, never enlarge)
   `:max-bytes` (keep trying until it fits)
   `:force` (return the optimised bytes even when they are bigger).

   Returns a byte[] — the smaller of optimised and original unless `:force`."
  (^bytes [data] (optimize data nil))
  (^bytes [data opts]
   (with-open [arena (Arena/ofConfined)]
     (let [^bytes b (->bytes data)]
       (take-bytes! (invoke :optimize (alloc-bytes arena b) (long (alength b))
                            (json-arg arena (color-opt opts [:background])))
                    :optimize)))))

(defn quantize
  "Median-cut colour quantisation with optional Floyd-Steinberg dithering — the
   palette engine behind `optimize`'s lossy PNG and GIF paths.

   Returns `{:width :height :palette :indices :transparent :rgba}`: `:palette` a
   vector of `0xRRGGBB` ints, `:indices` one palette index per pixel (byte[]),
   `:transparent` the palette slot used for fully transparent pixels (or nil),
   `:rgba` the quantised image back as straight RGBA8 rows."
  ([img] (quantize img nil))
  ([img {:keys [colors dither] :or {colors 256 dither false}}]
   (let [m (or (some-> (take-string! (invoke :quantize img (int colors) (int (if dither 1 0))))
                       json->)
               (fail! :quantize))]
     (-> m
         (update :palette vec)
         (update :indices b64-decode)
         (update :rgba b64-decode)))))

;; ---------------------------------------------------------------------------
;; Public API — neighbourhood filters
;; ---------------------------------------------------------------------------

(defn convolve
  "Correlate `img` with an odd `size` x `size` `kernel` (a seq of `size*size`
   weights, row-major) — blur, sharpen, emboss, edge detect: Pillow's
   `ImageFilter.Kernel`, and the same arithmetic.

   `:scale` divides the weighted sum (0 or nil = no division) and `:offset` is
   added afterwards; the result is rounded half-up and clamped to 0..255. Edge
   samples clamp to the border pixel and the source ALPHA is carried through
   untouched, so blurring never eats a mask."
  (^Img [img size kernel] (convolve img size kernel nil))
  (^Img [img size kernel {:keys [scale offset] :or {scale 0 offset 0}}]
   (with-open [arena (Arena/ofConfined)]
     (img! (invoke :filter img (json-arg arena {:kind   "convolve"
                                                :size   (int size)
                                                :kernel (mapv double kernel)
                                                :scale  (double scale)
                                                :offset (double offset)}))
           :filter))))

(defn rank-filter
  "Per-channel rank filter over an odd `size` x `size` window: every channel
   (alpha included) is sorted independently and the `rank`-th smallest kept.
   `:min` / `:median` / `:max` are the named ranks — Pillow's `MinFilter`,
   `MedianFilter` and `MaxFilter`; an integer picks the rank directly."
  ^Img [img size rank]
  (with-open [arena (Arena/ofConfined)]
    (let [n (* (long size) (long size))
          r (case rank
              (:min :minimum) 0
              (:max :maximum) (dec n)
              (:median nil) (quot n 2)
              (long rank))]
      (img! (invoke :filter img (json-arg arena {:kind "rank" :size (int size) :rank r}))
            :filter))))

;; ---------------------------------------------------------------------------
;; Public API — Office Open XML documents
;; ---------------------------------------------------------------------------

(defn- office-wire
  "Prepare an Office spec for the wire: binary payloads (`:data`) may be given as
   a byte[], File, Path, URL or InputStream, and cross as base64."
  [spec]
  (walk/postwalk
   (fn [v]
     (if (or (bytes? v)
             (instance? File v) (instance? Path v) (instance? URL v)
             (instance? InputStream v) (instance? java.nio.ByteBuffer v))
       (b64-encode (->bytes v))
       v))
   spec))

(defn office
  "Build an Office Open XML document from a declarative spec and return its
   bytes. `kind` is `:xlsx`, `:docx` or `:pptx`. Pure Rust — rust_xlsxwriter for
   workbooks, hand-written WordprocessingML/PresentationML for the rest; no POI,
   no JVM document model, and it works in a native image.

   See `xlsx`, `docx` and `pptx` for the per-kind spec."
  ^bytes [kind spec]
  (with-open [arena (Arena/ofConfined)]
    (take-bytes! (invoke :office-build (cs arena (name kind))
                         (.allocateFrom arena (->json (office-wire spec))))
                 :office-build)))

(defn xlsx
  "A `.xlsx` workbook as bytes, written by rust_xlsxwriter.

   `{:properties {:title :subject :author :manager :company :category :keywords
                  :comments :status}
     :sheets [{:name \"Sales\"
               :columns [{:first 0 :last 3 :width 18 :format {…} :hidden false}]
               :rows [{:index 0 :height 22 :format {…}}]
               :freeze [row col] :autofilter [r1 c1 r2 c2]
               :merges [{:range [r1 c1 r2 c2] :value … :format {…}}]
               :images [{:row :col :data <bytes> :scale-x :scale-y}]
               :cells [{:row 0 :col 0 :value \"Region\" :format {:bold true}}]}]}`

   A cell's `:type` is optional — a string/number/boolean/nil is written as
   itself; the explicit types are `string` `number` `boolean` `formula`
   `datetime` `date` `time` `url` `blank` and `rich` (a run list of
   `[format? text]`). A `:format` map (`:bold :italic :underline :font :size
   :color :bg-color :align :valign :wrap :rotation :indent :border :num-format
   :locked` …) is deduplicated into one workbook format automatically."
  ^bytes [spec] (office :xlsx spec))

(defn docx
  "A `.docx` document as bytes (hand-written WordprocessingML).

   `{:properties {…}
     :page {:width-pt 595 :height-pt 842 :landscape false :margin-pt 72}
     :blocks [{:type :heading :level 1 :text \"Q3\"}
              {:type :paragraph :style :Quote :align :center :bullet true
               :runs [{:text \"revenue \"} {:text \"up\" :bold true :color \"#c00\"}]}
              {:type :table :header true :borders true
               :rows [{:cells [{:text \"a\"} {:runs [{:text \"b\" :bold true}]}]}]}
              {:type :image :data <bytes> :width-pt 240 :alt \"chart\"}
              {:type :page-break}]}`

   `:body` is accepted as a synonym for `:blocks`. Font sizes are points,
   spacing/indent `*-pt` keys are points; the module converts to half-points and
   twips."
  ^bytes [spec] (office :docx spec))

(defn pptx
  "A `.pptx` presentation as bytes (hand-written PresentationML).

   `{:width 12192000 :height 6858000            ; EMU, 16:9 by default
     :properties {…}
     :layouts [{:name \"Title Slide\" :type :title
                :placeholders [{:type :ctrTitle :idx 0
                                :left :top :width :height :size :align}]}]
     :slides [{:layout 0 :background \"#ffffff\" :notes \"…\"
               :shapes [{:kind :textbox|:auto|:picture|:table|:chart|:connector
                         :left :top :width :height :rotation
                         :preset :roundRect :fill \"#3366cc\" :line {:color :width}
                         :image {:data <bytes> :crop {:left :top :right :bottom}}
                         :chart {:type :column :title \"Revenue\"
                                 :categories [\"Q1\" \"Q2\"]
                                 :legend {:position :bottom}
                                 :series [{:name \"Sales\" :values [3 7]
                                           :fill \"4472C4\"}]}
                         :text-frame {:paragraphs [{:align :level :bullet
                                                    :runs [{:text \"hi\" :bold true
                                                            :size 1800}]}]}}]}]}`

   A chart `:type` is one of `:column :bar :line :area :pie :doughnut :radar
   :scatter :bubble` plus the `*-stacked` / `*-percent-stacked` variants. Each
   chart becomes its own `ppt/charts/chartN.xml` part, so PowerPoint and
   `python-pptx` see a real chart rather than a picture of one.

   Lengths are EMU (`914400` per inch), font sizes centipoints (`1800` = 18 pt) —
   exactly how `python-pptx` normalises its own `Length`/`Pt` values."
  ^bytes [spec] (office :pptx spec))

(defn read-office
  "Read an Office document back. Workbooks (`.xlsx` `.xlsm` `.xlsb` `.xls`
   `.ods`) come back as `{:kind :sheets [{:name :rows [[cell …] …]}]}` with cells
   already coerced to strings/numbers/booleans; `.docx` comes back as
   `{:kind :paragraphs [...]}`.

   A `.pptx` is read back as the mirror image of what `pptx` writes — the whole
   DrawingML shape tree, not just its text:

   `{:kind :width :height :properties
     :slides [{:name :index :layout :notes :paragraphs
               :shapes [{:kind :name :left :top :width :height :rotation
                         :fill {…} :line {…}
                         :text-frame {…} :table {…} :chart {…} :image {…}}]}]}`

   so a deck can be read, edited and written again without losing its shapes.

   Options: `:sheet` (name or index — read just that one), `:header` (treat the
   first row as keys and return maps), `:max-rows`, and `:with-images` (base64
   every embedded picture of a presentation; off by default)."
  ([data] (read-office data nil))
  ([data opts]
   (with-open [arena (Arena/ofConfined)]
     (let [^bytes b (->bytes data)]
       (or (some-> (take-string! (invoke :office-read (alloc-bytes arena b) (long (alength b))
                                         (json-arg arena opts)))
                   json->)
           (fail! :office-read))))))

;; ---------------------------------------------------------------------------
;; Public API — any document -> Markdown
;; ---------------------------------------------------------------------------

(defn- doc-call [op data opts]
  (with-open [arena (Arena/ofConfined)]
    (let [^bytes b (->bytes data)]
      (or (some-> (take-string! (invoke op (alloc-bytes arena b) (long (alength b))
                                        (json-arg arena (some-> opts
                                                                (update :format #(some-> % name))))))
                  json->)
          (fail! op)))))

(defn document-format
  "Identify a document WITHOUT converting it: `{:format :source :formats}`.

   `:format` is one of `:doc :docx :odt :pdf :ppt :pptx :rtf :epub :excel :ods
   :odp :csv`, or nil when nothing recognised the input. `:source` says which
   evidence won — `\"content\"` (the container's own signature), `\"name\"` (the
   extension of `:name`) or `\"format\"` (you said so).

   The signature is asked FIRST, because a signature cannot lie and an
   extension routinely does. CSV carries no signature at all, so a `:name` is
   its only route.

   Options: `:name` (file name or extension), `:format` (force it)."
  ([data] (document-format data nil))
  ([data opts]
   (update (doc-call :doc-detect data opts) :format #(some-> % keyword))))

(defn read-document
  "Convert ANY document a human sent into GitHub-Flavored Markdown:
   `{:format :source :chars :markdown :assets}`, and with `:blocks` the
   STRUCTURE behind that Markdown too.

   Word (`.doc` `.docx` `.odt` `.rtf`), PDF, EPUB, presentations (`.ppt`
   `.pptx` `.odp`) and spreadsheets (`.xls` `.xlsx` `.xlsm` `.xlsb` `.ods`
   `.csv`) all come back as one Markdown string, with their headings, lists,
   tables and links intact — which is what an LLM can actually read. Nothing
   here shells out to a headless office suite: `anydoc` parses every format in
   pure Rust inside the cdylib.

   Options: `:format` (force the parser, e.g. `:csv`), `:name` (the file name,
   the only route for signature-less CSV), `:assets` (also return every
   embedded binary as `{:id :media-type :origin-part :size :bytes}` with
   `:bytes` a `byte[]`), `:max-assets`, `:blocks`.

   `:blocks true` additionally returns:

   - `:text` — the document as PLAIN text. Same content, none of Markdown's
     punctuation, so a matcher never trips over `**March**` or a `|`, and an
     offset means a position in the DOCUMENT rather than in a rendering of it.
   - `:blocks` — one entry per heading, paragraph, list item, table row, code
     block and note, in reading order: `{:index :kind :text :start :end :line
     :path}` where `:start`/`:end` are CHARACTER offsets into `:text`, `:line`
     is 1-based, and `:path` is the heading breadcrumb the block sits under.
     A heading adds `:level`/`:anchor`, a list item `:marker`/`:list-depth`/
     `:checked`, a code block `:lang`, a note `:note`/`:note-kind`, a quoted
     block `:quote-depth`, and a table row `:row`/`:is-header`/`:cells`, each
     cell `{:column :name :text :start :end}` — so a hit can be reported as
     row 4, column Revenue.
   - `:pages` — the page count for a PDF, nil for everything else.

   A PDF has no document model, so it never reports assets; its blocks come
   from per-page extraction instead, and every one of them carries `:page`."
  ([data] (read-document data nil))
  ([data opts]
   (let [m (doc-call :doc-markdown data opts)]
     (cond-> (update m :format #(some-> % keyword))
       (:assets m) (update :assets #(mapv (fn [a] (update a :bytes b64-decode)) %))))))

(defn document->markdown
  "The Markdown of any document, as a String — `read-document` when all you
   want is the text. Same options."
  (^String [data] (document->markdown data nil))
  (^String [data opts] (:markdown (read-document data opts))))
