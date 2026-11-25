class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.34.tar.gz"
  sha256 "75153e718df4722701f3762a553dd749fc8f7a20a8c3f8c4e42a7ba5d8891524"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.34"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "4807c72a59112ed1a83090d995ce9f0b6c8b79f2db348767cf4b8f4ad0635e53"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
