// Draws the README banner with headless Chrome.
// Run from the repo root: node assets/render.mjs   (set CHROME=/path/to/chrome if needed)
// Writes assets/logo.png.
import { writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const chrome = process.env.CHROME ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const tmp = mkdtempSync(join(tmpdir(), 'wave-render-'));

// Gruvbox Material Dark, the same palette as tascii.
const C = { bg: '#282828', text: '#d4be98', dim: '#a09176', accent: '#d8a657', aqua: '#89b482', blue: '#7daea3', line: '#7c6f64' };
const FONT = '"JetBrainsMono NFM", "JetBrains Mono", ui-monospace, Menlo, monospace';

// The mark: [~], a wave between brackets, the way tascii's is [x].
const mark = `<span>[</span><b style="color:${C.blue}">~</b><span>]</span>`;

const [w, h] = [960, 260];
const html = `<!doctype html><meta charset="utf-8"><style>html,body{margin:0;width:${w}px;height:${h}px;overflow:hidden;background:${C.bg};font-family:${FONT}}</style>
<div style="position:absolute;inset:0;display:flex;align-items:center;justify-content:center;gap:56px">
  <div style="font-size:120px;font-weight:800;color:${C.text};letter-spacing:-0.04em;line-height:1">${mark}</div>
  <div style="display:flex;flex-direction:column;gap:14px">
    <div style="font-size:64px;font-weight:800;color:${C.accent};letter-spacing:-0.02em;line-height:1">wave</div>
    <div style="font-size:20px;color:${C.text}">a tiling window manager for macOS</div>
  </div>
</div>`;

const file = join(tmp, 'logo.html');
writeFileSync(file, html);
const out = join(root, 'assets', 'logo.png');
execFileSync(chrome, ['--headless', '--disable-gpu', '--hide-scrollbars', '--force-device-scale-factor=2',
  `--user-data-dir=${join(tmp, 'profile')}`, `--window-size=${w},${h}`, `--screenshot=${out}`, pathToFileURL(file).href],
  { stdio: 'ignore' });
rmSync(tmp, { recursive: true, force: true });
console.log('wrote assets/logo.png');
