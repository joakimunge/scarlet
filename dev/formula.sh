#!/bin/sh
# Writes the Homebrew formula for a release: dev/formula.sh VERSION DIR
# DIR holds the release's scarlet-<target>.tar.gz.sha256 files.
set -eu
version=$1
dir=$2
base="https://github.com/joakimunge/scarlet/releases/download/v$version"
sum() { cut -d' ' -f1 "$dir/scarlet-$1.tar.gz.sha256"; }

cat <<FORMULA
class Scarlet < Formula
  desc "Fast, keyboard-driven terminal client for PostgreSQL"
  homepage "https://github.com/joakimunge/scarlet"
  version "$version"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "$base/scarlet-aarch64-apple-darwin.tar.gz"
      sha256 "$(sum aarch64-apple-darwin)"
    end
    on_intel do
      url "$base/scarlet-x86_64-apple-darwin.tar.gz"
      sha256 "$(sum x86_64-apple-darwin)"
    end
  end

  on_linux do
    on_arm do
      url "$base/scarlet-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "$(sum aarch64-unknown-linux-gnu)"
    end
    on_intel do
      url "$base/scarlet-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "$(sum x86_64-unknown-linux-gnu)"
    end
  end

  def install
    bin.install "scarlet"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/scarlet --version")
  end
end
FORMULA
