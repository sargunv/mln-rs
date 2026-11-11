// The page: it runs no wasm itself. It compiles the module, gives its canvas
// to the map's Web Worker (map-worker.js) as an OffscreenCanvas, and starts
// the tile workers' Web Workers (worker.js), each connected to the map's by
// a MessageChannel, so that their requests and replies never pass through
// this thread. Each Web Worker instantiates the same module with its own
// memory, so nothing needs SharedArrayBuffer or COOP/COEP headers.
//
// URL parameters:
//   style=<url>            the style (default: OpenFreeMap Liberty)
//   fallbackFontFaces=<json>
//                          font files that every font stack falls back to, as
//                          in a style's font-faces entry without a font name:
//                          [{"url": "...", "unicode-range": ["U+1000-109F"]}]
//   lat, lon, zoom, bearing, pitch
//   workers=<n>            Web Workers for the tiles (default: up to 4)
//   width, height, ratio   a fixed canvas size in CSS pixels and pixel ratio
//                          (default: the window and devicePixelRatio)
//
// Input: the canvas's pointer, wheel, key and focus events go to the map's
// Web Worker, whose gesture recognizer (mln_gestures) moves the camera: drag
// to pan, right- or Ctrl-drag to rotate and tilt, Shift-drag to zoom to a
// box, the wheel or a trackpad to zoom, double-click or double-tap to zoom
// in (Shift: out), two fingers to pinch, twist and tilt, and the arrow,
// +/= and - keys. Each event carries its time as performance.timeOrigin +
// timeStamp, which the worker turns into its own performance.now() clock,
// the one its frames run on. The canvas follows its size (ResizeObserver).
//
// `window.mln` is for scripts (check/check.mjs): `loaded` resolves with the
// load time and the number of worker replies once the map is fully loaded,
// and `readPixels()` draws the frame again and resolves with its RGBA
// pixels, read back from the GPU.

const params = new URLSearchParams(location.search);
const number = (name, fallback) => (params.has(name) ? Number(params.get(name)) : fallback);
const status = document.getElementById('status');
const canvas = document.getElementById('map');

if (params.has('width') && params.has('height')) {
  canvas.style.width = `${number('width')}px`;
  canvas.style.height = `${number('height')}px`;
}
const ratio = number('ratio', window.devicePixelRatio || 1);
canvas.width = Math.round(canvas.clientWidth * ratio);
canvas.height = Math.round(canvas.clientHeight * ratio);

const module = await WebAssembly.compileStreaming(fetch('./pkg/mln_web_bg.wasm'));

const count = number('workers', Math.max(1, Math.min(4, (navigator.hardwareConcurrency || 2) - 1)));
const ports = Array.from({ length: count }, () => {
  const channel = new MessageChannel();
  const worker = new Worker('./worker.js', { type: 'module' });
  worker.postMessage({ module, port: channel.port2 }, [channel.port2]);
  return channel.port1;
});

const map = new Worker('./map-worker.js', { type: 'module' });
const offscreen = canvas.transferControlToOffscreen();
map.postMessage({
  module,
  canvas: offscreen,
  ports,
  width: canvas.clientWidth,
  height: canvas.clientHeight,
  ratio,
  style: new URL(params.get('style') ?? 'https://tiles.openfreemap.org/styles/liberty', location.href).href,
  fallbackFontFaces: params.get('fallbackFontFaces'),
  camera: [number('lat', 52.37), number('lon', 4.89), number('zoom', 13), number('bearing', 0), number('pitch', 0)],
}, [offscreen, ...ports]);

let markLoaded;
const pixelRequests = [];
window.mln = {
  loaded: new Promise((resolve) => { markLoaded = resolve; }),
  readPixels() {
    return new Promise((resolve) => {
      pixelRequests.push(resolve);
      map.postMessage({ readPixels: true });
    });
  },
};

map.onmessage = ({ data }) => {
  if (data.box !== undefined) showBox(data.box);
  if (data.status !== undefined) status.textContent = data.status;
  if (data.loaded) markLoaded(data.loaded);
  if (data.pixels) pixelRequests.shift()?.(data.pixels);
};

// The map's input. Positions are CSS pixels from the canvas's top-left
// corner; modifiers are bits: Shift 1, Ctrl 2, Alt 4, Meta 8.
const send = (input, ...args) => map.postMessage({ input, args });
const time = (e) => performance.timeOrigin + e.timeStamp;
const modifiers = (e) => (e.shiftKey ? 1 : 0) | (e.ctrlKey ? 2 : 0) | (e.altKey ? 4 : 0) | (e.metaKey ? 8 : 0);
// The keys the map uses; Escape only ends a box zoom, so its default stays.
const KEYS = new Set(['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', '+', '=', '-', '_']);
// Where the canvas is: read on each press and resize rather than on every
// move, which would lay the page out again.
let origin = canvas.getBoundingClientRect();
const at = (e) => [e.clientX - origin.left, e.clientY - origin.top];

