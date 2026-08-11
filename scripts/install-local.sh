#!/bin/sh
set -eu

usage() {
	printf '%s\n' "Usage: $0 [--force]" >&2
	exit 1
}

case "$#" in
	0) force=0 ;;
	1) [ "$1" = "--force" ] || usage; force=1 ;;
	*) usage ;;
esac

if [ -z "${HOME:-}" ]; then
	printf '%s\n' 'HOME must be set.' >&2
	exit 1
fi

repo_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd)
destination="$HOME/.local/bin/prime-agent"

mkdir -p "$HOME/.local/bin"

if [ -d "$destination" ]; then
	printf '%s\n' "Destination is a directory: $destination" >&2
	exit 1
fi

if [ -e "$destination" ] || [ -L "$destination" ]; then
	if [ "$force" -ne 1 ]; then
		printf '%s\n' "Command already exists: $destination (use --force to replace it)" >&2
		exit 1
	fi
fi

(cd "$repo_root" && npm run build)

temporary=$(mktemp "$destination.XXXXXX")
trap 'rm -f "$temporary"' 0 HUP INT TERM
launcher=$(printf '%s' "$repo_root/prime-agent.sh" | sed "s/'/'\\''/g")
{
	printf '%s\n' '#!/bin/sh'
	printf "exec '%s' --dist \"\$@\"\n" "$launcher"
} > "$temporary"
chmod 755 "$temporary"
mv -f "$temporary" "$destination"
temporary=
trap - 0 HUP INT TERM

printf '%s\n' "Installed $destination"
case ":${PATH:-}:" in
	*":$HOME/.local/bin:"*) ;;
	*) printf '%s\n' "export PATH=\"$HOME/.local/bin:\$PATH\"" ;;
esac
