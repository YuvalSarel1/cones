class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.14.tar.gz"
  sha256 "055951331ecffa31264faf2f4df18c408c5d35024837c03c1250a98977ee706d"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.14"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "6511755e48cb56b4d193566c0d83f89a6c038090f2d4c746087f682d500fdeca"
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
