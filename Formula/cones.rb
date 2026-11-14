class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.13.tar.gz"
  sha256 "abea3da7d93eb1e0792c9c1750da759c69215d47ed0e46f9066e5874a5be9039"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.13"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "867411cebabf50916438eeb6640b3ac01be5ef5fefd32eb12894b2fb073781af"
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
