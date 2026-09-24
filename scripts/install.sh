#!/bin/sh
set -eu

usage() {
  cat <<'EOF'
Usage: scripts/install.sh VERSION [INSTALL_DIR]

Install a specific Jabar release after verifying its published SHA256 checksum.
VERSION may be written as 0.1.0 or v0.1.0. INSTALL_DIR defaults to
$JABAR_INSTALL_DIR or $HOME/.local/bin.

Environment:
  JABAR_TARGET       Override the detected Rust target triple.
  JABAR_INSTALL_DIR  Default installation directory.
  JABAR_RELEASE_URL  Override https://github.com/bmp4070/jabar/releases/download.
EOF
}

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  usage >&2
  exit 2
fi

case "$1" in
  v*) tag=$1 ;;
  *) tag="v$1" ;;
esac
version=${tag#v}
case "$version" in
  '' | *[!0-9A-Za-z.+-]*)
    echo "invalid release version: $1" >&2
    exit 2
    ;;
esac

if [ "$#" -eq 2 ]; then
  install_dir=$2
else
  install_dir=${JABAR_INSTALL_DIR:-"${HOME}/.local/bin"}
fi

if [ -n "${JABAR_TARGET:-}" ]; then
  target=$JABAR_TARGET
else
  os=$(uname -s)
  arch=$(uname -m)
  case "${os}:${arch}" in
    Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux:aarch64 | Linux:arm64) target=aarch64-unknown-linux-gnu ;;
    Darwin:x86_64) target=x86_64-apple-darwin ;;
    Darwin:arm64 | Darwin:aarch64) target=aarch64-apple-darwin ;;
    *)
      echo "unsupported host ${os}/${arch}; set JABAR_TARGET explicitly" >&2
      exit 1
      ;;
  esac
fi

case "$target" in
  x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu | x86_64-apple-darwin | aarch64-apple-darwin) ;;
  *)
    echo "unsupported release target: $target" >&2
    exit 1
    ;;
esac

command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }
command -v tar >/dev/null 2>&1 || { echo "tar is required" >&2; exit 1; }
command -v install >/dev/null 2>&1 || { echo "install is required" >&2; exit 1; }

artifact="jabar-${tag}-${target}.tar.gz"
release_root=${JABAR_RELEASE_URL:-https://github.com/bmp4070/jabar/releases/download}
url="${release_root}/${tag}"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/jabar-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
  --output "${tmp}/${artifact}" "${url}/${artifact}"
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
  --output "${tmp}/SHA256SUMS" "${url}/SHA256SUMS"

matches=$(awk -v file="$artifact" '$2 == file { count++ } END { print count + 0 }' "${tmp}/SHA256SUMS")
if [ "$matches" -ne 1 ]; then
  echo "expected exactly one checksum for ${artifact}, found ${matches}" >&2
  exit 1
fi
expected=$(awk -v file="$artifact" '$2 == file { print $1 }' "${tmp}/SHA256SUMS")
case "$expected" in
  '' | *[!0-9a-fA-F]*)
    echo "no valid SHA256 checksum for ${artifact}" >&2
    exit 1
    ;;
esac
if [ "${#expected}" -ne 64 ]; then
  echo "invalid SHA256 checksum length for ${artifact}" >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "${tmp}/${artifact}" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "${tmp}/${artifact}" | awk '{ print $1 }')
else
  echo "sha256sum or shasum is required" >&2
  exit 1
fi

if [ "$actual" != "$expected" ]; then
  echo "SHA256 verification failed for ${artifact}" >&2
  exit 1
fi

contents=$(tar -tzf "${tmp}/${artifact}" | LC_ALL=C sort)
expected_contents=$(printf '%s\n' LICENSE NOTICE jabar | LC_ALL=C sort)
if [ "$contents" != "$expected_contents" ]; then
  echo "archive contains unexpected paths; refusing to extract" >&2
  exit 1
fi
if ! tar -tvzf "${tmp}/${artifact}" | awk '
  NF == 0 || substr($1, 1, 1) != "-" { bad = 1 }
  END { exit bad }
'; then
  echo "archive contains a link or non-regular member; refusing to extract" >&2
  exit 1
fi

mkdir "${tmp}/unpack"
tar -xzf "${tmp}/${artifact}" -C "${tmp}/unpack"
mkdir -p "$install_dir"
install -m 0755 "${tmp}/unpack/jabar" "${install_dir}/jabar"
echo "installed jabar ${tag} to ${install_dir}/jabar"
