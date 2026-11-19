#!/bin/zsh
# Stress test: launch a burst of Ghostty windows the way a mashed hotkey does, and measure how
# wave keeps up.
#
#   scripts/stress.sh [N=20] [interval_ms=60]
#
# Run it with an empty workspace focused; it refuses otherwise. It launches N Ghostty instances
# with `open -na Ghostty`, one every interval_ms, records where every new window is every 10 ms
# (straight from the WindowServer), and reports:
#   ghostty     launch until the window appears: Ghostty's own start-up, not wave
#   reaction    window appears until wave first moves it
#   settle      last window appears until every window has been still for 500 ms
#   input lag   CLI round trips through wave's reactor every 25 ms during the burst, next to the
#               cost of launching a do-nothing process, which is macOS's share of that lag
#   threads     how much of the burst each wave thread spent working, or blocked on another
#               process (Accessibility and WindowServer calls), from a `sample` profile
# Afterwards it quits only the Ghostty processes it started. It never sends keystrokes.
set -e
cd "${0:A:h}/.."
watcher=${TMPDIR:-/tmp}/wave-stress-watch
if [[ ! -x $watcher || $0 -nt $watcher ]]; then
  swiftc -O -o $watcher - <<'SWIFT'
import CoreGraphics
import Foundation
// Prints "time pid window x y width height" for every on-screen Ghostty window, every 10 ms.
setvbuf(stdout, nil, _IOLBF, 0)
let end = Date().addingTimeInterval(Double(CommandLine.arguments[1]) ?? 10)
while Date() < end {
  let now = Date().timeIntervalSince1970
  let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
  for w in list where (w["kCGWindowLayer"] as? Int) == 0 && (w["kCGWindowOwnerName"] as? String) == "Ghostty" {
    guard let pid = w["kCGWindowOwnerPID"] as? Int, let id = w["kCGWindowNumber"] as? Int,
          let b = w["kCGWindowBounds"] as? NSDictionary, let r = CGRect(dictionaryRepresentation: b) else { continue }
    print(now, pid, id, r.minX, r.minY, r.width, r.height)
  }
  usleep(10_000)
}
SWIFT
fi
exec python3 - "${1:-20}" "${2:-60}" "$watcher" <<'EOF'
import json, os, re, signal, statistics, subprocess, sys, tempfile, threading, time

n, interval, watcher = int(sys.argv[1]), int(sys.argv[2]) / 1000, sys.argv[3]
cli = "target/release/wave.app/Contents/MacOS/wave-cli"
ghostty = "/Applications/Ghostty.app/Contents/MacOS/ghostty"
timeout = 30


def query(*args):
    out = subprocess.run([cli, "query", *args], capture_output=True, text=True, check=True)
    return json.loads(out.stdout)


def pids(pattern):
    out = subprocess.run(["pgrep", "-f", pattern], capture_output=True, text=True).stdout
    return {int(p) for p in out.split()}


def cpu_seconds(pid):
    out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True)
    parts = [float(x) for x in out.stdout.strip().split(":")]
    return sum(v * 60 ** i for i, v in enumerate(reversed(parts)))


def footprint(pid):
    out = subprocess.run(["footprint", "-p", str(pid)], capture_output=True, text=True).stdout
    now = re.search(r"Footprint: ([\d.]+ \w+)", out)
    return now.group(1) if now else "?"


def timed(*command):
    start = time.perf_counter()
    subprocess.run(command, capture_output=True)
    return (time.perf_counter() - start) * 1000


def summary(ms):
    if not ms:
        return "no samples"
    ms = sorted(ms)
    p95 = ms[min(len(ms) - 1, int(len(ms) * 0.95))]
    return f"p50 {statistics.median(ms):.0f} ms, p95 {p95:.0f} ms, max {ms[-1]:.0f} ms ({len(ms)})"


focused = next(w for w in query("workspaces") if w["is_focused"])
if focused["window_count"]:
    sys.exit(f"workspace {focused['name']} has {focused['window_count']} windows; focus an empty one")