canvas.tabIndex = 0;
canvas.style.touchAction = 'none';
canvas.style.outline = 'none';
canvas.addEventListener('pointerdown', (e) => {
  canvas.focus({ preventScroll: true });
  origin = canvas.getBoundingClientRect();
  send('pointer_down', e.pointerId, e.pointerType, e.button, ...at(e), modifiers(e), time(e));
  // The map drags with the primary and secondary buttons.
  if (e.button === 0 || e.button === 2) canvas.setPointerCapture(e.pointerId);
});
// In a worker, the map draws as soon as a move arrives: pointerrawupdate
// comes when the browser has the move, pointermove only with the page's
// next frame.
const moveEvent = 'onpointerrawupdate' in HTMLElement.prototype ? 'pointerrawupdate' : 'pointermove';
canvas.addEventListener(moveEvent, (e) => send('pointer_move', e.pointerId, ...at(e), time(e)));
canvas.addEventListener('pointerup', (e) => send('pointer_up', e.pointerId, ...at(e), time(e)));
canvas.addEventListener('pointercancel', (e) => send('pointer_cancel', e.pointerId, time(e)));
// A wheel event as the recognizer takes it (WebMap::wheel's `kind`):
// [WHEEL_NOTCHES, notches] for a mouse wheel's detents, which it eases, or
// [WHEEL_PIXELS, pixels] for continuous scrolling (a trackpad, a precise
// wheel), which it applies at once. Positive scrolls down. Browsers don't
// say which device scrolled, so this reads it from how they report each:
// - Firefox reports wheels in lines, 3 a notch, and pages.
// - Safari reports a notch as 4.000244140625 px, and a faster turn as a
//   whole multiple of it (GL JS's test): one notch per multiple.
// - Chromium's legacy wheelDeltaY is 120 a detent, and -3 × deltaY for
//   precise scrolling.
// Untested on real hardware; a misread zooms the same way by a similar
// amount, with or without the easing.
const WHEEL_PIXELS = 0;
const WHEEL_NOTCHES = 1;
const SAFARI_NOTCH = 4.000244140625;
function wheelDelta(e) {
  if (e.deltaMode === WheelEvent.DOM_DELTA_LINE) return [WHEEL_NOTCHES, e.deltaY / 3];
  if (e.deltaMode === WheelEvent.DOM_DELTA_PAGE) return [WHEEL_NOTCHES, e.deltaY];
  if (e.deltaY !== 0 && e.deltaY % SAFARI_NOTCH === 0) return [WHEEL_NOTCHES, e.deltaY / SAFARI_NOTCH];
  const legacy = e.wheelDeltaY;
  if (typeof legacy === 'number' && legacy !== 0 && Math.abs(legacy + 3 * e.deltaY) >= 1 && legacy % 120 === 0) {
    return [WHEEL_NOTCHES, -legacy / 120];
  }
  return [WHEEL_PIXELS, e.deltaY];
}
canvas.addEventListener('wheel', (e) => {
  e.preventDefault();
  if (e.deltaY !== 0) send('wheel', ...at(e), ...wheelDelta(e), modifiers(e), time(e));
}, { passive: false });
canvas.addEventListener('keydown', (e) => {
  if (KEYS.has(e.key)) e.preventDefault();
  if (KEYS.has(e.key) || e.key === 'Escape') send('key', e.key, modifiers(e), time(e));
});
canvas.addEventListener('blur', (e) => send('blur', time(e)));
canvas.addEventListener('contextmenu', (e) => e.preventDefault());

// A Shift-drag's box, which the worker reports as [x0, y0, x1, y1] while it
// is drawn, or null.
const box = document.createElement('div');
box.id = 'box';
canvas.after(box);
function showBox(corners) {
  if (!corners) {
    box.style.display = 'none';
    return;
  }
  const [x0, y0, x1, y1] = corners;
  Object.assign(box.style, {
    display: 'block',
    left: `${origin.left + window.scrollX + Math.min(x0, x1)}px`,
    top: `${origin.top + window.scrollY + Math.min(y0, y1)}px`,
    width: `${Math.abs(x1 - x0)}px`,
    height: `${Math.abs(y1 - y0)}px`,
  });
}

// The canvas's size in CSS pixels; the worker resizes the map and its
// surface. Its first report is the size the map started at.
let size = [canvas.clientWidth, canvas.clientHeight];
new ResizeObserver(() => {
  origin = canvas.getBoundingClientRect();
  const next = [canvas.clientWidth, canvas.clientHeight];
  if (next[0] === size[0] && next[1] === size[1]) return;
  size = next;
  map.postMessage({ resize: size });
}).observe(canvas);
