# This repository also serves as the inou/nrg Homebrew tap.
# After publishing a release, update version and all four hashes from its checksums.txt.
class Nrg < Formula
  desc "Energize — a Rhai-powered SSH/Docker deploy orchestration runner"
  homepage "https://github.com/inou/nrgize-rs"
  version "0.1.3"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-aarch64-apple-darwin.tar.gz"
      sha256 "266fbafb5e41558b74f2b9c743445ddb7fecf13d62217f0abd551caeceee02fd"
    end
    on_intel do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-x86_64-apple-darwin.tar.gz"
      sha256 "6d1ec1399cf92f34a2410dfb5a4558af4b3cc22fafe6efedc425c22184104c51"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "a80279bd00978fc562c390a6758fccdf45e358cefa1e40a40f26492995677cd8"
    end
    on_intel do
      url "https://github.com/inou/nrgize-rs/releases/download/v#{version}/nrg-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "204ce5d92ec60a24d7179600bed72eac456e8c9ecf6595c56da12523d757fb2a"
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
