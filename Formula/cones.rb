class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.3.6.tar.gz"
  sha256 "2c3fe7908804f381814a19933742e65fe43b05befca82c8d6d264606e671720b"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.3.6"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "af53b8e1d659ddfb1939f9cee32be322a257e378d4fc4e5fef655df33a24fd27"
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
