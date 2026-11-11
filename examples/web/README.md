# mln-rs in the browser

The browser build: the libraries built for wasm32-unknown-unknown (no
atomics, no shared memory) and driven from JavaScript through wasm-bindgen.
It needs no COOP/COEP headers. The page compiles one wasm module, and Web
Workers instantiate it, each instance with its own memory; the map's Web
Worker loads a second, the font-face shaper, only for styles with font
faces. The page's thread runs no wasm: it hands its canvas to the map's Web
Worker and only forwards to it.

- in the map's Web Worker (web/map-worker.js), `WebMap` (src/map.rs): the
  map, its renderer (wgpu's WebGPU backend, into the page's canvas as an
  OffscreenCanvas) and its file sources. The
  instance has no threads, so it installs a `CooperativeScheduler`. Each
  animation frame runs the RunLoop, draws, and runs the scheduler's tasks
  until the frame's budget is spent. Between frames the RunLoop and the
  scheduler wake the worker (`set_waker`), which runs what arrived in a
  task of its own (`WebMap::tick`): a finished fetch or a worker's reply
  is handled at once, rather than at the next frame, so that a load's
  steps don't each wait for a frame.
- in each tile worker's Web Worker (web/worker.js), `TileWorker`
  (src/worker.rs): a `TileWorkerHost` on the instance's own
  `CooperativeScheduler`. The map's `SerializingTransport` gives each
  tile's worker to one of the Web Workers and sends every message between
  the two as bytes (mln_renderer's `tile::worker_codec`), over a
  MessageChannel that the page connects between the two Web Workers. So
  every vector and GeoJSON tile parses and lays out its buckets (symbols
  included) there, and every raster and raster-dem tile decodes its image
  there (the map gets back the pixels, and a DEM with the border it
  backfills from the neighbouring tiles). A reply's large arrays (pixels,
  vertices, indices) travel beside its bytes as their own buffers, which
  the Web Worker transfers, and which the map copies once, into the
  arrays its buckets keep.
- in the map's Web Worker again, the first time a style declares font
  faces (`"font-faces"`): the shaper module
  ([crates/mln_web_shaper](../../crates/mln_web_shaper)), mln_text's
  `HBShaper` (HarfRust, skrifa and the FreeType port), which shapes and
  rasterizes them. See below.

## Font faces in a second module

