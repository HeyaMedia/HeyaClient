#!/bin/sh
set -eu

app_bundle=${1:-}
if [ -z "$app_bundle" ]; then
  echo "usage: $0 /path/to/Heya.app" >&2
  exit 2
fi

if [ ! -d "$app_bundle/Contents/MacOS" ]; then
  echo "not a macOS application bundle: $app_bundle" >&2
  exit 2
fi

frameworks="$app_bundle/Contents/Frameworks"
private_libs="$app_bundle/Contents/MacOS/lib"
if [ ! -d "$frameworks" ] && [ ! -d "$private_libs" ]; then
  echo "missing bundled native library directory" >&2
  exit 1
fi

scan_frameworks=$frameworks
scan_private_libs=$private_libs
[ -d "$scan_frameworks" ] || scan_frameworks=$private_libs
[ -d "$scan_private_libs" ] || scan_private_libs=$frameworks

if ! find "$scan_frameworks" "$scan_private_libs" -type f -name 'libmpv*.dylib' -print -quit | grep -q .; then
  echo "missing bundled libmpv dylib" >&2
  exit 1
fi

if ! find "$app_bundle/Contents/MacOS" "$scan_frameworks" -type f -exec sh -c '
  for binary do
    if ! file "$binary" | grep -q "Mach-O"; then
      continue
    fi

    unsafe_dependencies=$(otool -L "$binary" | grep -E "^[[:space:]]+(/opt/homebrew|/usr/local|/Users|/private/tmp|/tmp)/" || true)
    if [ -n "$unsafe_dependencies" ]; then
      echo "unsafe dynamic-library references in $binary:" >&2
      echo "$unsafe_dependencies" >&2
      exit 1
    fi
  done
' sh {} +; then
  exit 1
fi

codesign --verify --deep --strict "$app_bundle"
echo "native bundle verification passed: $app_bundle"
