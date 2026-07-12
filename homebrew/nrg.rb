# Homebrew Formula template for `nrg` (Energize) — roadmap 3.1.
#
# This file is the SOURCE OF TRUTH checked into the main nrgize-rs repo, not itself a live
# tap. A Homebrew tap is its own repository (conventionally `inou/homebrew-nrg`, so `brew tap
# inou/nrg` resolves it) containing a `Formula/nrg.rb`. After cutting a real release (pushing
# a `vX.Y.Z` tag — see .github/workflows/release.yml):
#
#   1. Copy this file to `Formula/nrg.rb` in the tap repo (or keep it in sync some other way).
#   2. Replace `version` with the tag just cut, and each `sha256` with the matching
#      `nrg-<target>.tar.gz.sha256` value from that release's assets (or its
#      `checksums.txt`).
#   3. Commit + push the tap repo.
#
# Until step 2 happens for a REAL release, `version` and every `sha256` below are placeholders,
# so `brew install` fails loud — today that's a plain "Failed to download resource" at
# `v0.0.0`'s (nonexistent) release URL, before checksums even come into play; once a real
# `version` is set but before real `sha256` values are, it instead fails Homebrew's own
# digest-length validation at formula-load time. Either way this is intentional (fail loud, not
# silently install something unverified), not a checksum-mismatch failure specifically.
#
#   brew tap inou/nrg
#   brew install nrg
class Nrg < Formula
  desc "Energize — a Rhai-powered SSH/Docker deploy orchestration runner"
  homepage "https://github.com/inou/nrgize-rs"
  license "MIT"
  version "0.0.0" # PLACEHOLDER — set to the real released version (see header comment above)

  on_macos do
    on_arm do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # PLACEHOLDER
    end
    on_intel do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # PLACEHOLDER
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # PLACEHOLDER
    end
    on_intel do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # PLACEHOLDER
    end
  end

  # Each release asset is a single-file archive (see .github/workflows/release.yml's Package
  # step) — the `nrg` binary sits at the archive root, no wrapping directory.
  def install
    bin.install "nrg"
  end

  test do
    system "#{bin}/nrg", "--help"
  end
end
