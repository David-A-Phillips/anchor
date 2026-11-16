# Homebrew formula for a tap (David-A-Phillips/homebrew-tap, file Formula/anchor.rb). Installs the prebuilt
# release archives; see anchor-source.rb for a build-from-source variant.
#
# The version and the four checksums are rewritten from a release's `.sha256` files by
# packaging/homebrew/update-formula.sh (the release workflow runs it). The all-zero checksums
# below are placeholders until that has run for a real release.
#
#   brew tap David-A-Phillips/tap
#   brew install anchor
class Anchor < Formula
  desc "Local-first context engine that grounds AI models in your codebases"
  homepage "https://github.com/David-A-Phillips/anchor"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/David-A-Phillips/anchor/releases/download/v0.1.0/anchor-0.1.0-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/David-A-Phillips/anchor/releases/download/v0.1.0/anchor-0.1.0-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/David-A-Phillips/anchor/releases/download/v0.1.0/anchor-0.1.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/David-A-Phillips/anchor/releases/download/v0.1.0/anchor-0.1.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "anchor"
    generate_completions_from_executable(bin/"anchor", "completions")
    (man1/"anchor.1").write Utils.safe_popen_read(bin/"anchor", "man")
  end

  service do
    run [opt_bin/"anchor", "daemon"]
    keep_alive crashed: true
    working_dir var/"anchor"
    log_path var/"log/anchor.log"
    error_log_path var/"log/anchor.log"
  end

  def caveats
    <<~EOS
      Next steps:
        anchor setup          # guided configuration
        anchor mcp install    # connect an AI agent

      Run the daemon in the background with:
        brew services start anchor
      (it needs a configuration first: `anchor init` or `anchor setup`).
    EOS
  end

  test do
    ENV["ANCHOR_HOME"] = testpath/"home"
    assert_match version.to_s, shell_output("#{bin}/anchor --version")
    system bin/"anchor", "demo", "--out", testpath/"demo", "--scale", "tiny"
    assert_path_exists testpath/"demo"
    refute_empty Dir[testpath/"demo/*"]
    system bin/"anchor", "init"
    assert_path_exists testpath/"home/anchor.toml"
  end
end
