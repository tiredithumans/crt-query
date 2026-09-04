# Homebrew formula for crt-query. GENERATED — do not edit by hand.
#
# Regenerate after a release with `just homebrew-formula`, then copy this
# file into the tap repository as Formula/crt-query.rb. The tap has to be a
# PUBLIC repo named homebrew-tap for
# `brew install tiredithumans/tap/crt-query` to resolve.
#
# A binary formula, not a source build: it installs the very archives the
# release publishes, checked against the same SHA256SUMS a manual install
# would verify.
class CrtQuery < Formula
  desc "Query crt.sh certificate-transparency data from its public PostgreSQL database"
  homepage "https://github.com/tiredithumans/crt-query"
  # No `version` stanza: Homebrew scans it out of the URLs below, and
  # declaring it as well is a `brew audit --strict` failure.
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.4.0/crt-query-v0.4.0-aarch64-apple-darwin.tar.gz"
      sha256 "51d7f7c66fc71000b179acb8659a5cac37d0a343a23175490647237e318558f7"
    end
    on_intel do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.4.0/crt-query-v0.4.0-x86_64-apple-darwin.tar.gz"
      sha256 "adbf3daac8399502df66d7f63e13a410d7da6ca11da5279da70e8611d004ed04"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.4.0/crt-query-v0.4.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "07fef60dbec6c0e4ad38af6afe24388d2c4c944ff4189fab2ed3a1ebe68c930b"
    end
    on_arm do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.4.0/crt-query-v0.4.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "6e5c41bdf0a60651ac35bff1ea9bafc4af5f5fd385594ccbf32f628ceb5eaeac"
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
