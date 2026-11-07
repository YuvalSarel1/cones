class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.12.tar.gz"
  sha256 "59168f84537d0b53299a5ffa34a7020d074f592c627828a7473ec429546e1d7b"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.12"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "efe60f02c8ea9b0cd2594592deccff38c06db2edf2166a37ce0157c462ab0cca"
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
