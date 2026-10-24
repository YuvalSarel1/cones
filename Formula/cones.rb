class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.5.tar.gz"
  sha256 "8a535f0bcc7a0b4e2e3e28483ea860db028c655d4c49c28494aecf74a9af4d82"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.5"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "a19d2730d4dfc829dff8ab60d3c1247af897927084ec7b338222caeca8e72603"
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
