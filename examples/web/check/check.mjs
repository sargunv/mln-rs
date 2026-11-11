// Checks the browser build in headless Chromium, offline: loads views of the
// render suite's fixtures in the page (web/), reads each frame back from the
// GPU inside the page, and compares it with the native render-test runner's
// render of the same view (pixelmatch, as the render tests compare). It
// fails when a frame differs by more than its threshold, when the page, one of
// its Web Workers (the map's or a tile worker's) or the native runner reports
// an error, or when a view's tiles got no worker replies. CI runs it (the
// `web` job); see ../README.md.
//
//   node examples/web/check/check.mjs [options]
//
//   --filter <regex>     the views of VIEWS to load, by id (default: all)
//   --workers <n>        Web Workers for the tiles (default 2)
//   --out <dir>          where the views, frames and results go
//                        (default: a new temporary directory)
//   --render-test <path> the native runner (default:
//                        $CARGO_TARGET_DIR/debug/render-test)
//   --codec <n>          render natively with --tileWorkerCodec=<n>
//   --chromium <path>    default: Playwright's Chromium CHROMIUM_BUILD
//                        (`npx playwright@1.61.1 install chromium`)
//
// The page's WebGPU adapter is SwiftShader, which Chromium bundles, so the
// frames don't depend on the machine; the native runner renders through
// VK_DRIVER_FILES (default /usr/share/vulkan/icd.d/lvp_icd.x86_64.json,
// lavapipe). Headless Chromium composites WebGPU canvases as white, so the
// page reads its frame back itself.

import { spawn, spawnSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { homedir, tmpdir } from 'node:os';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { crc32, deflateSync, inflateSync } from 'node:zlib';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '../../..');
const metrics = path.join(repo, 'tools/render-test/testdata/metrics');

// The render suite's tests the page loads, with their fixtures from the
// suite's cache database: the real-world styles (vector tiles with fills,
// lines, icons and labels), raster tiles, raster-dem tiles (hillshade and
// color relief, whose borders the map side backfills from the neighbouring
// tiles), raster with GeoJSON, and labels in font faces (complex scripts
// and right-to-left text, which the map shapes with the shaper module it
// loads for them).
//
// THRESHOLD is the share of a view's pixels that may differ (pixelmatch,
// with the render tests' per-pixel threshold), unless the view says
// otherwise. The page renders on SwiftShader and the native runner on
// lavapipe; most views come out identical, and leaving out one layer of
// labels changes 0.1% to 2% of the pixels.
const THRESHOLD = 0.001;
const VIEWS = [
  { id: 'real-world/nepal' },
  { id: 'real-world/norway' },
  { id: 'real-world/uruguay' },
  { id: 'real-world/chicago' },
  { id: 'real-world/sanfrancisco' },
  { id: 'raster-rotation/45' },
  { id: 'raster-masking/overlapping-vector' },
  { id: 'hillshade-accent-color/default' },
  // SwiftShader and lavapipe antialias the edges of the z1 coastlines
  // differently: 0.5% of the pixels.
  { id: 'combinations/color-relief--hillshade--vector-fill--color-relief--hillshade', allowed: 0.01 },
  { id: 'text-font/devanagari' },
  { id: 'text-font/khmer' },
  { id: 'text-font/font-faces-bidi' },
  { id: 'is-supported-script/font-faces-layout' },
];

// Playwright 1.61.1's Chromium (Chrome for Testing 149), as CI installs it.
const CHROMIUM_BUILD = 'chromium-1228';

const { values: options } = parseArgs({
  options: {
    filter: { type: 'string', default: '' },
    workers: { type: 'string', default: '2' },
    out: { type: 'string' },
    'render-test': { type: 'string' },
    codec: { type: 'string' },
    chromium: {
      type: 'string',
      default: path.join(
        process.env.PLAYWRIGHT_BROWSERS_PATH ?? path.join(homedir(), '.cache/ms-playwright'),
        CHROMIUM_BUILD,
        'chrome-linux64/chrome',
      ),
    },
  },
});
const workers = Number(options.workers);
const icd = process.env.VK_DRIVER_FILES ?? '/usr/share/vulkan/icd.d/lvp_icd.x86_64.json';
if (!existsSync(options.chromium)) {
  throw new Error(`no Chromium at ${options.chromium}: run \`npx playwright@1.61.1 install chromium\` or pass --chromium`);
}

