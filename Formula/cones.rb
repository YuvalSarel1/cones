class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.33.tar.gz"
  sha256 "57a9f1260bd663007c1e0df5bbd44ec7ef717301611023694999eb9d79fe9231"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.33"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "e47ecbcaa8f2a2e3fe8ed5e6d23fdff05c1db46a763ccd0710c43bb2efb60ccb"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
