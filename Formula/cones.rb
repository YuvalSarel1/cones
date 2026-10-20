class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.3.tar.gz"
  sha256 "34f34dbee2d11a71c293d5a0762d2bcc984b360bfe63dcf3e79f9e14837f86d0"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.3"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "4489b245c306a10328a1dfa7a448671138a7cf1a1033d9b8abbaedc2d976d2d3"
  end

  depends_on "rust" => :build
  depends_on :macos

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
