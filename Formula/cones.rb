class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.9.tar.gz"
  sha256 "50cc287a8101f3b3de9e1d632f84a7ba6eed7416018deb82f47afe4ec5d17268"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.9"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "8e901a9efed54391eb162f0ce2557f071eccff9c400c62e7926c86641ffc3d5f"
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
