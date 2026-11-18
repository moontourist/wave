#!/bin/zsh
# Runs clippy with the audit's lints and reports what a change added.
#
#   scripts/audit.sh [base]   findings on lines changed since `base` (default HEAD), plus new files
#   scripts/audit.sh --counts finding counts per lint for the whole workspace, to compare before
#                             and after a wide mechanical change such as a rename
#
# Prints nothing when the change is clean. Tests may panic: clippy.toml allows it there.
set -e
cd "${0:A:h}/.."

lints=(
  -W clippy::unwrap_used -W clippy::expect_used -W clippy::panic -W clippy::unreachable
  -W clippy::todo -W clippy::unimplemented -W clippy::indexing_slicing -W clippy::string_slice
  -W clippy::undocumented_unsafe_blocks -W clippy::multiple_unsafe_ops_per_block
  -W clippy::missing_safety_doc -W clippy::as_conversions -W clippy::wildcard_enum_match_arm
  -W clippy::clone_on_ref_ptr -W unreachable_pub
)
out=$(mktemp)
trap 'rm -f "$out"' EXIT
cargo clippy --workspace --all-targets --message-format=json -- -A clippy::all "${lints[@]}" \
  2>/dev/null >"$out"

mode=${1:-HEAD}
python3 - "$out" "$mode" <<'EOF'
import json, re, subprocess, sys
from collections import Counter

path, mode = sys.argv[1], sys.argv[2]
findings = set()
for line in open(path):
    m = json.loads(line)
    if m.get("reason") != "compiler-message":
        continue
    m = m["message"]
    code = (m.get("code") or {}).get("code") or ""
    if not code or not m["spans"]:
        continue
    s = next((s for s in m["spans"] if s["is_primary"]), m["spans"][0])
    findings.add((s["file_name"].replace("tests/../", ""), s["line_start"], code.removeprefix("clippy::")))

if mode == "--counts":
    for lint, n in sorted(Counter(f[2] for f in findings).items()):
        print(f"{n:6} {lint}")
    sys.exit()

def git(*args):
    return subprocess.run(["git", *args], capture_output=True, text=True, check=True).stdout

changed = {}
current = None
for line in git("diff", "-U0", "--no-color", mode).splitlines():
    if line.startswith("+++ "):
        current = line[6:] if line.startswith("+++ b/") else None
    elif line.startswith("@@") and current:
        start, _, count = re.search(r"\+(\d+)(,(\d+))?", line).groups()
        start, count = int(start), int(count) if count is not None else 1
        changed.setdefault(current, set()).update(range(start, start + count))
for new in git("ls-files", "--others", "--exclude-standard").splitlines():
    changed[new] = None

for file, line, lint in sorted(findings):
    lines = changed.get(file, ())
    if lines is None or line in lines:
        print(f"{file}:{line}: {lint}")
EOF
