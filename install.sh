#!/bin/sh
# crt-query installer for Linux and macOS.
#
# Detects your target triple, resolves the newest release (so there is no
# version to keep up to date here or in the README), verifies the archive
# against that release's SHA256SUMS, and installs the binary.
#
#   curl -fsSL https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.sh | sh
#
# Re-run it to upgrade: it overwrites the installed binary in place and, on
# macOS, clears the Gatekeeper quarantine attribute again — a fresh download
# arrives quarantined every time, not just on the first install.
#
# Options. When piping, pass them after `sh -s --`:
#
#   curl -fsSL .../install.sh | sh -s -- --dir "$HOME/.local/bin"
#
#   --dir <path>       install directory (default: /usr/local/bin)
#   --version <vX.Y.Z> install this release instead of the newest one
#   --help             print this and exit
#
# Environment: CRT_QUERY_DIR and CRT_QUERY_VERSION are read as defaults for
# the two options above.

set -eu

REPO="tiredithumans/crt-query"
BIN="crt-query"

dir="${CRT_QUERY_DIR:-/usr/local/bin}"
version="${CRT_QUERY_VERSION:-}"

die() {
    printf '%s: error: %s\n' "$BIN installer" "$1" >&2
    exit 1
}

note() {
    printf '%s\n' "$1" >&2
}

usage() {
    cat <<'USAGE'
crt-query installer for Linux and macOS.

Detects your target triple, resolves the newest release, verifies the archive
against that release's SHA256SUMS, and installs the binary. Re-run it to
upgrade.

  curl -fsSL https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.sh | sh

Options (when piping, pass them after `sh -s --`):
  --dir <path>        install directory (default: /usr/local/bin)
  --version <vX.Y.Z>  install this release instead of the newest one
  --help              print this and exit

Environment: CRT_QUERY_DIR and CRT_QUERY_VERSION supply defaults for those.
USAGE
}

need() {
    command -v "$1" >/dev/null 2>&1 || die "$1 is required but was not found on PATH"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --dir)
            [ "$#" -ge 2 ] || die "--dir needs a path"
            dir="$2"
            shift 2
            ;;
        --dir=*)
            dir="${1#--dir=}"
            shift
            ;;
        --version)
            [ "$#" -ge 2 ] || die "--version needs a release tag, e.g. v0.1.0"
            version="$2"
            shift 2
            ;;
        --version=*)
            version="${1#--version=}"
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1 (try --help)"
            ;;
    esac
done

need curl
need tar
need uname

# The checksum tool differs between distributions and macOS; either will do.
if command -v sha256sum >/dev/null 2>&1; then
    checksum="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
    checksum="shasum -a 256"
else
    die "neither sha256sum nor shasum was found, so the download cannot be verified"
fi

# --- Target triple ---------------------------------------------------------

os=$(uname -s)
case "$os" in
    Linux) os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    MINGW* | MSYS* | CYGWIN*)
        die "this script is for Linux and macOS; on Windows use install.ps1"
        ;;
    *) die "unsupported operating system: $os" ;;
esac

machine=$(uname -m)
case "$machine" in
    x86_64 | amd64) cpu="x86_64" ;;
    arm64 | aarch64) cpu="aarch64" ;;
    *) die "unsupported CPU architecture: $machine" ;;
esac

target="$cpu-$os_part"

# --- Release ---------------------------------------------------------------

if [ -n "$version" ]; then
    base="https://github.com/$REPO/releases/download/$version"
else
    # GitHub redirects /releases/latest/download/<asset> to the newest
    # release's copy of that asset, so the version never has to be resolved
    # through the API — and this stays inside the download host rather than
    # spending one of api.github.com's unauthenticated rate-limit slots.
    base="https://github.com/$REPO/releases/latest/download"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

fetch() {
    curl --fail --silent --show-error --location --retry 2 --output "$2" "$1" ||
        die "could not download $1"
}

note "Resolving the ${version:-latest} $BIN release..."
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"

