class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.28.tar.gz"
  sha256 "4bc0cbf8e215091c808d3fba5d490ee46969d94a4b59c42379458d7677b08465"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.28"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "106db42535ff6aaed59f437a88b94e41546a86a36c4ae6d623d1fb7a525833b4"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
