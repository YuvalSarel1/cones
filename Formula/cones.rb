class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.32.tar.gz"
  sha256 "383be596aeb03fef62512d8aef2417e5141fc9b77e24236b9bf3c7f362bed09b"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.32"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "c337d1822e1e3e7d34ce6ce92dc0252b60fe7e9bf3a91fe1ba22c5f51de3116f"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
