#!/usr/bin/env bash
# Build one native CLI release asset without publishing it.
set -euo pipefail
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="$repo_root/moxchat-cli/Cargo.toml"
if [[ -f "$repo_root/Cargo.toml" ]]; then manifest="$repo_root/Cargo.toml"; fi
output_dir=""
existing_binary=""
while (($#)); do
  case "$1" in
    --out) output_dir="${2:?--out requires a directory}"; shift 2 ;;
    --binary) existing_binary="${2:?--binary requires a binary path}"; shift 2 ;;
    -h|--help) echo 'Usage: scripts/mox-cli-build.sh --out <directory> [--binary <existing native mox>]'; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done
if [[ -z "$output_dir" ]]; then echo '--out is required' >&2; exit 2; fi
host_target="$(rustc -vV | sed -n 's/^host: //p')"
case "$host_target" in
  aarch64-apple-darwin|x86_64-apple-darwin|aarch64-unknown-linux-gnu|x86_64-unknown-linux-gnu) ;;
  *) echo "Unsupported release target: $host_target" >&2; exit 1 ;;
esac
run_dir="$(mktemp -d "${TMPDIR:-/tmp}/mox-cli-build.XXXXXXXX")"
cleanup() {
  if [[ -d "$run_dir" && -f "$run_dir/.mox-build-owned" ]]; then
    rm -rf -- "$run_dir"
  fi
}
trap cleanup EXIT
: > "$run_dir/.mox-build-owned"
if [[ -z "$existing_binary" ]]; then
  build_target_dir="${CARGO_TARGET_DIR:-$run_dir/target}"
  if [[ "$build_target_dir" != /* ]]; then build_target_dir="$PWD/$build_target_dir"; fi
  CARGO_TARGET_DIR="$build_target_dir" cargo build --manifest-path "$manifest" --locked --release
  existing_binary="$build_target_dir/release/mox"
fi
python3 - "$existing_binary" "$output_dir" "$host_target" "$manifest" <<'PY'
import gzip, hashlib, io, json, pathlib, re, subprocess, sys, tarfile
binary, output, target, manifest = sys.argv[1:]
binary = pathlib.Path(binary).resolve(strict=True)
version = re.search(r'^version\s*=\s*"([^"]+)"', pathlib.Path(manifest).read_text(), re.M).group(1)
actual = json.loads(subprocess.check_output([str(binary), 'version', '--json'], text=True, timeout=10))['version']
if actual != version:
    raise SystemExit(f'Binary version {actual} differs from manifest {version}')
output = pathlib.Path(output).resolve()
output.mkdir(parents=True, exist_ok=True)
name = f'mox-{target}.tar.gz'
archive = output / name
if archive.exists():
    raise SystemExit(f'Refusing to overwrite existing release asset: {archive}')
data = binary.read_bytes()
with archive.open('xb') as raw, gzip.GzipFile(fileobj=raw, filename='', mode='wb', mtime=0) as compressed, tarfile.open(fileobj=compressed, mode='w|', format=tarfile.USTAR_FORMAT) as tar:
    item = tarfile.TarInfo('mox')
    item.size = len(data)
    item.mode = 0o755
    item.mtime = 0
    tar.addfile(item, io.BytesIO(data))
sums = output / 'SHA256SUMS'
previous = sums.read_text() if sums.exists() else ''
lines = [line for line in previous.splitlines() if not line.endswith('  ' + name)]
lines.append(hashlib.sha256(archive.read_bytes()).hexdigest() + '  ' + name)
sums.write_text('\n'.join(sorted(lines)) + '\n')
print(f'Prepared mox-v{version}: {archive}')
print(f'Checksums: {sums}')
PY
