class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.8.tar.gz"
  sha256 "b8cbd6d36896e8796989dc0cf43f7e2bbe4111e6cad6dbdb12e8766f866bc771"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.8"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "fc44296b37a804a3489486a78febffbb8a520040c1bf8219b372fa67bb5db03e"
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
