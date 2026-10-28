class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.7.tar.gz"
  sha256 "75453b2138b36acae6b203e940d7d32eab4beab2b3df3fdc2dd2d2366cd3abf5"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.7"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "8fa0a359eac4aed92a35787ff881044aedf1d84ed8eccba81a2379796a0785ec"
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
