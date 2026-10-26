class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.6.tar.gz"
  sha256 "01e555e761c9ae018641eb03f467b624362571ecfdc6c5fe1da6ced232fa5bf8"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.6"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "f17368ad9b51eaa14d5d96c40de67281de816389eff380a85788516668e7614c"
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
