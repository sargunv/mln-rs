# mln-rs-bench

The benchmarks of mln-rs, and a comparison of two commits that says which
changes are real.

```sh
just bench                      # every case, once
just bench --filter Frame       # the cases whose name contains "Frame"
just bench-compare main         # this checkout, uncommitted changes included, against a Git ref
```

## Data

- The shared render suite's real-world styles (Chicago, San Francisco,
  Norway) at their tests' cameras, 512 x 512, read from its cache database.
- maplibre-native-ffi's basemap: VersaTiles' Shortbread tiles over central
  San Francisco (a 5 x 5 block at zoom 14) with Noto Sans glyphs and a
  generated sprite, 1024 x 768. `fixtures.json` and `style.json` are
  maplibre-native-ffi's, and the cases below that share its names use its
  cameras, so their numbers compare with the C++ core's. `just
  bench-fixtures` (`fixtures.py`) downloads the pinned files into
  `target/bench-fixtures` once; nothing else uses the network.

## Cases

| case | what one iteration is |
|---|---|
| `Still/<scene>` | a still frame of a loaded map, read back; the zoom alternates by 0.001, so each is a new frame over the same tiles |
| `Load/Style/<scene>` | from loading the style on a new map to its first complete still; the GPU device is set up before |
| `Frame/Throughput` | a camera jump around a 200-pixel circle until the GPU has finished the frame that shows it, over tiles an untimed lap loaded |
| `Frame/Paced` | a two-second flight from the block's north-west corner at zoom 14 to its south-east corner at zoom 15.5, past the tiles' zoom, on a 60 Hz display's clock, with only the first view loaded |
| `Frame/Idle` | 100 ms of a settled map whose run loop draws whenever the map asks |
| `Frame/SettleWithoutSymbols` | from a camera nudge on the basemap with its symbol layers left out, which has nothing to place, until the map stops asking for frames |
| `Load/Concurrent` | 16 renderers on threads of their own, each from loading a real-world style on a new map to its first complete still through `HeadlessFrontend::render`, as the render tests' workers draw; the GPU devices are set up before |
| `Load/JumpWarm`, `Load/JumpCold` | a jump between two views that share no tile until a frame shows the new view complete, on the display's clock; cold drops the loaded tiles first |

In `Frame/Paced`, refreshes are absolute deadlines, and at each refresh
with no frame in flight the camera moves to where the flight will be at the
next refresh, so every run draws the same cameras however slow it is. A
frame is shown at the first refresh after the GPU finishes it, and the next
frame starts there.

## Metrics

All are lower-is-better. A comparison gives a verdict on a case's **lead**
metrics only; `--verbose` also shows the others.

- **`time_ms`**, **`cpu_ms`**: an iteration's wall time, and the CPU time of
  every thread of the process, so work moved to the tile workers or a busy
  wait still counts. The median of the case's iterations.
- **`allocs`**, `alloc_kib`: heap allocations per iteration, of the whole
  process (the GPU driver's own allocations aren't Rust's and aren't
  counted).
- **`peak_heap_mib`**: the most heap live at once during the case.
- **`frame_p95_ms`**, **`frame_p99_ms`**: frame times, from asking for a
  frame until the GPU has finished it.
- In `Frame/Paced`: **`missed_pct`**, frames that finished after the next
  refresh; **`partial_pct`**, frames that drew a view with tiles still
  loading; **`longest_stall_ms`**, the longest time between two shown
  frames; **`complete_after_ms`**, how long after the flight's end the final
  view shows complete.
- In `Frame/Idle`: **`frames_per_s`**, frames drawn while nothing changes.
- In `Frame/SettleWithoutSymbols`: **`frames`**, frames drawn until the map stops asking for them.

## Comparing

`just bench-compare <ref>` checks the ref out in a worktree under
`target/bench-compare`, which the next comparison reuses, lays this
checkout's `tools/bench` over it so both sides run the same cases, and
builds it there. Then it runs one `mln-bench run` process per side per
round, in ABBA order so that drift hits both sides alike: `--rounds` (6)
processes per side. A process's value of a metric is one sample, since
iterations in one process share its caches and heap.

A lead metric gets a verdict when a two-sided Mann-Whitney U test,
Holm-corrected over the case's lead metrics, gives p < 0.05 and the medians
differ by at least `--threshold` percent (5). There are no verdicts when the
two sides ran on different GPUs. Then it compares the last frames of the
first round with pixelmatch, as the render tests do, and writes the
differences to `target/bench-compare/frames/diff`, so a side that got faster
by drawing less shows up.

Compare on an idle machine with a GPU; the report shows the load average
before and after, and a software rasterizer (lavapipe) is flagged. To see
how much your machine's numbers vary, compare a commit with itself, which
should give no verdicts.

## Not measured yet

- GPU time apart from the frame (timestamp queries), and the frame's CPU
  time by stage: the port doesn't keep `RenderingStats` yet.
- A synthetic clock for animations: in continuous mode the map reads the
  real clock, so fades and transitions in `Frame/Paced` follow wall time.
