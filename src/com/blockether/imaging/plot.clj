(ns com.blockether.imaging.plot
  "A small, opinionated plotting DSL over `com.blockether.imaging/plot`.

   The renderer lives in the Rust cdylib (`native/imaging-c/src/plot.rs`, drawn
   with tiny-skia and text-shaped through usvg), so plotting needs NO
   `java.desktop`, no headless AWT and no fonts on the host — four Noto faces are
   embedded in the library.

   Everything here is DATA: a figure is a map, a panel is a map, a series is a
   map. This namespace only builds those maps (and hands them to the renderer),
   so anything the DSL doesn't wrap yet you can still express by assoc'ing the
   raw key — unknown keys are passed straight through to the renderer.

   ```clojure
   (require '[com.blockether.imaging.plot :as plot])

   (plot/save! \"cpu.png\"
     (plot/figure {:title \"CPU\" :theme :dark :width 900 :height 400}
       (plot/lines {:x-label \"minute\" :y-label \"%\"}
         {:name \"user\" :x (range 60) :y user-samples}
         {:name \"sys\"  :x (range 60) :y sys-samples})))
   ```

   Panel constructors: `lines` `areas` `bars` `hbars` `stacked-bars` `scatter`
   `steps` `histogram` `pie` `donut` `heatmap`. Each takes an optional option map
   first, then series maps (or plain sequences of numbers). `figure` composes
   panels into a grid; a bare panel is itself a valid figure."
  (:refer-clojure :exclude [bytes])
  (:require [com.blockether.imaging :as img]))

;; ---------------------------------------------------------------------------
;; Series + panels
;; ---------------------------------------------------------------------------

(defn- series
  "Normalise one series argument. A bare sequence of numbers is `:y`; a map is
   taken as-is (`:x`/`:y`, or `:values`/`:labels` for pie & friends)."
  [s]
  (cond
    (map? s)        s
    (sequential? s) {:y (vec s)}
    (nil? s)        nil
    :else           (throw (ex-info "plot: not a series" {:series s}))))

(def ^:private series-keys
  "A leading map is the panel's OPTIONS unless it carries data — then it is the
   first series. `:labels` is deliberately NOT here: it names the categories of
   the whole panel far more often than of one series."
  #{:y :x :values :z :name})

(defn- panel
  "A panel of `type` from an optional leading option map plus series."
  [type args]
  (let [[opts args] (if (and (map? (first args))
                             (not-any? #(contains? (first args) %) series-keys))
                      [(first args) (rest args)]
                      [{} args])]
    (cond-> (assoc opts :type type)
      (seq args) (assoc :series (mapv series args)))))

(defn lines
  "Line panel. `(lines {:y-label \"ms\"} {:name \"p50\" :x xs :y ys} …)`.
   Series keys: `:name` `:x` `:y` `:color` `:dash` `:width` `:marker`
   `:marker-size` `:fill-alpha`."
  [& args] (panel "line" args))

(defn areas
  "Filled area panel (add `:stack true` to stack the series)."
  [& args] (panel "area" args))

(defn steps
  "Step / staircase panel — the honest shape for sampled counters."
  [& args] (panel "step" args))

(defn bars
  "Vertical bar panel. `:labels` on the panel (or `:x` on a series) names the
   categories; `:bar-width` 0-1, `:bar-radius` px, `:labels-on true` prints the
   value above each bar."
  [& args] (panel "bar" args))

(defn hbars
  "Horizontal bar panel — the readable choice when category names are long."
  [& args] (panel "barh" args))

(defn stacked-bars
  "Bar panel with the series stacked instead of grouped."
  [& args] (assoc (panel "bar" args) :stack true))

(defn scatter
  "Scatter panel. Series `:marker` ∈ #{\"circle\" \"square\" \"diamond\"
   \"triangle\" \"cross\"}, `:marker-size` px, `:z` a per-point weight."
  [& args] (panel "scatter" args))

(defn histogram
  "Histogram of raw samples; `:bins` (default auto)."
  [& args] (panel "hist" args))

(defn pie
  "Pie chart from `:values` + `:labels` (`:labels-on` prints percentages)."
  [& args] (panel "pie" args))

(defn donut
  "Pie with a hole (`:hole` 0-1, `:center-label` in the middle)."
  [& args] (panel "donut" args))

(defn heatmap
  "Matrix heat map: a series `:z` of rows, `:colormap` ∈ #{\"viridis\" \"magma\"
   \"inferno\" \"plasma\" \"coolwarm\" \"blues\" \"reds\" \"greys\"}."
  [& args] (panel "heatmap" args))

;; ---------------------------------------------------------------------------
;; Figures
;; ---------------------------------------------------------------------------

(defn figure
  "Compose panels into one figure spec.

   Figure options: `:width` `:height` (px), `:title` `:subtitle` `:caption`,
   `:theme` ∈ #{:light :dark :night}, `:background`, `:palette` (vector of
   colours), `:font-family` `:font-size`, `:legend` (`true`/`false`/`\"top\"`/
   `\"right\"`), `:grid`, `:rows`/`:cols` for the panel grid."
  [opts & panels]
  (let [[opts panels] (if (or (nil? opts) (and (map? opts) (not (:type opts))))
                        [(or opts {}) panels]
                        [{} (cons opts panels)])
        panels        (vec (remove nil? panels))]
    (assoc opts :panels panels)))

(defn- ->spec [x]
  (cond
    (:panels x) x
    (:type x)   (figure {} x)
    :else       (throw (ex-info "plot: not a figure or panel" {:spec x}))))

(defn render
  "Render a figure (or a bare panel) to an `Img` handle."
  ([fig] (render fig nil))
  ([fig opts] (img/plot (merge (->spec fig) opts))))

(defn bytes
  "Render straight to encoded image bytes (PNG by default).
   `(bytes fig {:format \"webp\" :quality 90})`"
  (^bytes [fig] (bytes fig nil))
  (^bytes [fig {:keys [format quality] :or {format "png" quality 0} :as opts}]
   (with-open [im (render fig (dissoc opts :format :quality))]
     (img/encode im format quality))))

(defn save!
  "Render a figure and write it to `path`; the format comes from the extension.
   Returns `path`."
  ([path fig] (save! path fig nil))
  ([path fig opts]
   (with-open [im (render fig opts)]
     (img/save! im path))
   path))
