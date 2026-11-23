class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.29.tar.gz"
  sha256 "351c85c30ae8438b77012a844328802e1c3f79a76171d8831a9e3f46b5023a92"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.29"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "96455808a2002482df47b5bd8f02e37ac75ebe6a94e3cce71cc9a67bf9ab8113"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
