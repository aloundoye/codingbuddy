class Codingbuddy < Formula
  desc "AI-powered coding assistant CLI"
  homepage "https://github.com/alassanendoye/codingbuddy"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/alassanendoye/codingbuddy/releases/download/v0.3.0/codingbuddy-v0.3.0-aarch64-apple-darwin.tar.gz"
      sha256 "PLACEHOLDER"
    end
    on_intel do
      url "https://github.com/alassanendoye/codingbuddy/releases/download/v0.3.0/codingbuddy-v0.3.0-x86_64-apple-darwin.tar.gz"
      sha256 "PLACEHOLDER"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/alassanendoye/codingbuddy/releases/download/v0.3.0/codingbuddy-v0.3.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "PLACEHOLDER"
    end
    on_intel do
      url "https://github.com/alassanendoye/codingbuddy/releases/download/v0.3.0/codingbuddy-v0.3.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "PLACEHOLDER"
    end
  end

  def install
    bin.install "codingbuddy"
  end

  test do
    assert_match "codingbuddy", shell_output("#{bin}/codingbuddy --version")
  end
end
