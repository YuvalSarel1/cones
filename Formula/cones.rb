class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.23.tar.gz"
  sha256 "d798802848a9b3380dcb3213c1a71fbcf96a5e6c23f777647db45ba6a6114e31"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.23"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "85d86fc0bb0876b83ea7b2b4734aec24df44a0c5830dc8f2e3609e72b72adefe"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