# SHA256SUMS names every archive in the release, so it doubles as the index
# that maps this machine's target triple to an archive — and therefore to the
# version, which is embedded in the archive name. A leading `*` (a binary-mode
# entry) or `./` (how the release workflow's glob spells the name) is not part
# of that name.
archive=$(awk -v suffix="-$target.tar.gz" '
    { name = $2; sub(/^\*?(\.\/)?/, "", name) }
    index(name, suffix) && substr(name, length(name) - length(suffix) + 1) == suffix { print name }
' "$tmp/SHA256SUMS")

if [ -z "$archive" ]; then
    available=$(awk '{ name = $2; sub(/^\*?(\.\/)?/, "", name); printf "  %s\n", name }' "$tmp/SHA256SUMS")
    die "the ${version:-latest} release has no build for $target.
It ships:
$available
Build from source instead:
  cargo install --git https://github.com/$REPO"
fi

if [ "$(printf '%s\n' "$archive" | wc -l)" -gt 1 ]; then
    die "SHA256SUMS lists more than one archive for $target; refusing to guess"
fi

# crt-query-v0.1.0-x86_64-unknown-linux-gnu.tar.gz -> v0.1.0. The prefix and
# suffix go through variables of their own because bash 3.2 — still /bin/sh on
# macOS — mis-parses a quoted expansion nested inside a quoted ${...%...}.
stem="${archive%.tar.gz}"
name_prefix="$BIN-"
name_suffix="-$target"
resolved=${stem#"$name_prefix"}
resolved=${resolved%"$name_suffix"}

note "Downloading $BIN $resolved for $target..."
fetch "$base/$archive" "$tmp/$archive"

# --- Verify ----------------------------------------------------------------

# One line, so the check covers exactly what was downloaded and cannot pass
# by skipping an entry. Run from $tmp because the paths in SHA256SUMS are
# relative to the release directory.
awk -v name="$archive" '
    { entry = $2; sub(/^\*?(\.\/)?/, "", entry) }
    entry == name { print }
' "$tmp/SHA256SUMS" > "$tmp/expected"
[ -s "$tmp/expected" ] || die "no SHA256SUMS entry for $archive"
(cd "$tmp" && $checksum --check --status expected) ||
    die "checksum mismatch for $archive — the download does not match the release's SHA256SUMS, so it was NOT installed"
note "Checksum verified against the release's SHA256SUMS."

tar -xzf "$tmp/$archive" -C "$tmp"
[ -f "$tmp/$stem/$BIN" ] || die "$archive does not contain $stem/$BIN"

# --- Install ---------------------------------------------------------------

# Elevate only when the destination genuinely needs it, and only for the two
# commands that touch it.
as_root=""
if [ ! -d "$dir" ]; then
    if mkdir -p "$dir" 2>/dev/null; then
        :
    elif command -v sudo >/dev/null 2>&1; then
        as_root="sudo"
        note "Creating $dir (needs sudo)..."
        sudo mkdir -p "$dir" || die "could not create $dir"
    else
        die "$dir does not exist and could not be created; pass --dir to pick a writable directory"
    fi
fi

if [ ! -w "$dir" ]; then
    command -v sudo >/dev/null 2>&1 ||
        die "$dir is not writable and sudo was not found; pass --dir to pick a writable directory"
    as_root="sudo"
    note "Installing to $dir (needs sudo)..."
fi

$as_root install -m 0755 "$tmp/$stem/$BIN" "$dir/$BIN" ||
    die "could not install to $dir/$BIN"

# The release binaries are unsigned, so macOS quarantines every download.
# Clearing it here is what makes a re-run work as an upgrade: the attribute
# comes back with each new archive.
if [ "$os" = "Darwin" ] && command -v xattr >/dev/null 2>&1; then
    $as_root xattr -d com.apple.quarantine "$dir/$BIN" 2>/dev/null || true
fi

installed=$("$dir/$BIN" --version 2>/dev/null || echo "")
[ -n "$installed" ] || die "installed $dir/$BIN but it would not run"

note "Installed $installed to $dir/$BIN"

case ":$PATH:" in
    *":$dir:"*) ;;
    *) note "Note: $dir is not on your PATH. Add it, or run $dir/$BIN directly." ;;
esac

note "Shell completions: $BIN completions zsh|bash|fish  (see the README)"
