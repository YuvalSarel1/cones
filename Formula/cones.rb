class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.36.tar.gz"
  sha256 "968341a2e6889ed8eb36924a1e3e823ae69f165b682a82751a03583fb410b1f0"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.36"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "7338ff2dd09373ca5a56cc5e21e344c0bf91566e1b6efd0343924bfc1ae916e2"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
