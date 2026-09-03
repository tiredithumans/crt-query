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
  version "0.1.0"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.1.0/crt-query-v0.1.0-aarch64-apple-darwin.tar.gz"
      sha256 "51d2378f182d8a6235ae8953f48b28409024eb61f63c094073bf3dd76dd945e0"
    end
    on_intel do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.1.0/crt-query-v0.1.0-x86_64-apple-darwin.tar.gz"
      sha256 "edc60e9062f7308f042ebcb678262d591c666ef17caf2e38a894680f5e27cbad"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.1.0/crt-query-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "a643ebf770b9ab63f08e99c159c010ce0dc6c8b09f5e035e8f048d1087c2d0a0"
    end
  end

  def install
    bin.install "crt-query"
    # `crt-query completions <shell>` takes the shell as a bare argument,
    # which is this helper's default parameter form.
    generate_completions_from_executable(bin/"crt-query", "completions")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/crt-query --version")
    # Offline by design: crt.sh is a shared public service on donated
    # infrastructure, and a formula test must not depend on it being up.
    assert_match "certificate-transparency", shell_output("#{bin}/crt-query --help")
  end
end
