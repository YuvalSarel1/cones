class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.16.tar.gz"
  sha256 "2fc4841adef32788c7640155980b918936c0322fd1c5ab5206c356c3d676f38f"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.16"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "d652085034ad60135d2444deab6f148c24655ca047e4b38d5271ea5319089322"
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
