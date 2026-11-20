class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.22.tar.gz"
  sha256 "3e56b153f8f3a3a3a737dc3e7f7148f1247a904c4e10815e38870c624991e046"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.22"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "1cdd939bcf5c6da4771a2dcd757c4e5ddf340578270d071da1dc410662f3b2d8"
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
