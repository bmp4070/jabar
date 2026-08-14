#!/usr/bin/env bash
# Rebuilds third_party/tinyjson/tinyjson.jar from its reference sources.
#
# The jar is checked in and consumed through java_import so that jabar sees a
# genuine binary-only dependency -- classfiles with no source alongside them,
# which is how the bulk of a real Java megarepo's classpath arrives.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tj="$here/third_party/tinyjson"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

javac --release 21 -d "$work" $(find "$tj/_src_reference" -name '*.java')
jar --create --file "$tj/tinyjson.jar" -C "$work" .
echo "wrote $tj/tinyjson.jar"
