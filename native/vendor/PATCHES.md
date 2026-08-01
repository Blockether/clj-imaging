# Vendored crates

## `rust_h264` 0.4.0

Vendored from crates.io (MIT OR Apache-2.0, upstream
<https://github.com/roticv/rust_h264>) because 0.4.0 — the latest release —
decodes the wrong picture for a large class of ordinary H.264 streams.

### The bug

`Dpb::short_term_ref_list` built ref_pic_list_0 for P slices by sorting the
short-term references by **descending `pic_order_cnt`**, documented as "a proxy
for recency (handles frame_num wraparound)".

It does not handle wraparound. When `pic_order_cnt_type == 2` — which x264
writes whenever B-frames are disabled — POC is derived from `frame_num` and
wraps with it at `MaxFrameNum`. x264 also writes `log2_max_frame_num_minus4 == 0`,
so `MaxFrameNum == 16`: from the 17th picture of a GOP onward, descending POC
puts the *oldest* reference at index 0. Every macroblock coded with
`ref_idx_l0 > 0` then predicts from the wrong picture, and the error accumulates
for the rest of the GOP.

Measured against ffmpeg on 30 clips: identical output for the first 17 frames,
then mean absolute error rising to ~7/255 over the whole frame. It is invisible
when `refs == 1` (only one candidate) and when the encoder emits explicit ref
list reordering commands (x264 does that for `weightp`), which is why it can
survive casual testing.

### The fix

`short_term_ref_list` now takes the current `frame_num` and `MaxFrameNum` and
sorts by descending `PicNum`, i.e. by `FrameNumWrap` as defined in spec 8.2.4.1:

```text
FrameNumWrap = if frame_num > current_frame_num { frame_num - MaxFrameNum }
               else                             { frame_num }
```

Changed files:

- `src/dpb.rs` — `short_term_ref_list` signature and sort key, new private
  `Dpb::frame_num_wrap`, callers in the crate's own unit tests, plus a new
  `test_ref_list_after_frame_num_wrap` regression test.
- `src/decoder.rs` — the single call site passes `header.frame_num` and the
  already-computed `max_pic_num`.

Nothing else is modified; the crate's own test suite passes unchanged otherwise.

### Proof the patch is load-bearing

`test/fixtures/refcycle27.mp4` is a lossless (`-qp 0`) 27-frame clip of three
distinct noise images cycled A,B,C,A,B,C…, encoded `-refs 3 -bf 0 weightp=0` in a
single GOP. Noise is expensive to intra-code, so every P picture predicts from
the matching picture three frames back — through `ref_idx_l0 > 0` — and lossless
coding means decoded frame `i` must come back byte-identical to frame `i % 3`. No
reference decoder is needed to check that.

It is asserted by `video::tests::picks_the_right_reference_past_the_frame_num_wrap`
(`imaging-c`) and, over the same fixture through the public API,
`decode-video-picks-the-right-reference-past-the-frame-num-wrap` (Clojure).
Restoring the upstream sort key and running `cargo test` in `imaging-c` fails the
Rust one at frame 17 — the first picture past the wrap — with 12223 of that
frame's 16384 RGBA bytes wrong.

### Known remaining upstream issue (not hit by us, not patched)

`decoder.rs` MMCO ops 1 and 3 compute `frame_num - (abs_diff + 1)` in `i32` and
cast a possibly negative result to `u32` instead of doing the modular
`(frame_num + MaxFrameNum - (abs_diff + 1)) % MaxFrameNum` the spec requires, so
an adaptive-marking stream that wraps would fail to unmark a reference. Fixing
it needs `MaxFrameNum` plumbed into `PictureState`, and no encoder we test emits
those ops. Left alone deliberately.

### Upgrading

If upstream releases a version with the ref-list fix, drop this directory and
restore the plain `rust_h264 = "x.y"` dependency in `imaging-c/Cargo.toml`.
