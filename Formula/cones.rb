class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.25.tar.gz"
  sha256 "0790061b91eb8178a334317d435f5e7d6247eea9fd388d36107aba93101ae2c6"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.25"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "efc318fc4f6cd7e5a8c63bb2fe49f83449f7743f0469b6396b45a0a54644132f"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
