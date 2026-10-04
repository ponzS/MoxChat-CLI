#!/bin/sh
# Download the published binary; no compiler or Rust installation is needed.
set -eu
repository=https://github.com/ponzS/MoxChat-CLI
case "$(uname -s)/$(uname -m)" in
  Darwin/arm64|Darwin/aarch64) target=aarch64-apple-darwin ;;
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  Linux/x86_64|Linux/amd64) target=x86_64-unknown-linux-gnu ;;
  Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-gnu ;;
  *) echo 'Unsupported system. On Windows, use the PowerShell installer with WSL2.' >&2; exit 1 ;;
esac
for tool in curl tar mktemp; do
  command -v "$tool" >/dev/null 2>&1 || { echo "Required system tool: $tool" >&2; exit 1; }
done
run_dir=$(mktemp -d "${TMPDIR:-/tmp}/mox-install.XXXXXXXX")
stage=''
cleanup() {
  if [ -n "$stage" ]; then rm -f -- "$stage"; fi
  if [ -f "$run_dir/.mox-install-owned" ]; then rm -rf -- "$run_dir"; fi
}
touch "$run_dir/.mox-install-owned"
trap cleanup EXIT
trap 'exit 130' INT TERM
fetch() { curl --proto '=https' --tlsv1.2 --connect-timeout 15 --max-time 900 --retry 2 -fsSL "$1" -o "$2"; }
tag=${MOX_VERSION:-}
if [ -z "$tag" ]; then
  latest=$(curl --proto '=https' --tlsv1.2 --connect-timeout 15 --max-time 60 -fsSL -o /dev/null -w '%{url_effective}' "$repository/releases/latest")
  case "$latest" in "$repository/releases/tag/"*) tag=${latest##*/} ;; *) echo 'No stable Mox release found.' >&2; exit 1 ;; esac
fi
if ! printf '%s\n' "$tag" | grep -Eq '^mox-v[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo 'Invalid release version. Use mox-vX.Y.Z.' >&2; exit 1
fi
asset="mox-$target.tar.gz"
base="$repository/releases/download/$tag"
echo "Downloading $tag for $target..."
fetch "$base/$asset" "$run_dir/$asset"
fetch "$base/SHA256SUMS" "$run_dir/SHA256SUMS"
expected=$(awk -v name="$asset" '$2 == name {print $1}' "$run_dir/SHA256SUMS")
if command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$run_dir/$asset" | awk '{print $1}')
elif command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$run_dir/$asset" | awk '{print $1}')
else
  echo 'Required system tool: shasum or sha256sum' >&2; exit 1
fi
[ "$actual" = "$expected" ] || { echo 'Download checksum mismatch; installation cancelled.' >&2; exit 1; }
[ "$(tar -tzf "$run_dir/$asset")" = mox ] || { echo 'Invalid release archive.' >&2; exit 1; }
install_dir=${MOX_INSTALL_DIR:-"$HOME/.local/bin"}
mkdir -p "$install_dir"
stage=$(mktemp "$install_dir/.mox-install.XXXXXXXX")
tar -xOzf "$run_dir/$asset" mox > "$stage"
chmod 755 "$stage"
installed_version=$("$stage" version)
[ "$installed_version" = "${tag#mox-v}" ] || { echo 'Binary version mismatch; installation cancelled.' >&2; exit 1; }
mv -f -- "$stage" "$install_dir/mox"
stage=''
case ":$PATH:" in
  *":$install_dir:"*) ;;
  *)
    # Only the default directory is added automatically; custom paths are explicit.
    if [ "$install_dir" = "$HOME/.local/bin" ]; then
      case "${SHELL:-}" in */zsh) profile="$HOME/.zshrc" ;; */bash) profile="$HOME/.bashrc" ;; *) profile="$HOME/.profile" ;; esac
      line='export PATH="$HOME/.local/bin:$PATH"'
      if ! grep -Fqx "$line" "$profile" 2>/dev/null; then printf '\n%s\n' "$line" >> "$profile"; fi
      echo 'Reopen your terminal to use mox.'
    else
      printf 'Add %s to PATH to use mox.\n' "$install_dir"
    fi ;;
esac
printf 'Installed: %s/mox\nNext: mox login, then mox start\n' "$install_dir"