// MARK: - The fixtures: the render suite's cache database

const database = new DatabaseSync(path.join(metrics, 'cache-style.db'), { readOnly: true });
const resourceQuery = database.prepare('SELECT kind, data, compressed FROM resources WHERE url = ?');
const tileQuery = database.prepare('SELECT data, compressed FROM tiles WHERE id = ?');

/** The cache's URL for a URL of the fixtures, as the runner's file source maps it (tools/render-test/src/file_source.rs). */
function cacheUrl(url) {
  for (const prefix of ['local://', 'http://localhost:2900/']) {
    if (url.startsWith(prefix)) return `http://${url.slice(prefix.length)}`;
  }
  return url;
}

// The cache keys tiles by their URL template and coordinates; the page
// requests them by URL.
const tiles = new Map();
for (const { id, url_template: template, pixel_ratio: ratio, z, x, y } of database
  .prepare('SELECT id, url_template, pixel_ratio, z, x, y FROM tiles')
  .all()) {
  const url = template
    .replaceAll('{z}', z)
    .replaceAll('{x}', x)
    .replaceAll('{y}', y)
    .replaceAll('{ratio}', ratio > 1 ? `@${ratio}x` : '');
  tiles.set(url, id);
}

function unpack(row) {
  return row.compressed ? inflateSync(row.data) : Buffer.from(row.data);
}

/** The cached resource or tile at a URL of the fixtures, if there is one. */
function cached(url) {
  const key = cacheUrl(url);
  const resource = resourceQuery.get(key);
  if (resource) return { kind: resource.kind, data: unpack(resource) };
  const tile = tiles.has(key) ? tileQuery.get(tiles.get(key)) : undefined;
  return tile ? { kind: 'tile', data: unpack(tile) } : undefined;
}

/** A cached JSON document, such as a style. */
function cachedJson(url) {
  const entry = cached(url);
  if (!entry) throw new Error(`${url} is not in the cache`);
  return JSON.parse(entry.data.toString('utf8'));
}

// MARK: - Views

/**
 * A view: the render test's style, with its operations followed, so that
 * the camera is in the style. The page can't follow other operations or
 * test options, so a view must not have them.
 */
function view({ id, allowed = THRESHOLD }) {
  const test = JSON.parse(readFileSync(path.join(metrics, 'integration/render-tests', id, 'style.json'), 'utf8'));
  const metadata = test.metadata?.test ?? {};
  for (const key of Object.keys(metadata)) {
    if (!['width', 'height', 'pixelRatio', 'operations', 'allowed', 'description'].includes(key)) {
      throw new Error(`${id}: the page can't follow the test's "${key}"`);
    }
  }
  let style = test;
  for (const [operation, ...args] of metadata.operations ?? []) {
    if (operation === 'setStyle') style = cachedJson(args[0]);
    else if (operation === 'setZoom') style.zoom = args[0];
    else if (operation === 'setCenter') style.center = args[0];
    else if (operation !== 'wait') throw new Error(`${id}: the page can't follow the operation ${operation}`);
  }
  style = structuredClone(style);
  style.center ??= [0, 0];
  style.zoom ??= 0;
  style.bearing ??= 0;
  style.pitch ??= 0;
  const width = metadata.width ?? 512;
  const height = metadata.height ?? 512;
  const ratio = metadata.pixelRatio ?? 1;
  style.metadata = { test: { width, height, pixelRatio: ratio, allowed } };
  return { id, style, width, height, ratio, allowed };
}

const views = VIEWS.filter(({ id }) => new RegExp(options.filter).test(id)).map(view);
if (views.length === 0) throw new Error(`no view matches ${options.filter}`);

// MARK: - The page, served from web/, and the fixtures

const root = path.join(here, '../web');
if (!existsSync(path.join(root, 'pkg/mln_web_bg.wasm'))) throw new Error('no web/pkg: run `just web` first');
const types = { '.html': 'text/html', '.js': 'text/javascript', '.wasm': 'application/wasm', '.json': 'application/json' };

// Every URL the page loads is on this server: `/fixtures/<scheme>/<rest>`
// serves the cache's `<scheme>://<rest>`.
let origin;
const fixtureUrl = (url) => {
  const match = /^([a-z]+):\/\/(.*)$/.exec(url);
  return match ? `${origin}/fixtures/${match[1]}/${match[2]}` : url;
};

