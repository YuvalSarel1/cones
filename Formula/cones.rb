class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.1.tar.gz"
  sha256 "074718bf4ad1cfb7d267c534a985fb6e6816993bad3b1886a6de3b2c83ec1b80"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.1"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "d9efa73efbc576703853e3df8fe47409551fcfe7f80f6241fcf27dac484c0be1"
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
