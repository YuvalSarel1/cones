class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.10.tar.gz"
  sha256 "fcb9db9563ab414edfa744f3ab1d719a4d64aa0d334e7d6983af7c2ad8af68fc"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.10"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "606946b7e09651d97507b4350b8b4bb8d806004cc2dc1aa104bca72913683efb"
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