/** The style or TileJSON with each URL in it on this server. */
function served(json) {
  const copy = structuredClone(json);
  if (Array.isArray(copy.tiles)) copy.tiles = copy.tiles.map(fixtureUrl);
  for (const source of Object.values(copy.sources ?? {})) {
    if (typeof source.url === 'string') source.url = fixtureUrl(source.url);
    if (Array.isArray(source.urls)) source.urls = source.urls.map(fixtureUrl);
    if (Array.isArray(source.tiles)) source.tiles = source.tiles.map(fixtureUrl);
    if (typeof source.data === 'string') source.data = fixtureUrl(source.data);
  }
  if (typeof copy.sprite === 'string') copy.sprite = fixtureUrl(copy.sprite);
  if (Array.isArray(copy.sprite)) copy.sprite = copy.sprite.map((sprite) => ({ ...sprite, url: fixtureUrl(sprite.url) }));
  if (typeof copy.glyphs === 'string') copy.glyphs = fixtureUrl(copy.glyphs);
  for (const [name, faces] of Object.entries(copy['font-faces'] ?? {})) {
    copy['font-faces'][name] = typeof faces === 'string'
      ? fixtureUrl(faces)
      : faces.map((face) => ({ ...face, url: fixtureUrl(face.url) }));
  }
  return copy;
}

const SOURCE = 2; // Resource::Kind::Source: a TileJSON
// The fixtures the current view asked for that the cache doesn't have. The
// runner's cache-only file source answers those with no content, and so
// does this server (204, as HTTPFileSource reads it).
let missing = [];

const server = createServer((request, response) => {
  const url = new URL(request.url, 'http://x');
  const fixture = /^\/fixtures\/([a-z]+)\/(.*)$/.exec(request.url);
  if (fixture) {
    const entry = cached(`${fixture[1]}://${fixture[2]}`);
    if (!entry) {
      missing.push(`${fixture[1]}://${fixture[2]}`);
      response.writeHead(204).end();
    } else if (entry.kind === SOURCE) {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify(served(JSON.parse(entry.data.toString('utf8')))));
    } else {
      response.writeHead(200, { 'content-type': 'application/octet-stream' }).end(entry.data);
    }
    return;
  }
  const style = /^\/style\/(\d+)$/.exec(url.pathname);
  if (style) {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify(served(views[Number(style[1])].style)));
    return;
  }
  if (url.pathname === '/blank') {
    response.writeHead(200, { 'content-type': 'text/html' }).end('<!doctype html>');
    return;
  }
  const file = path.join(root, path.normalize(decodeURIComponent(url.pathname)));
  const target = file.endsWith(path.sep) ? path.join(file, 'index.html') : file;
  if (!target.startsWith(root) || !existsSync(target)) {
    response.writeHead(404).end();
    return;
  }
  response.writeHead(200, { 'content-type': types[path.extname(target)] ?? 'application/octet-stream' });
  response.end(readFileSync(target));
});
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
origin = `http://127.0.0.1:${server.address().port}`;

const pageUrl = (view, count) => {
  const params = new URLSearchParams({
    style: `${origin}/style/${views.indexOf(view)}`,
    lon: view.style.center[0],
    lat: view.style.center[1],
    zoom: view.style.zoom,
    bearing: view.style.bearing,
    pitch: view.style.pitch,
    width: view.width,
    height: view.height,
    ratio: view.ratio,
    workers: String(count),
    // The native runner downloads every glyph, as GL JS's render tests do.
    localIdeographFontFamily: 'false',
  });
  return `${origin}/index.html?${params}`;
};

// MARK: - Chromium over the DevTools protocol

