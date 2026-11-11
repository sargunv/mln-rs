// The map's Web Worker: its own instance of the page's module, rendering
// into the page's canvas (an OffscreenCanvas), so that the map's work never
// runs on the page's thread. Its tile workers (worker.js) talk to it
// directly, each over a MessageChannel port the page connected: requests
// go as bytes, and replies come back as bytes and payloads, transferred.
//
// From the page: first {module, canvas, ports, width, height, ratio, style,
// camera, localIdeographFontFamily}, then {readPixels}, {resize: [width, height]} in CSS pixels, and
// {input, args}: a call of WebMap's input methods (pointer_down,
// pointer_move, pointer_up, pointer_cancel, wheel, key, blur) whose last
// argument is the event's time as performance.timeOrigin + timeStamp. To the
// page: {status}, {loaded: {loadedMs, replies}} once the map is fully
// loaded, {pixels: {width, height, pixels}} for each readPixels, and {box}
// when a Shift-drag's box changes ([x0, y0, x1, y1], or null).

import init, { WebMap } from './pkg/mln_web.js';

let start = null;
const early = []; // messages before the map exists
onmessage = ({ data }) => {
  if (data.module) start = run(data);
  else if (start) start.then((handle) => handle(data));
  else early.push(data);
};

async function run({ module, canvas, ports, width, height, ratio, style, fallbackFontFaces, camera, localIdeographFontFamily }) {
  await init({ module_or_path: module });

  // The map's work between frames (finished fetches, worker replies, queued
  // tasks) runs as soon as the map asks for it, in a task of its own, rather
  // than waiting for the next animation frame: each step of a load (a tile's
  // response, its parsing, the next request) would otherwise wait a frame.
  const ticks = new MessageChannel();
  let tickScheduled = false;
  const wake = () => {
    if (!tickScheduled) {
      tickScheduled = true;
      ticks.port2.postMessage(null);
    }
  };
  // A style's font faces are shaped by a second module, which only this
  // worker loads, the first time a style declares them.
  const loadShaper = async () => {
    const shaper = await import('./pkg/mln_web_shaper.js');
    await shaper.default();
    return shaper;
  };
  const map = await WebMap.create(canvas, width, height, ratio, ports.length,
    (instance, bytes) => ports[instance].postMessage(bytes, [bytes.buffer]), wake, loadShaper,
    localIdeographFontFamily);

  ticks.port1.onmessage = () => {
    tickScheduled = false;
    if (map.tick(4)) wake();
  };
  ports.forEach((port, instance) => {
    port.onmessage = ({ data: replies }) => {
      for (const reply of replies) map.receive(instance, reply);
    };
  });

  if (fallbackFontFaces) map.set_fallback_font_faces(fallbackFontFaces);
  map.set_style_url(style);
  map.jump_to(...camera);
  const started = performance.now();

  let loaded = null;
  let readPixels = 0;
  let reported = 0;
  const report = (force) => {
    const now = performance.now();
    if (!force && now - reported < 250) return;
    reported = now;
    postMessage({
      status: `${ports.length} tile workers, ${map.received()} replies` +
        (loaded === null ? '' : `, loaded in ${Math.round(loaded.loadedMs)} ms`),
    });
  };
  function frame() {
    map.frame(8, performance.now());
    if (loaded === null && map.is_fully_loaded()) {
      loaded = { loadedMs: performance.now() - started, replies: map.received() };
      postMessage({ loaded });
      report(true);
    }
    if (readPixels > 0) {
      const pixels = map.take_pixels();
      if (pixels !== undefined) {
        readPixels--;
        postMessage({ pixels: { width: canvas.width, height: canvas.height, pixels } }, [pixels.buffer]);
        if (readPixels > 0) map.request_pixels();
      }
    }
    report(false);
    requestAnimationFrame(frame);
  }
  requestAnimationFrame(frame);

  // The page's events, on this worker's clock: the one frames run on.
  const INPUTS = new Set(['pointer_down', 'pointer_move', 'pointer_up', 'pointer_cancel', 'wheel', 'key', 'blur']);
  let box = null;
  const handle = (data) => {
    if (data.readPixels) {
      if (readPixels++ === 0) map.request_pixels();
    }
    if (data.resize) map.resize(...data.resize);
    if (INPUTS.has(data.input)) {
      const args = data.args.slice();
      args[args.length - 1] -= performance.timeOrigin;
      map[data.input](...args);
      const next = map.box_zoom() ?? null;
      if (String(next) !== String(box)) {
        box = next && Array.from(next);
        postMessage({ box });
      }
    }
  };
  for (const data of early.splice(0)) handle(data);
  return handle;
}