wave = min(pids("wave.app/Contents/MacOS/wave$"))
before = pids(ghostty)
idle = [timed(cli, "query", "workspaces") for _ in range(20)]

# Every test window's frames over time, from the watcher: {(pid, window): [(time, frame)]}.
seen, lock = {}, threading.Lock()
watch = subprocess.Popen([watcher, str(n * interval + timeout)], stdout=subprocess.PIPE, text=True)


def read_watcher():
    for line in watch.stdout:
        t, pid, wid, *frame = line.split()
        if int(pid) in before:
            continue
        with lock:
            history = seen.setdefault((int(pid), int(wid)), [])
            if not history or history[-1][1] != frame:
                history.append((float(t), frame))


lag, spawn, stop = [], [], threading.Event()


def probe():
    while not stop.is_set():
        lag.append(timed(cli, "query", "workspaces"))
        spawn.append(timed("/usr/bin/true"))
        time.sleep(0.025)


profile = tempfile.NamedTemporaryFile(suffix=".sample.txt", delete=False).name
sampler = subprocess.Popen(["sample", str(wave), str(int(n * interval) + 4), "-file", profile],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
cpu_before = cpu_seconds(wave)
threading.Thread(target=read_watcher, daemon=True).start()
threading.Thread(target=probe, daemon=True).start()

launches = []
for _ in range(n):
    launches.append(time.time())
    subprocess.Popen(["open", "-na", "Ghostty"])
    time.sleep(interval)

# Settled: N test windows on screen, none moved for 500 ms.
settled = None
while time.time() - launches[-1] < timeout:
    with lock:
        windows = list(seen.values())
    last_move = max((h[-1][0] for h in windows), default=0)
    if len(windows) >= n and time.time() - last_move >= 0.5:
        settled = last_move
        break
    time.sleep(0.05)
stop.set()
cpu = cpu_seconds(wave) - cpu_before
sampler.wait()

with lock:
    windows = {k: list(v) for k, v in seen.items()}
appeared = sorted(h[0][0] for h in windows.values())
reaction = [(h[1][0] - h[0][0]) * 1000 for h in windows.values() if len(h) > 1]
print(f"burst      {n} launches, one every {interval * 1000:.0f} ms")
print(f"ghostty    first window {(appeared[0] - launches[0]) * 1000:.0f} ms after the first launch, "
      f"last {(appeared[-1] - launches[-1]) * 1000:.0f} ms after the last" if appeared else "ghostty    no windows appeared")
print(f"reaction   {summary(reaction)}  (window appears until wave first moves it)")
print(f"never      {sum(len(h) == 1 for h in windows.values())} windows wave never moved")
wave_view = {w["window_server_id"]: w for w in query("windows")}
for (pid, wid), history in windows.items():
    if len(history) == 1:
        w = wave_view.get(wid)
        state = "not in wave's list" if w is None else (
            f"floating {w['is_floating']}, wave frame {w['frame']['origin']['x']:.0f},{w['frame']['origin']['y']:.0f} "
            f"{w['frame']['size']['width']:.0f}x{w['frame']['size']['height']:.0f}")
        print(f"  pid {pid} window {wid} at {' '.join(history[0][1])}: {state}")
if settled is None:
    print(f"settle     not settled after {timeout} s: {len(windows)} of {n} windows on screen")
    known = {a["pid"] for a in query("applications")}
    have = {pid for pid, _ in windows}
    swift = """import CoreGraphics
for w in CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as! [[String: Any]] {
  let b = w["kCGWindowBounds"] as! [String: Any]
  print(w["kCGWindowOwnerPID"]!, w["kCGWindowNumber"]!, w["kCGWindowLayer"]!, w["kCGWindowIsOnscreen"] as? Bool ?? false, b["X"]!, b["Y"]!, b["Width"]!, b["Height"]!)
}"""
    cg = [l.split() for l in subprocess.run(["swift", "-"], input=swift, capture_output=True, text=True).stdout.splitlines()]
    placed = {w["window_server_id"]: (ws["name"], w["is_floating"]) for ws in query("workspaces") for w in ws["windows"]}
    for pid in sorted(pids(ghostty) - before - have):
        print(f"  no window from pid {pid}; wave knows the app: {pid in known}")
        for _, wid, layer, onscreen, x, y, w, h in (l for l in cg if l[0] == str(pid)):
            print(f"    macOS window {wid}: layer {layer}, on screen {onscreen}, {x},{y} {w}x{h}; "
                  f"wave workspace and floating: {placed.get(int(wid), 'not tracked')}")
else:
    print(f"settle     {(settled - appeared[-1]) * 1000:.0f} ms after the last window appeared, "
          f"{(settled - launches[-1]) * 1000:.0f} ms after the last launch")
print(f"idle lag   {summary(idle)}")
print(f"burst lag  {summary(lag)}")
print(f"spawn only {summary(spawn)}  (macOS's share: launching /usr/bin/true)")
print(f"wave       {cpu:.2f} s CPU during the burst; footprint now {footprint(wave)}")

# Each thread's samples, split into waiting for work, blocked on another process, and working.
# A wait counts as idle when it's the thread's own loop asking for its next job.
waits = re.compile(r"mach_msg2_trap|semaphore_\w*_trap|__psynch_cvwait|__semwait_signal|__workq_kernreturn|kevent|__ulock_wait|swtch_pri|__select")
loops = re.compile(r"__CFRunLoopServiceMachPort|park|recv|_pthread_wqthread|start_wqthread|thread::sleep|nanosleep|__select|CVDisplayLink|_dispatch_semaphore_wait|block_on")
threads, thread, stack = {}, None, []
for line in open(profile, errors="replace"):
    if line.startswith(("Total number", "Sort by")):
        break
    m = re.match(r"^ {4}(\d+) Thread_\d+(?:: ([^ \n]+))?", line)
    if m:
        thread, stack = m.group(2) or "unnamed", []
        threads.setdefault(thread, {"total": 0, "idle": 0, "blocked": 0})["total"] += int(m.group(1))
        continue
    m = re.match(r"^([\s+!:|]*)(\d+) (\S+)", line)
    if not (thread and m):
        continue
    depth = len(m.group(1))
    stack = [(d, f) for d, f in stack if d < depth] + [(depth, m.group(3))]
    if waits.match(m.group(3)):
        idle_wait = any(loops.search(f) for _, f in stack[:-1])
        threads[thread]["idle" if idle_wait else "blocked"] += int(m.group(2))
print("threads    share of the burst spent working / blocked on another process")
rows = []
for name, t in threads.items():
    if name.startswith("com.") or name.startswith("org."):
        name = "app threads"  # one per running app; summed
    rows.append((name, t))
merged = {}
for name, t in rows:
    m = merged.setdefault(name, {"total": 0, "idle": 0, "blocked": 0, "count": 0})
    for k in ("total", "idle", "blocked"):
        m[k] += t[k]
    m["count"] += 1
for name, t in sorted(merged.items(), key=lambda kv: kv[1]["idle"] / kv[1]["total"]):
    work = t["total"] - t["idle"] - t["blocked"]
    if t["total"] and (work + t["blocked"]) / t["total"] >= 0.01:
        label = f"{name} ({t['count']})" if t["count"] > 1 else name
        print(f"  {100 * work / t['total']:5.1f}% / {100 * t['blocked'] / t['total']:5.1f}%  {label}")
print(f"profile    {profile}")

# Quit only the Ghostty processes this test started.
watch.kill()
ours = pids(ghostty) - before
for pid in ours:
    os.kill(pid, signal.SIGTERM)
time.sleep(1)
left = pids(ghostty) & ours
print(f"cleanup    quit {len(ours) - len(left)} test Ghostty processes"
      + (f", {len(left)} still running: {sorted(left)}" if left else ""))
EOF
