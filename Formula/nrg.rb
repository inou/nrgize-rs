# This repository also serves as the inou/nrg Homebrew tap.
# Updated by the homebrew-pr job in .github/workflows/release.yml (scripts/bump-formula.py), which
# opens a PR after each stable release; review and merge it. To do it by hand, run
# scripts/bump-formula.py VERSION checksums.txt (musl archives are not used by this formula).
class Nrg < Formula
  desc "Rhai-scripted SSH deploys with optional health-gated rollouts"
  homepage "https://github.com/inou/nrgize-rs"
  version "0.1.5"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-aarch64-apple-darwin.tar.gz"
      sha256 "4da37f7620ffa37194b765959307bd5919c1d213d39d8abdc5dedd4dcdb5ddfe"
    end
    on_intel do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-x86_64-apple-darwin.tar.gz"
      sha256 "8aa95c130c20c12fb84878a1931eeeecdd69d0fea0c6d59270aa0b98d53aafe3"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "1d2848bf0ff81323ceb3e225d697a1e24e0a3fb73f3c65d1175e468169c93468"
    end
    on_intel do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "cc3d5af481c6f96bbf3009607ef69cca1a9ad15e0d57dec31b06f8675a1171d1"
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
