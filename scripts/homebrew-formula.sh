#!/usr/bin/env bash
# Print Formula/rrover.rb for a release from its .sha256 files.
#   scripts/homebrew-formula.sh TAG SHA256_DIR
set -euo pipefail

tag="$1"
dir="$2"
base="https://github.com/danik-tro/redis-rover/releases/download/$tag"

sum() {
    local value
    value=$(cut -d ' ' -f 1 "$dir/rrover-$tag-$1.sha256" | tr -d '\r\n')
    [[ "$value" =~ ^[0-9a-fA-F]{64}$ ]] || { echo "bad checksum for $1" >&2; exit 1; }
    echo "$value"
}

mac_arm=$(sum macos-arm64)
mac_x86=$(sum macos-x86_64)
linux_arm=$(sum linux-arm64-musl)
linux_x86=$(sum linux-x86_64-musl)

cat <<RUBY
class Rrover < Formula
  desc "Terminal UI client for Redis"
  homepage "https://github.com/danik-tro/redis-rover"
  license "MIT"

  on_macos do
    on_arm do
      url "$base/rrover-$tag-macos-arm64.tar.gz"
      sha256 "$mac_arm"
    end
    on_intel do
      url "$base/rrover-$tag-macos-x86_64.tar.gz"
      sha256 "$mac_x86"
    end
  end

  on_linux do
    on_arm do
      url "$base/rrover-$tag-linux-arm64-musl.tar.gz"
      sha256 "$linux_arm"
    end
    on_intel do
      url "$base/rrover-$tag-linux-x86_64-musl.tar.gz"
      sha256 "$linux_x86"
    end
  end

  def install
    bin.install "rrover"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/rrover --version")
  end
end
RUBY