async function launch() {
  const profile = await mkdtemp(path.join(tmpdir(), 'mln-web-check-profile-'));
  const env = { ...process.env };
  delete env.VK_DRIVER_FILES; // the native runner's driver
  const child = spawn(
    options.chromium,
    [
      '--headless=new',
      '--no-sandbox',
      '--no-first-run',
      '--password-store=basic',
      '--use-mock-keychain',
      '--disable-dev-shm-usage',
      '--enable-unsafe-webgpu',
      // SwiftShader, which Chromium bundles, for WebGPU and for compositing
      // through Vulkan: with headless Chromium's default GL compositing, the
      // copy of the frame that the page reads back is never mapped.
      '--enable-features=Vulkan',
      '--use-angle=vulkan',
      '--use-vulkan=swiftshader',
      '--use-webgpu-adapter=swiftshader',
      // Offline: a request that escapes the local server (a URL that
      // served() doesn't rewrite) fails to resolve, an error in the page
      // or a worker, rather than reaching the network.
      '--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1',
      '--remote-debugging-port=0',
      `--user-data-dir=${profile}`,
      'about:blank',
    ],
    { stdio: ['ignore', 'ignore', 'pipe'], env },
  );
  const exited = new Promise((resolve) => child.on('exit', resolve));
  let log = '';
  const url = await new Promise((resolve, reject) => {
    child.stderr.on('data', (chunk) => {
      log += chunk;
      const match = /DevTools listening on (ws:\/\/\S+)/.exec(log);
      if (match) resolve(match[1]);
    });
    exited.then((code) => reject(new Error(`chromium exited with ${code}\n${log}`)));
  });
  const socket = new WebSocket(url);
  await new Promise((resolve, reject) => {
    socket.onopen = resolve;
    socket.onerror = reject;
  });
  let next = 1;
  const pending = new Map();
  const listeners = new Set();
  socket.onmessage = (event) => {
    const message = JSON.parse(event.data);
    if (message.id === undefined) {
      for (const listener of listeners) listener(message);
      return;
    }
    const entry = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) entry.reject(new Error(`${entry.method}: ${JSON.stringify(message.error)}`));
    else entry.resolve(message.result);
  };
  const send = (method, params = {}, sessionId = undefined) =>
    new Promise((resolve, reject) => {
      const id = next++;
      pending.set(id, { resolve, reject, method });
      socket.send(JSON.stringify({ id, method, params, sessionId }));
    });
  return {
    send,
    listeners,
    async close() {
      socket.close();
      child.kill();
      await exited;
      await rm(profile, { recursive: true, force: true, maxRetries: 5 });
    },
  };
}

const text = (args) => args.map((arg) => arg.value ?? arg.description).join(' ');

/**
 * Opens `url` in a new tab; returns the session, with the errors that its
 * console and its Web Workers' consoles report, and `failed`, which
 * resolves at the first of them.
 */
async function open(browser, url) {
  const { targetId } = await browser.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await browser.send('Target.attachToTarget', { targetId, flatten: true });
  const errors = [];
  let fail;
  const failed = new Promise((resolve) => {
    fail = resolve;
  });
  const report = (error) => {
    errors.push(error);
    fail();
  };
  const sessions = new Map([[sessionId, 'page']]);
  const listener = (message) => {
    // The Web Workers: each starts paused until its console is attached.
    if (message.method === 'Target.attachedToTarget' && sessions.has(message.sessionId)) {
      const worker = message.params.sessionId;
      sessions.set(worker, `worker ${sessions.size}`);
      browser
        .send('Runtime.enable', {}, worker)
        .then(() => browser.send('Runtime.runIfWaitingForDebugger', {}, worker))
        .catch((error) => report(`attaching to a worker: ${error.message}`));
      return;
    }
    const where = sessions.get(message.sessionId);
    if (where === undefined) return;
    if (process.env.MLN_WEB_CHECK_LOG && message.method === 'Runtime.consoleAPICalled') {
      console.error(`[${where}] ${text(message.params.args)}`);
    }
    if (message.method === 'Runtime.consoleAPICalled' && message.params.type === 'error') {
      report(`${where}: ${text(message.params.args)}`);
    } else if (message.method === 'Runtime.exceptionThrown') {
      const details = message.params.exceptionDetails;
      report(`${where}: ${details.exception?.description ?? details.text}`);
    }
  };
  browser.listeners.add(listener);
  await browser.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: true, flatten: true }, sessionId);
  await browser.send('Runtime.enable', {}, sessionId);
  await browser.send('Page.enable', {}, sessionId);
  const loaded = new Promise((resolve) => {
    const onLoad = (message) => {
      if (message.sessionId === sessionId && message.method === 'Page.loadEventFired') {
        browser.listeners.delete(onLoad);
        resolve();
      }
    };
    browser.listeners.add(onLoad);
  });
  await browser.send('Page.navigate', { url }, sessionId);
  await loaded;
  return {
    targetId,
    sessionId,
    errors,
    failed,
    async close() {
      await browser.send('Target.closeTarget', { targetId });
      browser.listeners.delete(listener);
    },
  };
}

