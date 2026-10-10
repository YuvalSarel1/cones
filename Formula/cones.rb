class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.3.9.tar.gz"
  sha256 "a2961100809d9daac881ec30b9397fa79b9d0938ccf7f0c17be567885ddbeb91"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.3.9"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "ac437c15aed543b4beb338741034e0c42b560ed3af3be3171bc16508daf0d3ec"
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
