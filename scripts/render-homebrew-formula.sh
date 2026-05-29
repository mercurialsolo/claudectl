#!/bin/sh

set -eu

VERSION="${1:?version is required}"
MACOS_ARM_SHA="${2:?macOS arm64 sha is required}"
MACOS_INTEL_SHA="${3:?macOS x86_64 sha is required}"
LINUX_INTEL_SHA="${4:?Linux x86_64 sha is required}"
LINUX_ARM_SHA="${5:?Linux arm64 sha is required}"

TAG="v${VERSION}"
BASE_URL="https://github.com/mercurialsolo/claudectl/releases/download/${TAG}"

cat <<EOF
class Claudectl < Formula
  desc "Orchestrate a swarm of Claude Code agents with a learning local-LLM brain"
  homepage "https://github.com/mercurialsolo/claudectl"
  version "${VERSION}"
  license "MIT"

  on_macos do
    on_arm do
      url "${BASE_URL}/claudectl-${TAG}-aarch64-apple-darwin.tar.gz"
      sha256 "${MACOS_ARM_SHA}"
    end

    on_intel do
      url "${BASE_URL}/claudectl-${TAG}-x86_64-apple-darwin.tar.gz"
      sha256 "${MACOS_INTEL_SHA}"
    end
  end

  on_linux do
    on_arm do
      url "${BASE_URL}/claudectl-${TAG}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "${LINUX_ARM_SHA}"
    end

    on_intel do
      url "${BASE_URL}/claudectl-${TAG}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "${LINUX_INTEL_SHA}"
    end
  end

  def install
    bin.install "claudectl"
  end

  test do
    assert_match "claudectl", shell_output("#{bin}/claudectl --version 2>&1", 0)
  end
end
EOF
