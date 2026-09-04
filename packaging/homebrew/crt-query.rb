# Homebrew formula for crt-query. GENERATED — do not edit by hand.
#
# Regenerate after a release with `just homebrew-formula`, then copy this
# file into the tap repository as Formula/crt-query.rb. The tap has to be a
# PUBLIC repo named homebrew-tap for
# `brew install tiredithumans/tap/crt-query` to resolve.
#
# A binary formula, not a source build: it installs the very archives the
# release publishes, checked against the same SHA256SUMS a manual install
# would verify. Homebrew also strips the macOS quarantine attribute from what
# it downloads, so there is no `xattr` step for this install path.
class CrtQuery < Formula
  desc "Query crt.sh certificate-transparency data from its public PostgreSQL database"
  homepage "https://github.com/tiredithumans/crt-query"
  # No `version` stanza: Homebrew scans it out of the URLs below, and
  # declaring it as well is a `brew audit --strict` failure.
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.5.0/crt-query-v0.5.0-aarch64-apple-darwin.tar.gz"
      sha256 "5c417cde00ec2fe938e76c5c27e973238ec96f5f35de40a0d3b53659e50cae1f"
    end
    on_intel do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.5.0/crt-query-v0.5.0-x86_64-apple-darwin.tar.gz"
      sha256 "c4419872b378db63789793e506cc1da49280cdfe61c312dd397f2823154eeae4"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.5.0/crt-query-v0.5.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "46032efa4678b9b3ca864ff997ef9f3f31e3fd4df2226ea9c7abdce2c377ff9c"
    end
    on_arm do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.5.0/crt-query-v0.5.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "a9bfb07a0458741224db25b131962391f57df49fdf37095d00bdf6666426d61a"
    end
  end

  def install
    bin.install "crt-query"
    # `crt-query completions <shell>` takes the shell as a bare argument, which
    # is this helper's default parameter form.
    generate_completions_from_executable(bin/"crt-query", "completions")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/crt-query --version")
    # Offline by design: crt.sh is a shared public service on donated
    # infrastructure, and a formula test must not depend on it being up.
    assert_match "certificate-transparency", shell_output("#{bin}/crt-query --help")
  end
end
