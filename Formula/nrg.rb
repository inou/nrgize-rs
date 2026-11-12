# This repository also serves as the inou/nrg Homebrew tap.
# Updated by the homebrew-pr job in .github/workflows/release.yml (scripts/bump-formula.py), which
# opens a PR after each stable release; review and merge it. To do it by hand, run
# scripts/bump-formula.py VERSION checksums.txt (musl archives are not used by this formula).
class Nrg < Formula
  desc "Rhai-scripted SSH deploys with optional health-gated rollouts"
  homepage "https://github.com/inou/nrgize-rs"
  version "0.3.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-aarch64-apple-darwin.tar.gz"
      sha256 "030aa4bf1885d951385403d9300d5c65c60566b698f6a241bd29884674a25a7f"
    end
    on_intel do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-x86_64-apple-darwin.tar.gz"
      sha256 "2acff6ad935e178dc267f4e216de2ad92f7e96c80c96bfddaebefc59cf3c0fe6"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "3f8f7f25210e7db7a88f998d630bad11c39e7e16900cf404225110cc5194d13c"
    end
    on_intel do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "2322d7c6c893c68cd61779bdbe6c4b7d26bddc5ff9cec738304d44a666a63024"
    end
  end

  # Each release asset is a single-file archive (see .github/workflows/release.yml's Package
  # step) — the `nrg` binary sits at the archive root, no wrapping directory.
  def install
    bin.install "nrg"
  end

  test do
    assert_equal "nrg #{version}", shell_output("#{bin}/nrg --version").strip
    system "#{bin}/nrg", "--help"
  end
end
