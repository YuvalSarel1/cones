class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.30.tar.gz"
  sha256 "caf0684608d1cec9759c0337d86295773a12abdb8fd049b4c8b2ce5951506048"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.30"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "7d40b90456ae94f6ae27220e982d62e5ff79abb032f4314d75a7c60e656ab2fe"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