async function evaluate(browser, session, expression) {
  const result = await browser.send(
    'Runtime.evaluate',
    { expression, awaitPromise: true, returnByValue: true },
    session.sessionId,
  );
  if (result.exceptionDetails) {
    throw new Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text);
  }
  return result.result.value;
}

/**
 * Loads `view` with `count` workers; resolves with the page's statistics,
 * its frame, the errors the page and its workers reported, and why it
 * couldn't load or read the frame, if it couldn't. An error stops the
 * loading: the tiles of a worker that panicked never load.
 */
async function load(browser, view, count) {
  const session = await open(browser, pageUrl(view, count));
  const failed = session.failed.then(async () => {
    await new Promise((resolve) => setTimeout(resolve, 500)); // the errors that follow
    throw new Error('the page or a worker reported an error');
  });
  const run = async () => {
    const stats = await evaluate(
      browser,
      session,
      `(async () => {
        const deadline = performance.now() + 60000;
        while (!window.mln) {
          if (performance.now() > deadline) throw new Error('the page did not start');
          await new Promise((resolve) => setTimeout(resolve, 50));
        }
        return await Promise.race([
          window.mln.loaded,
          new Promise((_, reject) => setTimeout(() => reject(new Error('not loaded after 60 s')), 60000)),
        ]);
      })()`,
    );
    // Let the labels fade in, then draw the frame again and read it back.
    await evaluate(browser, session, 'new Promise((resolve) => setTimeout(resolve, 1500))');
    const pixels = await evaluate(
      browser,
      session,
      `(async () => {
        const { width, height, pixels } = await window.mln.readPixels();
        let binary = '';
        for (let i = 0; i < pixels.length; i += 0x8000) {
          binary += String.fromCharCode(...pixels.subarray(i, i + 0x8000));
        }
        return { width, height, base64: btoa(binary) };
      })()`,
    );
    return { stats, pixels };
  };
  try {
    return { ...(await Promise.race([run(), failed])), errors: session.errors };
  } catch (error) {
    return { failure: error.message.split('\n')[0], errors: session.errors };
  } finally {
    await session.close();
  }
}

// MARK: - PNG

