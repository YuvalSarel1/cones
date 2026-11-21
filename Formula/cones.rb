class Cones < Formula
  desc "A terminal workspace for coding agents"
  homepage "https://github.com/YuvalSarel1/cones"
  url "https://github.com/YuvalSarel1/cones/archive/refs/tags/v0.4.24.tar.gz"
  sha256 "6ae1c771d8b4da275ef37fe300c2f68476db62b96cb8ee60583bd2abe9298c7b"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/YuvalSarel1/cones.git", branch: "main"

  bottle do
    root_url "https://github.com/YuvalSarel1/cones/releases/download/v0.4.24"
    sha256 cellar: :any_skip_relocation, arm64_big_sur: "1a0fbb64ca807b703d1e38dcf4315ecfe70d699a478d7de12e9fc1316c022471"
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "cones #{version}", shell_output("#{bin}/cones --version")
  end
end
