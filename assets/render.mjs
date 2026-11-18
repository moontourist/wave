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

// The wave, one row per line: [color, text] runs. Foam on top, the curl in aqua, open water in blue.
const WAVE = [
  [['text', '             _.-~~-._'], ['line', '   .']],
  [['text', "          .-'  .--.  '."], ['line', "  ' ."]],
  [['aqua', "       .-'    / .-.\\   \\"]],
  [['aqua', "    .-'      | (  ~ |   |"], ['line', ' .']],
  [['aqua', " .-'          \\ '--'   /"]],
  [['blue', "~~~~~~~~~~~~~~~'~~~~~~'~~~~~~~"]],
  [['blue', '  ~~~~~  ~~~~~~~  ~~~~~~  ~~~~']],
];
const escape = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;');
const art = WAVE.map((row) => row.map(([c, s]) => `<span style="color:${C[c]}">${escape(s)}</span>`).join('')).join('\n');

const [w, h] = [960, 300];
const html = `<!doctype html><meta charset="utf-8"><style>html,body{margin:0;width:${w}px;height:${h}px;overflow:hidden;background:${C.bg};font-family:${FONT}}</style>
<div style="position:absolute;inset:0;display:flex;align-items:center;justify-content:center;gap:64px">
  <pre style="margin:0;font:inherit;font-size:22px;line-height:1.2">${art}</pre>
  <div style="display:flex;flex-direction:column;gap:14px">
    <div style="font-size:64px;font-weight:800;color:${C.accent};letter-spacing:-0.02em;line-height:1">wave</div>
    <div style="font-size:20px;color:${C.text}">a tiling window manager for macOS</div>
    <div style="font-size:16px;color:${C.dim}">built on rift</div>
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
