#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
installer="${repo_root}/scripts/install.sh"
test_root=$(mktemp -d "${TMPDIR:-/tmp}/jabar-install-tests.XXXXXX")
trap 'rm -rf "$test_root"' EXIT

mkdir -p "${test_root}/bin"
cat > "${test_root}/bin/curl" <<'EOF'
#!/bin/sh
set -eu
output=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output) output=$2; shift 2 ;;
    --proto) shift 2 ;;
    --tlsv1.2) shift ;;
    --fail | --location | --silent | --show-error) shift ;;
    *) url=$1; shift ;;
  esac
done
cp "${MOCK_RELEASE_DIR}/${url##*/}" "$output"
EOF
chmod +x "${test_root}/bin/curl"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  else
    shasum -a 256 "$1" | awk '{ print $1 }'
  fi
}

make_archive() {
  local archive=$1
  local mode=$2
  python3 - "$archive" "$mode" <<'PY'
import io
import sys
import tarfile

archive, mode = sys.argv[1:]
with tarfile.open(archive, "w:gz") as out:
    def regular(name, data):
        payload = data.encode()
        info = tarfile.TarInfo(name)
        info.mode = 0o755 if name == "jabar" else 0o644
        info.size = len(payload)
        out.addfile(info, io.BytesIO(payload))

    regular("LICENSE", "license\n")
    regular("NOTICE", "notice\n")
    if mode == "symlink":
        info = tarfile.TarInfo("jabar")
        info.type = tarfile.SYMTYPE
        info.linkname = "LICENSE"
        out.addfile(info)
    elif mode == "hardlink":
        info = tarfile.TarInfo("jabar")
        info.type = tarfile.LNKTYPE
        info.linkname = "LICENSE"
        out.addfile(info)
    elif mode == "fifo":
        info = tarfile.TarInfo("jabar")
        info.type = tarfile.FIFOTYPE
        out.addfile(info)
    else:
        regular("jabar", "#!/bin/sh\necho jabar 0.1.0\n")

    if mode == "extra":
        regular("extra", "unexpected\n")
    elif mode == "traversal":
        regular("../escaped", "unexpected\n")
    elif mode == "duplicate":
        regular("jabar", "duplicate\n")
PY
}

run_case() {
  local name=$1
  local archive_mode=$2
  local checksum_mode=$3
  local expected=$4
  local release_dir="${test_root}/${name}/release"
  local install_dir="${test_root}/${name}/install"
  local artifact="jabar-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
  mkdir -p "$release_dir"
  make_archive "${release_dir}/${artifact}" "$archive_mode"
  local digest
  digest=$(sha256 "${release_dir}/${artifact}")
  case "$checksum_mode" in
    valid) printf '%s  %s\n' "$digest" "$artifact" > "${release_dir}/SHA256SUMS" ;;
    bad) printf '%064d  %s\n' 0 "$artifact" > "${release_dir}/SHA256SUMS" ;;
    missing) : > "${release_dir}/SHA256SUMS" ;;
    duplicate)
      printf '%s  %s\n%s  %s\n' "$digest" "$artifact" "$digest" "$artifact" \
        > "${release_dir}/SHA256SUMS"
      ;;
  esac

  if PATH="${test_root}/bin:${PATH}" \
    MOCK_RELEASE_DIR="$release_dir" \
    JABAR_TARGET=x86_64-unknown-linux-gnu \
    JABAR_RELEASE_URL=https://fixture.invalid \
    "$installer" 0.1.0 "$install_dir" >/dev/null 2>&1; then
    result=pass
  else
    result=fail
  fi
  if [ "$result" != "$expected" ]; then
    echo "${name}: expected ${expected}, got ${result}" >&2
    exit 1
  fi
  if [ "$expected" = pass ]; then
    test -x "${install_dir}/jabar"
    test "$("${install_dir}/jabar")" = "jabar 0.1.0"
  fi
}

run_case valid regular valid pass
run_case bad-digest regular bad fail
run_case missing-checksum regular missing fail
run_case duplicate-checksum regular duplicate fail
run_case extra-member extra valid fail
run_case traversal-member traversal valid fail
run_case symlink-member symlink valid fail
run_case hardlink-member hardlink valid fail
run_case special-member fifo valid fail
run_case duplicate-member duplicate valid fail

if PATH="${test_root}/bin:${PATH}" JABAR_TARGET=../../unsupported \
  "$installer" 0.1.0 "${test_root}/unsupported" >/dev/null 2>&1; then
  echo "unsupported target was accepted" >&2
  exit 1
fi

test ! -e "${test_root}/escaped"
echo "installer fixture tests passed"
