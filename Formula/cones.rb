class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.26.tar.gz"
  sha256 "4a95c02c4ad53f4528438ca6c4288e1ffe9b4b956ed2c5ffd5cccb54bba60cc0"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.26"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "804ec6284f5592fb1396d40073b42da5235c5f7d101eac45442d7ce267e6cc34"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
