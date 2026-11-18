// Draws the README banner with headless Chrome.
// Run from the repo root: node assets/render.mjs   (set CHROME=/path/to/chrome if needed)
// Writes assets/logo.png (the README banner) and assets/icon.png (the 1024px app icon).
import { writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const chrome = process.env.CHROME ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const tmp = mkdtempSync(join(tmpdir(), 'wave-render-'));

// Kanagawa (wave theme): sumiInk3 background, fujiWhite text, springBlue wave, carpYellow title.
const C = { bg: '#1f1f28', text: '#dcd7ba', bracket: '#c8c093', accent: '#e6c384', blue: '#7fb4ca' };
const FONT = '"JetBrainsMono NFM", "JetBrains Mono", ui-monospace, Menlo, monospace';

// The mark: [~], a wave between brackets, the way tascii's is [x].
const mark = `<span>[</span><b style="color:${C.blue}">~</b><span>]</span>`;

const [w, h] = [960, 260];
const html = `<!doctype html><meta charset="utf-8"><style>html,body{margin:0;width:${w}px;height:${h}px;overflow:hidden;background:${C.bg};font-family:${FONT}}</style>
<div style="position:absolute;inset:0;display:flex;align-items:center;justify-content:center;gap:56px">
  <div style="font-size:120px;font-weight:800;color:${C.bracket};letter-spacing:-0.04em;line-height:1">${mark}</div>
  <div style="display:flex;flex-direction:column;gap:14px">
    <div style="font-size:64px;font-weight:800;color:${C.accent};letter-spacing:-0.02em;line-height:1">wave</div>
    <div style="font-size:20px;color:${C.text}">a tiling window manager for macOS</div>
  </div>
</div>`;

// One page, screenshotted at 2x by headless Chrome. `transparent` keeps the corners clear.
function shot(name, page, [pw, ph], out, transparent = false) {
  const file = join(tmp, `${name}.html`);
  writeFileSync(file, page);
  execFileSync(chrome, ['--headless', '--disable-gpu', '--hide-scrollbars', '--force-device-scale-factor=2',
    ...(transparent ? ['--default-background-color=00000000'] : []),
    `--user-data-dir=${join(tmp, 'profile')}`, `--window-size=${pw},${ph}`, `--screenshot=${out}`, pathToFileURL(file).href],
    { stdio: 'ignore' });
  console.log(`wrote ${out.slice(root.length + 1)}`);
}

shot('logo', html, [w, h], join(root, 'assets', 'logo.png'));

// The icon's wave is drawn larger than in the banner, so it still reads at 16px in Finder.
const iconMark = `<span>[</span><b style="color:${C.blue};display:inline-block;transform:scale(1.4)">~</b><span>]</span>`;

// The app icon, laid out on Apple's macOS grid: a 412pt rounded square, 50pt in from each edge of
// the 512pt canvas, with a soft drop shadow. Rendered at 2x, so 1024px.
const icon = `<!doctype html><meta charset="utf-8"><style>html,body{margin:0;width:512px;height:512px;overflow:hidden;background:transparent;font-family:${FONT}}</style>
<div style="position:absolute;left:50px;top:50px;width:412px;height:412px;border-radius:92px;background:${C.bg};
  box-shadow:0 10px 24px rgba(0,0,0,.35), inset 0 0 0 1.5px rgba(220,215,186,.10);
  display:flex;align-items:center;justify-content:center">
  <div style="font-size:150px;font-weight:800;color:${C.bracket};letter-spacing:-0.04em;line-height:1;margin-top:-8px">${iconMark}</div>
</div>`;
shot('icon', icon, [512, 512], join(root, 'assets', 'icon.png'), true);

rmSync(tmp, { recursive: true, force: true });
