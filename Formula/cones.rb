class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.15.tar.gz"
  sha256 "35c8b80ae56033aaea72f2be4ff6aafec3760836705fc081f395d403fddb3124"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.15"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "e8e3b92115b7a04a753d68dbcf1a12f5352b8ed92e79ac17f3760ff934a4b861"
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
