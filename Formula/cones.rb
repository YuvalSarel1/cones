class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.2.tar.gz"
  sha256 "b4fa637efff14be582dfc4fb34e7984d0a5554410fa36a8532b5a7f6ef368c23"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.2"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "6906f74f32e1a39f299958171a3d612b038609f325cb96d69eafd759cb57f455"
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
