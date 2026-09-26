# This repository also serves as the inou/nrg Homebrew tap.
# After publishing a release, update version and all four hashes from its checksums.txt.
class Nrg < Formula
  desc "Energize — a Rhai-powered SSH/Docker deploy orchestration runner"
  homepage "https://github.com/inou/nrgize-rs"
  version "0.1.4"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-aarch64-apple-darwin.tar.gz"
      sha256 "208f4958e5446678b2398acceccef1834fbc54851e63793e0aba77a77bd43393"
    end
    on_intel do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-x86_64-apple-darwin.tar.gz"
      sha256 "f4ad73a68ddc75d3d0124c65d438bc6357e69766accfeba204351bf753a2eb12"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "21f97e35be1afa9a16f1a2f63d82ae2e52feb974e74b3bcac68c92af330cab49"
    end
    on_intel do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "244762545b2d6434945d76d021f5985e6603e73012ebb5c254da438a2d5a0b6d"
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
