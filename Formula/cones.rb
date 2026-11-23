class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.31.tar.gz"
  sha256 "419fe37433b067ab8fce589bc6f91670cf8e1f92510939c0f503e58373f0fb3b"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.31"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "dc9135c0a1a9253736ae51d9ed43317cad6e88deab3bdb26d531d7427b8f2e82"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
