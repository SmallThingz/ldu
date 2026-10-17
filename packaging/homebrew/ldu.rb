class Ldu < Formula
  desc "Browse disk usage, preview files and queue deletions in your terminal"
  homepage "https://github.com/SmallThingz/ldu"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      depends_on arch: :arm64
      url "https://github.com/SmallThingz/ldu/releases/download/v0.1.0/ldu-macos-arm64", using: :nounzip
      sha256 "7e7d66b1b7913d8e4c55976dc777550c99551c08dbdf625e1bd81fd660c6b81d"
    end
    on_intel do
      depends_on arch: :x86_64
      url "https://github.com/SmallThingz/ldu/releases/download/v0.1.0/ldu-macos-x86_64", using: :nounzip
      sha256 "9ccb4502d4bef6368248db7e6185bba9cd3390f9568d94564ad29c1ef86b2601"
    end
  end

  on_linux do
    on_arm do
      depends_on arch: :arm64
      url "https://github.com/SmallThingz/ldu/releases/download/v0.1.0/ldu-linux-arm64", using: :nounzip
      sha256 "642220cafc90272ff4877829dc1abb9320620bcfc983823579ebd4491280f1d7"
    end
    on_intel do
      depends_on arch: :x86_64
      url "https://github.com/SmallThingz/ldu/releases/download/v0.1.0/ldu-linux-x86_64", using: :nounzip
      sha256 "48f965d47ca9cb8e9d1768e12ab6a506b764ce9fda8bc6b6c0f2d09585f010b4"
    end
  end

  # The executable contains license notices and, on macOS, a verified signature.
  skip_clean "bin/ldu"

  def install
    system_name = OS.mac? ? "macos" : "linux"
    architecture = Hardware::CPU.arm? ? "arm64" : "x86_64"
    bin.install "ldu-#{system_name}-#{architecture}" => "ldu"
    (bin/"ldu").chmod 0755
  end

  test do
    assert_equal "ldu #{version}", shell_output("#{bin}/ldu --version").strip
  end
end
