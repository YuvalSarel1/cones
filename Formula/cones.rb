class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.19.tar.gz"
  sha256 "6208aa43ca4cff53a675391b1b0495cb89586c7ba0f12620da64fa1ab36cb6ac"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.19"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "7352d59b98b6398fee624afdbbc9aaae531ad5106b4a7a4f04ad2a4e99e0469a"
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
