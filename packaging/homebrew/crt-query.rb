# Homebrew formula for crt-query. GENERATED — do not edit by hand.
#
# Written by the `tap` job in release.yml over in
# tiredithumans/crt-query, which regenerates this from the published
# SHA256SUMS on `release: published` and pushes it here. Reproduce it by hand
# with `just homebrew-formula` in that repo. The tap has to be a PUBLIC repo
# named homebrew-tap for `brew install tiredithumans/tap/crt-query` to
# resolve.
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
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.5.1/crt-query-v0.5.1-aarch64-apple-darwin.tar.gz"
      sha256 "37f3f6a5424fa97d7abc541ecfce1fbfd9f78ab81695799667bce2d376ba7f14"
    end
    on_intel do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.5.1/crt-query-v0.5.1-x86_64-apple-darwin.tar.gz"
      sha256 "3d93bb29a275b9f097130174d245c8233ec42ed1b39884b3874b65fd28d897ff"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.5.1/crt-query-v0.5.1-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "fff3ebe4ec53dae3d3b64640cd2b1bb5ef4c137b53c2bff5059797565dc34229"
    end
    on_arm do
      url "https://github.com/tiredithumans/crt-query/releases/download/v0.5.1/crt-query-v0.5.1-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "50b5aead06556ad7becf79c1f7c94a0fa67509ea0149c208c0ccd98d7a1f790a"
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
