# UNTESTED Homebrew formula template. Never run through `brew audit` or `brew install`.
#
# Fill in: OWNER (GitHub owner), VERSION, and the four SHA256 values (they are the contents of
# the `.sha256` files that the release workflow uploads next to each archive). Put the result in
# a tap repository, e.g. OWNER/homebrew-tap at Formula/anchor.rb, then:
#   brew install OWNER/tap/anchor
class Anchor < Formula
  desc "Local-first context engine and sync daemon that grounds AI models in your codebases"
  homepage "https://github.com/OWNER/anchor"
  version "VERSION" # without the leading "v"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/OWNER/anchor/releases/download/vVERSION/anchor-VERSION-aarch64-apple-darwin.tar.gz"
      sha256 "SHA256_AARCH64_APPLE_DARWIN"
    else
      url "https://github.com/OWNER/anchor/releases/download/vVERSION/anchor-VERSION-x86_64-apple-darwin.tar.gz"
      sha256 "SHA256_X86_64_APPLE_DARWIN"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/OWNER/anchor/releases/download/vVERSION/anchor-VERSION-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "SHA256_AARCH64_UNKNOWN_LINUX_GNU"
    else
      url "https://github.com/OWNER/anchor/releases/download/vVERSION/anchor-VERSION-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "SHA256_X86_64_UNKNOWN_LINUX_GNU"
    end
  end

  def install
    # Archives contain a single top-level directory; Homebrew strips it automatically.
    bin.install "anchor"
  end

  def caveats
    <<~EOS
      Next steps:
        anchor setup
        anchor mcp install
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/anchor --version")
  end
end
