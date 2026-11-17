# Homebrew formula for Pasture, installing the prebuilt binaries from
# the GitHub Release. Intended for a personal tap
# (shizukutanaka/homebrew-tap); homebrew-core would require a
# build-from-source formula instead — see packaging/README.md.
#
# The sha256 values are the SHA-256 of the actually downloaded release
# assets, verified 2026-11-15. Before publishing the tap, re-download
# the assets and re-check the hashes.
#
# Only the platforms the release workflow actually builds are covered:
# macOS Apple Silicon and Linux x86_64. There is no Intel macOS or
# Linux ARM build.
class Pasture < Formula
  desc "OpenAI-compatible local/cloud LLM routing proxy"
  homepage "https://github.com/shizukutanaka/Pasture"
  version "0.28.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/shizukutanaka/Pasture/releases/download/v0.28.0/pasture-v0.28.0-macos-aarch64.tar.gz"
      sha256 "f22641e744c3f6f55a9a6c149b894d097944b4734566f5a254e3934d69233c63"

      def install
        bin.install "pasture-v#{version}-macos-aarch64/pasture"
      end
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/shizukutanaka/Pasture/releases/download/v0.28.0/pasture-v0.28.0-linux-x86_64.tar.gz"
      sha256 "edb51219f0b12852a6c2b6c61bb6bdb3910890dc337e7220c3284bf07b4a69a8"

      def install
        bin.install "pasture-v#{version}-linux-x86_64/pasture"
      end
    end
  end

  test do
    assert_match "pasture #{version}", shell_output("#{bin}/pasture version")
  end
end
