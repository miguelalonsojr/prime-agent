#!/bin/sh
set -eu

if [ "$#" -gt 1 ] || { [ "$#" -eq 1 ] && [ "$1" != --force ]; }; then
    printf 'Usage: %s [--force]\n' "$0" >&2
    exit 2
fi
if [ -z "${HOME:-}" ]; then
    printf 'HOME must be set\n' >&2
    exit 2
fi

root=$(CDPATH= cd -P "$(dirname "$0")/.." && pwd -P)
install_dir=$HOME/.local/bin
destination=$install_dir/prime-agent
if [ -d "$destination" ]; then
    printf 'Refusing to replace directory: %s\n' "$destination" >&2
    exit 1
fi
if { [ -e "$destination" ] || [ -L "$destination" ]; } && [ "${1:-}" != --force ]; then
    printf 'Already exists: %s (pass --force to replace)\n' "$destination" >&2
    exit 1
fi

(cd "$root" && cargo build --release -p pa-cli --locked)
case ${CARGO_TARGET_DIR:-} in
    '') binary=$root/target/release/prime-agent ;;
    /*) binary=$CARGO_TARGET_DIR/release/prime-agent ;;
    *) binary=$root/$CARGO_TARGET_DIR/release/prime-agent ;;
esac
if [ ! -f "$binary" ] || [ ! -x "$binary" ]; then
    printf 'Release binary missing or not executable: %s\n' "$binary" >&2
    exit 1
fi

mkdir -p "$install_dir"
temporary=$(mktemp "$install_dir/.prime-agent.XXXXXXXX")
trap 'rm -f "$temporary"' 0
trap 'exit 1' 1 2 3 15
quote() { printf '%s' "$1" | sed "s/'/'\\\\''/g"; }
quoted_root=$(quote "$root")
quoted_binary=$(quote "$binary")
printf '#!/bin/sh\nPI_PACKAGE_DIR=\047%s\047 exec \047%s\047 "$@"\n' "$quoted_root" "$quoted_binary" > "$temporary"
chmod 755 "$temporary"
if [ -d "$destination" ]; then
    printf 'Refusing to replace directory: %s\n' "$destination" >&2
    exit 1
fi
mv -f "$temporary" "$destination"
trap - 0 1 2 3 15
printf 'Installed %s\n' "$destination"
case :$PATH: in
    *:"$install_dir":*) ;;
    *) printf 'Add %s to your PATH.\n' "$install_dir" ;;
esac
