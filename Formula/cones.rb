class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.38.tar.gz"
  sha256 "9cc8867d5b2e63a40d3308d39187096607ba3722debc7481e4401c8265863c72"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.38"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "41407ddd223085bb5c263f915a69995d6a780b4c3db427c06d0030105a7e3b2c"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
