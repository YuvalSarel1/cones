class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.37.tar.gz"
  sha256 "e7e00553a73290efaacea4c91ec1cfe5744d1fce40f0959c68fb08ce764a6a46"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.37"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "37434c80812639b73b28c267985a55de52d124435a333ff478cc4b2a9793f3ff"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
