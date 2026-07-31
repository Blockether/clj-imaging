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
            [clojure.string :as str])
  (:import [java.io ByteArrayOutputStream File InputStream]
           [java.lang.foreign AddressLayout Arena FunctionDescriptor Linker Linker$Option
            MemoryLayout MemorySegment SymbolLookup ValueLayout ValueLayout$OfDouble
            ValueLayout$OfFloat ValueLayout$OfInt ValueLayout$OfLong]
           [java.lang.invoke MethodHandle]
           [java.lang.ref Cleaner]
           [java.net URL]
           [java.nio.file CopyOption Files LinkOption Path StandardCopyOption]
           [java.util.concurrent.atomic AtomicBoolean]
           [java.util.jar JarFile]))

(set! *warn-on-reflection* true)

;; ---------------------------------------------------------------------------
;; Platform + library resolution (mirrors clj-ruff / clj-fff)
;; ---------------------------------------------------------------------------

(defn- platform []
  (let [os   (.. (System/getProperty "os.name") toLowerCase)
        arch (.. (System/getProperty "os.arch") toLowerCase)
        os*  (cond
               (or (.contains os "mac") (.contains os "darwin")) "darwin"
               (.contains os "linux") "linux"
               (.contains os "win")   "windows"
               :else (throw (ex-info (str "Unsupported OS for imaging: " os) {:os os})))
        arch* (cond
                (#{"aarch64" "arm64"} arch) "arm64"
                (#{"x86_64" "amd64"} arch)  "x64"
                :else (throw (ex-info (str "Unsupported arch for imaging: " arch) {:arch arch})))]
    [os* arch*]))

(defn- lib-file-name [os]
  (case os
    "darwin"  "libimaging_c.dylib"
    "linux"   "libimaging_c.so"
    "windows" "imaging_c.dll"))

(defn- native-artifact [platform] (str "imaging-native-" platform))

(defn- configured-native-path ^Path []
  (when-let [p (or (System/getenv "IMAGING_NATIVE_PATH")
                   (System/getProperty "com.blockether.imaging.native.path"))]
    (.toPath (io/file p))))

(defn- bundled-library-path ^Path [res fname]
  (when-let [^URL url (io/resource res)]
    (if (= "file" (.getProtocol url))
      (.toPath (io/file url))
      (let [dot (.lastIndexOf ^String fname ".")
            tmp (doto (File/createTempFile "libimaging_c" (subs fname dot)) .deleteOnExit)]
        (with-open [in (io/input-stream url)]
          (io/copy in tmp))
        (.toPath tmp)))))

(defn- artifact-version []
  ;; NAMESPACED resource (imaging/VERSION), never the jar root — an unqualified
  ;; "VERSION" collides with every other lib that ships one (ruff, fff, rift…),
  ;; whichever is first on the classpath wins, so a lib could resolve a FOREIGN
  ;; version and 404 a nonexistent <lib>-native-<that>.
  (str/trim (slurp (io/resource "imaging/VERSION"))))

(defn- cache-root ^Path []
  (if-let [p (or (System/getenv "IMAGING_CACHE_DIR")
                 (System/getProperty "com.blockether.imaging.cache-dir"))]
    (.toPath (io/file p))
    (.toPath (io/file (System/getProperty "user.home") ".cache" "clj-imaging"))))

(defn- resolve-native-jar
  "Resolve the per-platform native jar through `clojure.tools.deps` — the same
   resolver the `clojure` CLI uses, so configured Maven repositories, mirrors and
   `~/.m2/settings.xml` are honoured. tools.deps is loaded via `requiring-resolve`
   so it's only touched on this runtime download path (never pulled into a native
   image)."
  ^Path [version platform]
  (let [lib          (symbol "com.blockether" (native-artifact platform))
        create-basis (or (requiring-resolve 'clojure.tools.deps/create-basis)
                         (throw (ex-info "org.clojure/tools.deps is not on the classpath; cannot resolve the imaging native artifact. Add com.blockether/<artifact>, set IMAGING_NATIVE_PATH, or add tools.deps."
                                         {:lib lib})))
        basis        (create-basis {:project nil :extra {:deps {lib {:mvn/version version}}}})
        path         (-> basis :libs (get lib) :paths first)]
    (when-not path
      (throw (ex-info (str "Could not resolve " lib " " version
                           " via Clojure's dependency resolver. Check your Maven repositories / mirrors.")
                      {:lib lib :version version})))
    (.toPath (io/file path))))

(defn- extract-native! ^Path [^Path jar-path res ^Path dest]
  (Files/createDirectories (.getParent dest) (make-array java.nio.file.attribute.FileAttribute 0))
  (with-open [jar (JarFile. (.toFile jar-path))]
    (let [entry (.getEntry jar res)]
      (when-not entry
        (throw (ex-info (str "Native artifact is missing " res) {:jar (str jar-path) :resource res})))
      (with-open [^InputStream in (.getInputStream jar entry)]
        (let [^"[Ljava.nio.file.CopyOption;" opts (into-array CopyOption [StandardCopyOption/REPLACE_EXISTING])]
          (Files/copy in dest opts)))))
  dest)

(defn- downloaded-library-path ^Path [platform res fname]
  (when-not (#{"1" "true" "yes"} (some-> (System/getenv "IMAGING_DISABLE_DOWNLOAD") str/lower-case))
    (let [version  (artifact-version)
          root     (cache-root)
          lib-path (.resolve root (str version "/" platform "/" fname))]
      (if (Files/exists lib-path (make-array LinkOption 0))
        lib-path
        (extract-native! (resolve-native-jar version platform) res lib-path)))))

(defn- library-path ^Path []
  (let [[os arch] (platform)
        platform  (str os "-" arch)
        fname     (lib-file-name os)
        res       (str "prebuilds/" platform "/" fname)]
    (or (configured-native-path)
        (bundled-library-path res fname)
        (downloaded-library-path platform res fname)
        (throw (ex-info (str "No imaging native library for " platform
                             ". Add com.blockether/" (native-artifact platform)
                             ", set IMAGING_NATIVE_PATH, or enable runtime download.")
                        {:platform platform :resource res})))))

;; ---------------------------------------------------------------------------
;; FFM binding
;; ---------------------------------------------------------------------------

(def ^AddressLayout ^:private addr ValueLayout/ADDRESS)
(def ^ValueLayout$OfInt ^:private i32 ValueLayout/JAVA_INT)
(def ^ValueLayout$OfLong ^:private i64 ValueLayout/JAVA_LONG)
(def ^ValueLayout$OfFloat ^:private f32 ValueLayout/JAVA_FLOAT)
(def ^ValueLayout$OfDouble ^:private f64 ValueLayout/JAVA_DOUBLE)

(defn- fd [ret & args]
  (if ret
    (FunctionDescriptor/of ret (into-array MemoryLayout args))
    (FunctionDescriptor/ofVoid (into-array MemoryLayout args))))

(defn- bind! []
  (let [linker (Linker/nativeLinker)
        ;; ofAuto (GC-managed, process-lifetime), NOT ofShared: a SHARED arena is
        ;; incompatible with Truffle runtime compilation, so a native image that
        ;; also embeds GraalPy (e.g. vis) fails to build with "Arena.ofShared is
        ;; not supported with runtime compilations". ofAuto needs no flag.
        arena  (Arena/ofAuto)
        lookup (SymbolLookup/libraryLookup (library-path) arena)
        opts   (make-array Linker$Option 0)
        sym    (fn [name] (.orElseThrow (.find lookup name)))
        down   (fn [name desc] (.downcallHandle linker (sym name) desc opts))]
    {:version       (down "imaging_version"       (fd addr))
     :last-error    (down "imaging_last_error"    (fd addr))
     :free-string   (down "imaging_free_string"   (fd nil addr))
     :buf-ptr       (down "imaging_buf_ptr"       (fd addr addr))
     :buf-len       (down "imaging_buf_len"       (fd i64 addr))
     :buf-free      (down "imaging_buf_free"      (fd nil addr))
     :image-free    (down "imaging_image_free"    (fd nil addr))
     :probe         (down "imaging_probe"         (fd addr addr i64))
     :decode        (down "imaging_decode"        (fd addr addr i64))
     :new           (down "imaging_new"           (fd addr i32 i32 i32))
     :from-pixels   (down "imaging_from_pixels"   (fd addr addr i64 i32 i32))
     :pixels        (down "imaging_pixels"        (fd addr addr))
     :info          (down "imaging_info"          (fd addr addr))
     :encode        (down "imaging_encode"        (fd addr addr addr i32))
     :convert       (down "imaging_convert"       (fd addr addr i64 addr))
     :resize        (down "imaging_resize"        (fd addr addr i32 i32 addr))
     :thumbnail     (down "imaging_thumbnail"     (fd addr addr i32 i32))
     :crop          (down "imaging_crop"          (fd addr addr i64 i64 i32 i32))
     :rotate        (down "imaging_rotate"        (fd addr addr f64 i32 i32))
     :flip          (down "imaging_flip"          (fd addr addr addr))
     :grayscale     (down "imaging_grayscale"     (fd addr addr))
     :invert        (down "imaging_invert"        (fd addr addr))
     :blur          (down "imaging_blur"          (fd addr addr f32))
     :flatten       (down "imaging_flatten"       (fd addr addr i32))
     :adjust        (down "imaging_adjust"        (fd addr addr addr))
     :paste         (down "imaging_paste"         (fd i32 addr addr i64 i64 i32))
     :get-pixel     (down "imaging_get_pixel"     (fd i64 addr i32 i32))
     :put-pixel     (down "imaging_put_pixel"     (fd i32 addr i32 i32 i32))
     :draw          (down "imaging_draw"          (fd i32 addr addr))
     :text-measure  (down "imaging_text_measure"  (fd addr addr))
     :svg-render    (down "imaging_svg_render"    (fd addr addr i64 addr))
     :plot          (down "imaging_plot"          (fd addr addr))
     :register-font (down "imaging_register_font" (fd addr addr i64))
     :fonts         (down "imaging_fonts"         (fd addr))}))

(defonce ^:private handles (delay (bind!)))
(defn- invoke [k & args]
  (.invokeWithArguments ^MethodHandle (get @handles k) (object-array args)))

(defn- null? [^MemorySegment p] (or (nil? p) (= 0 (.address p))))

(defn- take-string!
  "Read an owned `char*` result and free it. nil when the call returned NULL."
  [^MemorySegment p]
  (when-not (null? p)
    (let [s (.getString (.reinterpret p Long/MAX_VALUE) 0)]
      (invoke :free-string p)
      s)))

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

(def ^:private ^Cleaner cleaner (Cleaner/create))

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
    (.register cleaner i (reify Runnable
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
   `{:format :width :height :is-animated :frames}`. SVG documents report
   `:format \"svg\"`."
  [data]
  (with-open [arena (Arena/ofConfined)]
    (let [^bytes b (->bytes data)]
      (or (some-> (take-string! (invoke :probe (alloc-bytes arena b) (long (alength b)))) json->)
          (fail! :probe)))))

(defn decode
  "Decode PNG/JPEG/WebP/GIF/BMP/TIFF/ICO/QOI/PNM/TGA/HDR/EXR bytes — or an
   SVG/SVGZ document — into an `Img` handle. Close it (`with-open`) when done."
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
  ^bytes [img] (take-bytes! (invoke :pixels (seg img)) :pixels))

(defn info
  "`{:width :height :is-opaque}`."
  [img] (or (some-> (take-string! (invoke :info (seg img))) json->) (fail! :info)))

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
     (take-bytes! (invoke :encode (seg img) (cs arena (name (or fmt :png))) (int (or quality 0)))
                  :encode))))

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
         ext (str/lower-case (or format (last (str/split (.getName f) #"\.")) "png"))]
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
     (img! (invoke :resize (seg img) (int w) (int h) (cs arena (some-> filter name))) :resize))))

(defn thumbnail
  "Scale DOWN to fit inside `max-w` x `max-h`, preserving aspect ratio."
  ^Img [img max-w max-h]
  (img! (invoke :thumbnail (seg img) (int max-w) (int max-h)) :thumbnail))

(defn crop ^Img [img x y w h]
  (img! (invoke :crop (seg img) (long x) (long y) (int w) (int h)) :crop))

(defn rotate
  "Rotate counter-clockwise by `degrees`. `:expand` true grows the canvas to hold
   the rotated image; `:background` fills behind it."
  (^Img [img degrees] (rotate img degrees nil))
  (^Img [img degrees {:keys [expand background]}]
   (img! (invoke :rotate (seg img) (double degrees) (int (if expand 1 0))
                 (unchecked-int (color background)))
         :rotate)))

(defn flip
  "`:horizontal`/`:h` or `:vertical`/`:v`."
  ^Img [img mode]
  (with-open [arena (Arena/ofConfined)]
    (img! (invoke :flip (seg img) (cs arena (name mode))) :flip)))

(defn grayscale ^Img [img] (img! (invoke :grayscale (seg img)) :grayscale))
(defn invert ^Img [img] (img! (invoke :invert (seg img)) :invert))
(defn blur ^Img [img sigma] (img! (invoke :blur (seg img) (float sigma)) :blur))

(defn flatten
  "Composite onto an opaque background (default white)."
  (^Img [img] (flatten img "#ffffffff"))
  (^Img [img bg] (img! (invoke :flatten (seg img) (unchecked-int (color bg))) :flatten)))

(defn adjust
  "Photometric adjustment: `:brightness` `:contrast` `:saturation` `:opacity`
   (1.0 = unchanged)."
  ^Img [img opts]
  (with-open [arena (Arena/ofConfined)]
    (img! (invoke :adjust (seg img) (json-arg arena opts)) :adjust)))

;; ---------------------------------------------------------------------------
;; Public API — mutation
;; ---------------------------------------------------------------------------

(defn paste!
  "Draw `src` into `dst` at (x,y), IN PLACE. `:blend` false replaces the
   destination pixels instead of alpha-compositing."
  ([dst src x y] (paste! dst src x y nil))
  ([dst src x y {:keys [blend] :or {blend true}}]
   (when (neg? (int (invoke :paste (seg dst) (seg src) (long x) (long y) (int (if blend 1 0)))))
     (fail! :paste))
   dst))

(defn get-pixel
  "Packed `0xRRGGBBAA` of one pixel, or nil when out of bounds."
  [img x y]
  (let [v (long (invoke :get-pixel (seg img) (int x) (int y)))]
    (when-not (neg? v) v)))

(defn put-pixel! [img x y c]
  (when (neg? (int (invoke :put-pixel (seg img) (int x) (int y) (unchecked-int (color c)))))
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
                        (assoc :image (str "handle:" (.address ^MemorySegment (seg (:image op))))))))]
    (with-open [arena (Arena/ofConfined)]
      (when (neg? (int (invoke :draw (seg img)
                               (.allocateFrom arena (->json {:ops (mapv prepare ops)})))))
        (fail! :draw)))
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
   `:background` `:dpi` `:font-family` `:font-size` `:base-url`."
  (^Img [svg] (render-svg svg nil))
  (^Img [svg opts]
   (with-open [arena (Arena/ofConfined)]
     (let [^bytes b (->bytes svg)]
       (img! (invoke :svg-render (alloc-bytes arena b) (long (alength b))
                     (json-arg arena (color-opt opts [:background])))
             :svg-render)))))

(defn plot
  "Render a plot specification to an image. See `com.blockether.imaging.plot` for
   the spec builders (`line`, `bar`, `scatter`, `hist`, `pie`, `heatmap`, …) and
   the full vocabulary."
  ^Img [spec]
  (with-open [arena (Arena/ofConfined)]
    (img! (invoke :plot (.allocateFrom arena (->json spec))) :plot)))
