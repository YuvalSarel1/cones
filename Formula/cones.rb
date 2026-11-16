class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.17.tar.gz"
  sha256 "759709fe6b947a938352d188c7209f7a31bf44c9642db589691c2c1075eb6d55"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.17"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "6dd4ccd3b66ab6ae1c0ae50597d9ece1072522a72e3eba2d83414e4c8964e411"
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
