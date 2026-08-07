#!/usr/bin/env bash
# Shared helpers for scripts/inspect/*.sh.
#
# This directory is developer tooling, not a cargo test target: its job is to
# produce images a human looks at, never to assert pass/fail on pixels. The
# one thing every script here must still get right is failing loudly — a
# silently-empty or silently-wrong image is worse than a crash, because a
# human trusts the picture. Hence `require` and `die` below, and hence every
# script sourcing this file starts with `set -euo pipefail`.

# Fails with a clear message naming exactly which external tool is missing,
# rather than letting a bare "command not found" three frames deep in a
# pipeline stand in for a diagnosis.
require() {
	for tool in "$@"; do
		if ! command -v "$tool" >/dev/null 2>&1; then
			echo "inspect: missing required tool '$tool' (see scripts/inspect/README.md for what each script needs)" >&2
			exit 1
		fi
	done
}

die() {
	echo "inspect: $*" >&2
	exit 1
}

log() {
	echo "inspect: $*" >&2
}

# Repo root, regardless of the caller's cwd — `just inspect` runs recipes
# from the justfile's directory already, but a human running this script
# directly from scripts/inspect/ should get the same result.
repo_root() {
	cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd
}

# ImageMagick, vhs and other tools on this machine live in Homebrew's
# prefix, which a Nix-managed PATH may not include. Add it unconditionally
# rather than requiring the caller to fix their environment first.
case ":${PATH:-}:" in
	*/opt/homebrew/bin:*) ;;
	*) PATH="/opt/homebrew/bin:$PATH" ;;
esac
export PATH

ROOT="$(repo_root)"
OUT="$ROOT/inspect-out"
TAPE_DVD="$ROOT/tests/tapes/macchina.dvd"
TAPE_VHS="$ROOT/tests/tapes/macchina.vhs.tape"

# The two tapes hold Width/Height/Padding/Theme fixed so the renderer is the
# only variable (see the header comments in both tapes). Scripts that need
# those numbers outside the tape parser read them straight out of the tape
# text below rather than hard-coding a second copy that could drift.
tape_setting() {
	local key="$1" file="$2"
	grep -E "^Set[[:space:]]+${key}[[:space:]]" "$file" | head -1 | sed -E "s/^Set[[:space:]]+${key}[[:space:]]+\"?([^\"]*)\"?.*/\1/"
}

mkdir -p "$OUT"

# ImageMagick on this machine (Homebrew 7.1.2, freetype-only, no fontconfig)
# cannot resolve font *names* — `-font Menlo` fails even though the family
# is installed — so every script that draws text points `-font` at this
# file directly. Menlo, because it is the font both tapes ask for.
MENLO_FONT="/System/Library/Fonts/Menlo.ttc"