The map shapes a style's font faces (complex scripts such as Devanagari,
and right-to-left runs) as natively, with HarfRust, and rasterizes their
glyphs with skrifa and the FreeType port. That code is a large share of
the module, and most styles have no font faces, so it is in a module of its
own: the main module is built without mln_renderer's `font-face-shaper`
feature, and its map's `FontFaceEngine` (src/shaper.rs) imports
web/pkg/mln_web_shaper.js the first time a style declares font faces
(`GlyphManager::set_font_faces`), while their files download. A font file
that arrives first waits in the `GlyphManager` until the module is ready,
and the tiles that need it wait for it as for any glyph range in flight.
Then the map calls the module synchronously, through JavaScript: it opens
each font file there (a copy of it in that instance's memory), shapes the
tiles' text and rasterizes glyphs, and turns the glyphs into SDFs itself.
Tile workers never load it: the map shapes for them.

A style with font faces downloads and compiles the shaper once more. A
panic in the shaper traps its instance; each call from the map catches the
trap as an error, so the map logs it and leaves the text out, rather than
unwinding through its own state.

The page forwards the canvas's input to the map's Web Worker: pointer
events (mouse, pen and touch, with pointer capture; `pointerrawupdate`
where the browser has it, so that a move reaches the worker without
waiting for the page's next frame), wheel, the keys the map uses, and
focus loss. `WebMap` feeds them to a gesture recognizer (crates/mln_gestures),
which moves the camera and continues each release with the map's momentum.
Each event's time travels as `performance.timeOrigin + timeStamp`, and the
worker turns it into its own `performance.now()`, the clock it gives
`WebMap::frame`, which also puts the map's animations on the frame clock.
The page tells a mouse wheel's notches, which the recognizer eases, from a
trackpad's continuous pixels, which it applies at once, by how browsers
report each (`wheelDelta` in web/main.js: Firefox's lines, Safari's
4.000244 px steps, Chromium's legacy `wheelDeltaY`); those rules aren't
checked on real hardware yet. A ResizeObserver resizes the map with the
canvas; the map keeps the pixel ratio it started with.

Controls: drag to pan; right- or Ctrl-drag to rotate and tilt; Shift-drag
to zoom to a box (Escape cancels it); the wheel (a third of a level a
notch, eased) or a trackpad to zoom, Ctrl or a trackpad pinch faster;
double-click or double-tap to zoom in, Shift-double-click or a two-finger
tap to zoom out, double-tap-and-drag to zoom; two fingers to pinch, twist
and tilt; the arrow keys to pan (Shift: rotate and tilt) and `+`/`=` and
`-` to zoom, once the map has focus.

## Images decode in the browser

Every instance decodes images with the browser's decoders (mln_image's
default on wasm), so the module carries no PNG or WebP decoder. The decoder
answers later, from the event loop, so a tile worker's reply can come
outside `TileWorker::run`; it then wakes web/worker.js for a run.

## CJK glyphs are drawn locally

As MapLibre GL JS does with its `localIdeographFontFamily`, the map draws
CJK ideographs, kana, hangul and the related blocks (mln_util's
`allows_fixed_width_glyph_generation`) in a local font, rather than
downloading the style's glyph ranges for them, which are most of a CJK
label's glyphs. mln_text's `DefaultLocalGlyphRasterizer` draws each in the
map's Web Worker with an `OffscreenCanvas`, and the `GlyphManager` turns it
into an SDF. The check turns it off, as GL JS's render tests do, so that
its frames match the native renders.

## Caching

The browser's HTTP cache caches the map's resources, as in MapLibre GL
JS: every request goes through `fetch` (mln_storage's default on wasm), and the build
leaves out SQLite (mln_storage's `sqlite` feature), so there is no
ambient cache database (`ResourceOptions::cache_path`) and no offline
packs. Without it the map's file source serves no cache-only requests,
so the renderer doesn't load the optional tiles it would show from the
cache while their parents load, as the C++ core built without a
database, and `mbtiles://` URLs don't resolve (the browser has no files
to open). TODO: offline packs on the web, which could run SQLite on OPFS
in a storage worker loaded only when a page asks for them.

## Build and run

`wasm-bindgen-cli` must be the version in Cargo.lock, and a release build
also runs binaryen's `wasm-opt` (its latest release), which shrinks the code with
`-Oz` and strips the names and the other custom sections:

```sh
cargo install wasm-bindgen-cli --version 0.2.130
mise use -g aqua:WebAssembly/binaryen@latest  # or binaryen's release, on the PATH
just profile=release web
python3 -m http.server -d examples/web/web
```

`just web` (the dev profile) skips wasm-opt and keeps the names, for
debugging and profiling.

Then open <http://localhost:8000/> in a browser with WebGPU. URL parameters
(web/main.js):

- `style=<url>` (default: OpenFreeMap Liberty), `lat`, `lon`, `zoom`,
  `bearing`, `pitch`;
- `fallbackFontFaces=<json>`: font files that every font stack falls back
  to, in the format of a style's `font-faces` entry without a font name
  (`[{"url": "...", "unicode-range": ["U+1000-109F"]}]`);
- `workers=<n>`: Web Workers for the tiles (default: up to 4);
- `width`, `height`, `ratio`: a fixed canvas size in CSS pixels and pixel
  ratio;
- `localIdeographFontFamily=<CSS font-family list>` (default: `sans-serif`):
  see below; empty or `false` turns it off.

## A panic stops a Web Worker

wasm32-unknown-unknown aborts on a panic, so the tile workers'
`catch_unwind`, which natively turns a panic while parsing (a malformed
vector tile, MVT or MLT) into an error for its tile, as C++ `catch (...)`
does, doesn't run in the browser. The panic aborts the Web Worker's wasm
instance instead: web/worker.js logs the instance as dead and drops its
requests, and every tile whose worker it hosts stays pending. TODO(port):
build with `panic=unwind` and wasm exception handling, or respawn the
instance and create its tiles' workers again.

## Check

`just profile=release web-check` (check/check.mjs, Node 24 with no
packages) loads views of the render suite in headless Chromium, offline
(Chromium resolves no host but the local server's), and compares each
with the native render-test runner's render of the same view. CI runs it
(the `web` job), the same way: install Playwright's Chromium once with
`npx playwright@1.61.1 install chromium` (the check uses that build), and
keep lavapipe for the native runner (`VK_DRIVER_FILES`).

- The views are render tests of the suite whose operations the page can
  follow (check.mjs, `VIEWS`): five real-world styles (vector tiles with
  fills, lines, icons and labels), raster tiles, raster-dem tiles
  (hillshade and color relief), raster with GeoJSON, and labels in font
  faces (Devanagari, Khmer, and Arabic beside Devanagari), which load the
  shaper module. In the raster
  and raster-dem views only the raster tiles' workers can reply, so their
  replies show that those tiles decode in the Web Workers.
- A local server serves the page and every fixture from the suite's
  cache database (tools/render-test/testdata/metrics/cache-style.db), with
  the URLs that the runner's file source maps (`local://`, `mapbox://`,
  ...) rewritten to it. What the cache lacks gets no content, as in the
  runner.
- Once a view is fully loaded and its labels have faded in, the page
  reads its frame back from the GPU: headless Chromium composites WebGPU
  canvases as white, so a screenshot shows nothing.
  `WebMap::request_pixels` draws the frame again and copies the canvas
  texture into a buffer, which the event loop maps
  (`SurfaceBackend::take_readback`).
- The frames become the `expected.png` of render tests of the same views,
  which the native runner renders from the same cache database and
  compares with pixelmatch, as the render tests do.

It fails when more than 0.1% of a view's pixels differ (the combination
of raster-dem and vector at z1: 1%, where SwiftShader and lavapipe
antialias coastlines differently), when the page or a Web Worker logs an
error or throws (a worker that panics, a tile that fails to load), or
when a view gets no worker replies. The page renders on SwiftShader, which
Chromium bundles, and most views come out identical to lavapipe's;
leaving out one layer of labels changes 0.1% to 2% of the pixels.

Options: `--filter <regex>` picks views, `--workers <n>` sets the page's
Web Workers (default 2), `--codec <n>` renders natively through n codec
instances (`--tileWorkerCodec`), `--out <dir>` keeps the frames, the
native renders and their diffs, `--chromium <path>` uses another
Chromium. `MLN_WEB_CHECK_LOG=1` prints the consoles of the page and its
workers, and the fixtures the cache lacks.
