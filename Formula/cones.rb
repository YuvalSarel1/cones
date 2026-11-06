class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.11.tar.gz"
  sha256 "4031c5753b8f9a88406749362853bbc47477a351346dfb570100e447a3c9afa7"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.11"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "9b2d4b110bf0a386a416310900fcc1d5615789389262559d76ba84ba52e753c9"
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
