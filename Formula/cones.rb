class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.3.8.tar.gz"
  sha256 "2ad39f6b4febf5b3bc98cd99b4bfb027af18a8a7ce347b891b5f47fe44355813"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.3.8"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "b7e12d3db17739afc76ed84382f29f11ff3023e3b621abd101a2b829d8659ce1"
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
