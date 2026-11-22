class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.27.tar.gz"
  sha256 "42d43bad47fd305c3598cbb2b26f458d09f15ba82c57d3af9cef5141b0196f90"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.27"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "3c88a849f57b536b622d94f33021cac29cf9cc586127ef0afe7e383dc43ae71a"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
