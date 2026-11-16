# Build-from-source variant of the formula, the shape homebrew-core requires (no prebuilt
# binaries). Needs a source tarball with a stable checksum: GitHub's tag archive works.
# Replace OWNER, and the checksum with `curl -L <url> | shasum -a 256`. Install it from a tap
# under a different name, or instead of anchor.rb; both define class Anchor.
class Anchor < Formula
  desc "Local-first context engine that grounds AI models in your codebases"
  homepage "https://github.com/OWNER/anchor"
  url "https://github.com/OWNER/anchor/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  # TODO(owner): the real SPDX license identifier (homebrew-core requires an OSI-approved one).
  license "TODO(owner)"
  head "https://github.com/OWNER/anchor.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
    generate_completions_from_executable(bin/"anchor", "completions", shells: [:bash, :zsh, :fish])
    (man1/"anchor.1").write Utils.safe_popen_read(bin/"anchor", "man")
  end

  service do
    run [opt_bin/"anchor", "daemon"]
    keep_alive crashed: true
    working_dir var/"anchor"
    log_path var/"log/anchor.log"
    error_log_path var/"log/anchor.log"
  end

  test do
    ENV["ANCHOR_HOME"] = testpath/"home"
    assert_match version.to_s, shell_output("#{bin}/anchor --version")
    system bin/"anchor", "demo", "--out", testpath/"demo", "--scale", "tiny"
    assert_path_exists testpath/"demo"
  end
end
