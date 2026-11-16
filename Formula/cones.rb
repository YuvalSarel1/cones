class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.18.tar.gz"
  sha256 "90051c5216427c744656466aa22218a3c23117ab0203e37712da45465f540e32"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.18"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "4fed0314c83a75a412db3dfe4c7772ba3911ff298c694afeb62b74e7faf386da"
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
