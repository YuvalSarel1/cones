class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.4.tar.gz"
  sha256 "cdeb0d6959c40df3d5e00cc1f3fcccaf4fcea9ab071a1d02b2a162079f1895e8"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.4"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "b3e2a70c653c8ad76d2118510d33a50b20bb0998ef689806d9d99b82437b8b21"
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
