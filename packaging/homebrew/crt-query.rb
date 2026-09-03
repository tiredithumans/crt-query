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
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.2.0/crt-query-v0.2.0-aarch64-apple-darwin.tar.gz"
      sha256 "194de5f7e536d7b181374f48f3a1026c0065b049dc64f128d40f3f607bee7b46"
    end
    on_intel do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.2.0/crt-query-v0.2.0-x86_64-apple-darwin.tar.gz"
      sha256 "7b6d4192357c02dece3f04eca1d79cf12ac98e1df24ab6530539d3dd41dd5513"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.2.0/crt-query-v0.2.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "45047feca1e0dea24cd9c8f5b2898990b26463017da2d7b74ae01044bd8bb71b"
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
