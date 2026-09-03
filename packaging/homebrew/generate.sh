#!/usr/bin/env bash
# Regenerate packaging/homebrew/crt-query.rb from a published release.
#
#   packaging/homebrew/generate.sh [vX.Y.Z]   (default: the latest release)
#
# Every checksum in the formula is copied straight out of the release's
# SHA256SUMS, so the formula cannot drift from the archives it points at, and
# nothing here has to be typed by hand at release time.
set -euo pipefail

REPO="tiredithumans/crt-query"
OUT="$(cd "$(dirname "$0")" && pwd)/crt-query.rb"

version="${1:-}"
if [ -n "$version" ]; then
    base="https://github.com/$REPO/releases/download/$version"
else
    base="https://github.com/$REPO/releases/latest/download"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl --fail --silent --show-error --location --output "$tmp/SHA256SUMS" "$base/SHA256SUMS"

# hash for a target's tarball, by suffix match on the archive name
sum_for() {
    awk -v suffix="-$1.tar.gz" '
        { name = $2; sub(/^\*?(\.\/)?/, "", name) }
        substr(name, length(name) - length(suffix) + 1) == suffix { print $1 }
    ' "$tmp/SHA256SUMS"
}

archive_for() {
    awk -v suffix="-$1.tar.gz" '
        { name = $2; sub(/^\*?(\.\/)?/, "", name) }
        substr(name, length(name) - length(suffix) + 1) == suffix { print name }
    ' "$tmp/SHA256SUMS"
}

# `crt-query completions` first shipped in this release. Homebrew generates the
# completion scripts by RUNNING the installed binary, so emitting that call for
# an older release does not degrade gracefully -- it aborts `brew install`
# outright. Nothing in this repo's gates catches it, because none of them runs
# the released binary.
MIN_COMPLETIONS_VERSION="0.2.0"

ARM_MAC="aarch64-apple-darwin"
INTEL_MAC="x86_64-apple-darwin"
INTEL_LINUX="x86_64-unknown-linux-gnu"
ARM_LINUX="aarch64-unknown-linux-gnu"

for target in "$ARM_MAC" "$INTEL_MAC" "$INTEL_LINUX" "$ARM_LINUX"; do
    [ -n "$(sum_for "$target")" ] ||
        { echo "error: the ${version:-latest} release has no $target archive" >&2; exit 1; }
done

# The tag is embedded in every archive name, so the release resolved above
# names itself and no second request is needed to find out which one it was.
archive=$(archive_for "$ARM_MAC")
stem="${archive%.tar.gz}"
# Unquoted assignment, quoted pattern: bash 3.2 mis-parses a quoted expansion
# nested inside a quoted ${...%...}, and it is still /bin/bash on macOS.
arm_suffix="-$ARM_MAC"
tag=${stem#crt-query-}
tag=${tag%"$arm_suffix"}
# Cargo versions have no `v`; Homebrew's `version` field matches them.
plain="${tag#v}"

url_for() {
    echo "https://github.com/$REPO/releases/download/$tag/$(archive_for "$1")"
}

# Lowest version sorts first, so the minimum leading means this release is at
# or above it.
oldest=$(printf '%s\n%s\n' "$MIN_COMPLETIONS_VERSION" "$plain" | sort -V | head -1)
if [ "$oldest" = "$MIN_COMPLETIONS_VERSION" ]; then
    completions=$(
        cat <<'BLOCK'
    # `crt-query completions <shell>` takes the shell as a bare argument, which
    # is this helper's default parameter form.
    generate_completions_from_executable(bin/"crt-query", "completions")
BLOCK
    )
else
    completions=$(
        cat <<BLOCK
    # No completions here: \`crt-query completions\` arrived in v$MIN_COMPLETIONS_VERSION,
    # and Homebrew generates them by running the installed binary, so calling
    # it against $tag would abort the install.
BLOCK
    )
fi

cat > "$OUT" <<RUBY
# Homebrew formula for crt-query. GENERATED — do not edit by hand.
#
# Regenerate after a release with \`just homebrew-formula\`, then copy this
# file into the tap repository as Formula/crt-query.rb. The tap has to be a
# PUBLIC repo named homebrew-tap for
# \`brew install tiredithumans/tap/crt-query\` to resolve.
#
# A binary formula, not a source build: it installs the very archives the
# release publishes, checked against the same SHA256SUMS a manual install
# would verify. Homebrew also strips the macOS quarantine attribute from what
# it downloads, so there is no \`xattr\` step for this install path.
class CrtQuery < Formula
  desc "Query crt.sh certificate-transparency data from its public PostgreSQL database"
  homepage "https://github.com/$REPO"
  # No \`version\` stanza: Homebrew scans it out of the URLs below, and
  # declaring it as well is a \`brew audit --strict\` failure.
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "$(url_for "$ARM_MAC")"
      sha256 "$(sum_for "$ARM_MAC")"
    end
    on_intel do
      url "$(url_for "$INTEL_MAC")"
      sha256 "$(sum_for "$INTEL_MAC")"
    end
  end

  on_linux do
    on_intel do
      url "$(url_for "$INTEL_LINUX")"
      sha256 "$(sum_for "$INTEL_LINUX")"
    end
    on_arm do
      url "$(url_for "$ARM_LINUX")"
      sha256 "$(sum_for "$ARM_LINUX")"
    end
  end

  def install
    bin.install "crt-query"
$completions
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/crt-query --version")
    # Offline by design: crt.sh is a shared public service on donated
    # infrastructure, and a formula test must not depend on it being up.
    assert_match "certificate-transparency", shell_output("#{bin}/crt-query --help")
  end
end
RUBY

echo "wrote $OUT for $tag"
