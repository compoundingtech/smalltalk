#!/bin/sh
# Download a verified Small Talk release, install it, and open st.
set -eu
base=${ST_INSTALL_RELEASE_BASE_URL:-https://github.com/compoundingtech/smalltalk/releases}
tag=
bin_dir=${HOME:?HOME is required}/.local/bin
run=true
while [ "$#" -gt 0 ]; do
    case "$1" in
        --tag) tag=${2:?--tag needs a release tag}; shift 2 ;;
        --bin-dir) bin_dir=${2:?--bin-dir needs a directory}; shift 2 ;;
        --no-run) run=false; shift ;;
        -h|--help) printf 'Usage: install.sh [--tag TAG] [--bin-dir DIRECTORY] [--no-run]\n'; exit 0 ;;
        *) printf 'install: unknown option: %s\n' "$1" >&2; exit 2 ;;
    esac
done
case "$(uname -s):$(uname -m)" in
    Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
    Darwin:arm64) target=aarch64-apple-darwin ;;
    *) printf 'install: supported platforms are Linux x86_64 and Apple Silicon macOS\n' >&2; exit 1 ;;
esac
case "$tag" in *[!A-Za-z0-9._-]*) printf 'install: invalid release tag\n' >&2; exit 2 ;; esac
case "$base" in
    https://*) protocol=https ;;
    http://127.0.0.1:*|http://localhost:*) protocol=https,http ;;
    *) printf 'install: release downloads need HTTPS\n' >&2; exit 2 ;;
esac
if [ -n "$tag" ]; then channel=download/$tag; else channel=latest/download; fi
package=smalltalk-$target
archive=$package.tar.gz
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 1' HUP INT TERM
for name in "$archive" "$archive.sha256"; do
    curl --fail --silent --show-error --location --proto "=$protocol" --proto-redir "=$protocol" \
        "$base/$channel/$name" --output "$work/$name"
done
read -r expected filename extra < "$work/$archive.sha256"
case "$expected" in ''|*[!0-9a-fA-F]*) printf 'install: invalid archive checksum\n' >&2; exit 1 ;; esac
[ "${#expected}" -eq 64 ] && [ "${filename#\*}" = "$archive" ] && [ -z "$extra" ] || {
    printf 'install: invalid archive checksum\n' >&2; exit 1;
}
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$work/$archive")
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$work/$archive")
else
    printf 'install: sha256sum or shasum is required\n' >&2; exit 1
fi
actual=${actual%% *}
expected=$(printf '%s' "$expected" | tr A-F a-f)
[ "$actual" = "$expected" ] || { printf 'install: archive checksum mismatch\n' >&2; exit 1; }
tar -tzf "$work/$archive" > "$work/members"
while IFS= read -r member; do
    case "$member" in
        "$package"|"$package/"*) ;;
        *) printf 'install: archive contains a path outside its package\n' >&2; exit 1 ;;
    esac
    case "/$member/" in */../*) printf 'install: archive contains a parent path\n' >&2; exit 1 ;; esac
done < "$work/members"
tar -xzf "$work/$archive" -C "$work"
# Older retained releases predate --quiet; keep their installer spelling and
# suppress notices here while preserving failures and stderr.
"$work/$package/install.sh" --bin-dir "$bin_dir" >/dev/null
bin_dir=$(cd "$bin_dir" && pwd)
rm -rf "$work"
trap - EXIT HUP INT TERM
if "$run"; then
    # curl | sh leaves stdin on the script pipe; the terminal UI needs the person's terminal.
    if [ -t 1 ] && [ -r /dev/tty ]; then
        exec "$bin_dir/st" </dev/tty
    fi
    exec "$bin_dir/st"
fi
