class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.20.tar.gz"
  sha256 "bd8ea2694bd056e5e1984ffe2ef3de64895f1eafdea2d35309ae3595a1734d44"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.20"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "ffd71424af062473b62f018dfe91a41ea3796b832759d6566d5be4eb5ec02913"
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