function png(width, height, rgba) {
  const chunk = (type, data) => {
    const length = Buffer.alloc(4);
    length.writeUInt32BE(data.length);
    const body = Buffer.concat([Buffer.from(type), data]);
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(body));
    return Buffer.concat([length, body, crc]);
  };
  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0);
  header.writeUInt32BE(height, 4);
  header.set([8, 6, 0, 0, 0], 8); // 8-bit RGBA
  const rows = Buffer.alloc((width * 4 + 1) * height);
  for (let y = 0; y < height; y++) {
    rgba.copy(rows, y * (width * 4 + 1) + 1, y * width * 4, (y + 1) * width * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', header),
    chunk('IDAT', deflateSync(rows)),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

// MARK: - Main

const browser = await launch();
let failures = [];
try {
  const gpu = await (async () => {
    const session = await open(browser, `${origin}/blank`);
    try {
      return await evaluate(
        browser,
        session,
        `navigator.gpu?.requestAdapter().then((a) => (a ? \`\${a.info.vendor} \${a.info.architecture}\` : 'none'))`,
      );
    } finally {
      await session.close();
    }
  })();
  console.log(`WebGPU adapter: ${gpu}`);
  if (!/swiftshader/.test(gpu ?? '')) throw new Error(`the page's WebGPU adapter is ${gpu}, not SwiftShader`);

  failures = await compare();
} finally {
  await browser.close();
  server.close();
  database.close();
}
if (failures.length > 0) {
  console.log(`\n${failures.length} of ${views.length} views failed:\n  ${failures.join('\n  ')}`);
  process.exitCode = 1;
}

/** Loads and compares every view; returns what failed. */
async function compare() {
  const out = path.resolve(options.out ?? (await mkdtemp(path.join(tmpdir(), 'mln-web-check-'))));
  // A kept --out holds an earlier run's views and results; without this a
  // runner that writes no results would leave the old ones to be read.
  for (const dir of ['tests', 'results']) await rm(path.join(out, dir), { recursive: true, force: true });
  const failures = [];
  for (const view of views) {
    missing = [];
    const { stats, pixels, errors, failure } = await load(browser, view, workers);
    console.log(`${view.id}: ` +
      (failure ? failure : `loaded in ${Math.round(stats.loadedMs)} ms, ${stats.replies} worker replies`) +
      (missing.length ? `, ${missing.length} tiles or resources not in the cache (no content)` : '') +
      (errors.length ? `, ${errors.length} errors:\n  ${errors.map(firstLines).join('\n  ')}` : ''));
    if (process.env.MLN_WEB_CHECK_LOG && missing.length) console.error(`  not in the cache: ${missing.join(', ')}`);
    if (failure) {
      failures.push(`${view.id}: ${failure}`);
      continue;
    }
    if (errors.length) failures.push(`${view.id}: ${errors.length} errors in the page or its workers`);
    // Every view has tiles, and only their workers reply: the raster and
    // raster-dem tiles' too, in the views that have nothing else.
    if (stats.replies === 0) failures.push(`${view.id}: no worker replies`);
    const dir = path.join(out, 'tests/render-tests', view.id);
    await mkdir(dir, { recursive: true });
    await writeFile(path.join(dir, 'style.json'), JSON.stringify(view.style, null, 1));
    await writeFile(path.join(dir, 'expected.png'), png(pixels.width, pixels.height, Buffer.from(pixels.base64, 'base64')));
  }
  if (!existsSync(path.join(out, 'tests'))) return failures;

  // The native renders, compared with the page's frames as references.
  const manifest = path.join(out, 'manifest.json');
  await writeFile(manifest, JSON.stringify({
    base_test_path: path.join(out, 'tests'),
    cache_path: path.join(metrics, 'cache-style.db'),
    expectation_paths: [],
    ignore_paths: [],
  }));
  const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repo, 'target');
  const runner = options['render-test'] ?? path.join(targetDir, 'debug/render-test');
  const args = ['-p', manifest, '--output', path.join(out, 'results')];
  if (options.codec) args.push(`--tileWorkerCodec=${options.codec}`);
  const native = spawnSync(runner, args, { env: { ...process.env, VK_DRIVER_FILES: icd }, encoding: 'utf8' });
  if (native.error) throw native.error;
  if (!existsSync(path.join(out, 'results/manifest.html'))) {
    console.log(native.stdout, native.stderr);
    return [...failures, `the native runner exited with ${native.status} and no results`];
  }
  const page = await readFile(path.join(out, 'results/manifest.html'), 'utf8');
  console.log(`\nnative render (${runner}${options.codec ? `, codec with ${options.codec} instances` : ''}) ` +
    `against the page's frame, ${workers} tile workers:`);
  const results = new Map();
  for (const item of page.split('<div class="test ').slice(1)) {
    const id = /<\/span> render-tests\/([^<]+)<\/h2>/.exec(item)?.[1];
    const status = /^(\w+)/.exec(item)?.[1];
    const diff = Number(/<strong>Diff:<\/strong> ([0-9.e-]+)/.exec(item)?.[1] ?? 0);
    const error = /<strong>Error:<\/strong> ([^<]*)/.exec(item)?.[1];
    results.set(id, { status, diff, error });
  }
  for (const view of views) {
    if (!existsSync(path.join(out, 'tests/render-tests', view.id))) continue;
    const result = results.get(view.id);
    if (!result) {
      console.log(`  ${view.id}: not rendered`);
      failures.push(`${view.id}: the native runner didn't render it`);
    } else if (result.error) {
      console.log(`  ${view.id}: error: ${result.error}`);
      failures.push(`${view.id}: the native runner: ${result.error}`);
    } else {
      console.log(`  ${view.id}: ${(result.diff * 100).toFixed(3)}% of the pixels differ ` +
        `(at most ${(view.allowed * 100).toFixed(1)}%)`);
      if (result.status !== 'passed' || result.diff > view.allowed) {
        failures.push(`${view.id}: ${(result.diff * 100).toFixed(3)}% of the pixels differ (${result.status})`);
      }
    }
  }
  console.log(`\nviews, frames and diffs: ${out}`);
  return failures;
}

/** An error's message, without the rest of its stack. */
function firstLines(error) {
  return error.split('\n').slice(0, 2).join(' ');
}
