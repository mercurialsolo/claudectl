class Claudectl < Formula
  desc "Mission control for Claude Code: supervise, orchestrate, and connect coding agents"
  homepage "https://github.com/mercurialsolo/claudectl"
  url "https://github.com/mercurialsolo/claudectl/archive/refs/tags/v0.49.2.tar.gz"
  sha256 "REPLACE_WITH_SHA256_OF_SOURCE_TARBALL"
  license "MIT"
  head "https://github.com/mercurialsolo/claudectl.git", branch: "main"

  livecheck do
    url :stable
    strategy :github_latest
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: ".")

    generate_completions_from_executable(bin/"claudectl", "completions")

    (man1/"claudectl.1").write Utils.safe_popen_read(bin/"claudectl", "man")
  end

  test do
    # Completions render for the major shells we support
    assert_match "_claudectl", shell_output("#{bin}/claudectl completions bash")
    assert_match "#compdef claudectl", shell_output("#{bin}/claudectl completions zsh")
    assert_match "complete -c claudectl", shell_output("#{bin}/claudectl completions fish")

    # Man page renders to roff
    assert_match ".TH claudectl 1", shell_output("#{bin}/claudectl man")

    # Version surface
    assert_match version.to_s, shell_output("#{bin}/claudectl --version")

    # `--list` against an empty HOME should succeed and produce no sessions.
    # Use a sandboxed HOME so we don't read the user's real ~/.claude.
    ENV["HOME"] = testpath
    system bin/"claudectl", "--list"
  end
end
