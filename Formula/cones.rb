class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.21.tar.gz"
  sha256 "12a9d01120c792dc84a82b47a62bb5be56cd762ca86bce9bf6986b32d099165f"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.21"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "333b97e13bf08ebc75c15f3fd81e7e4efa837f92ca7f40baced9ae527f9d7e6c"
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
