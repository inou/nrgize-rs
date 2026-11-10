# This repository also serves as the inou/nrg Homebrew tap.
# Updated by the homebrew-pr job in .github/workflows/release.yml (scripts/bump-formula.py), which
# opens a PR after each stable release; review and merge it. To do it by hand, run
# scripts/bump-formula.py VERSION checksums.txt (musl archives are not used by this formula).
class Nrg < Formula
  desc "Rhai-scripted SSH deploys with optional health-gated rollouts"
  homepage "https://github.com/inou/nrgize-rs"
  version "0.2.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-aarch64-apple-darwin.tar.gz"
      sha256 "c52b77bead1052c4bfd8c38cc7c80b8b90cb8ff3c04f9203c3029f3e91a7ec71"
    end
    on_intel do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-x86_64-apple-darwin.tar.gz"
      sha256 "4c160fae4e69f087747b5c5ca52c64e36038d05cedd41a068a747773f49a52ee"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "9839007da88ff376e510493c792945793b2c85d7b5e29ec3b7567ecc01f7b8cb"
    end
    on_intel do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "7ad42d48ff8cc2269658dbd6e3e0480cd0c8a5a0821e856176c44db934eac4e3"
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
