class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.3.7.tar.gz"
  sha256 "9b06342138a4b0490bef1c2ca7dae7b85d84ce6d62b4c4fbf407d186eabff130"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.3.7"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "64190f24115757e7dff668f6dfbb04560333fde6e73738a8b0225078bdd3b6cd"
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
