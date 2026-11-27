class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.39.tar.gz"
  sha256 "a4e7233161661bf7d764c459f5028e5155ec809630976f8f98a42b9903c93602"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.39"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "b7b3b836bb88be2072242a3a11a9f3523095dafa9691f28d5f60e23042b714c6"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
