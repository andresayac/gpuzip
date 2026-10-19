# Homebrew formula: brew tap andresayac/gpuzip https://github.com/andresayac/gpuzip
#                   brew install gpuzip
class Gpuzip < Formula
  desc "Get back what a damaged .gz, .tar.gz, .zip or Office file still holds"
  homepage "https://github.com/andresayac/gpuzip"
  version "0.2.1"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    url "https://github.com/andresayac/gpuzip/releases/download/v0.2.1/gpuzip-0.2.1-macos-universal.tar.gz"
    sha256 "2e6a867419b398ec290f81851e4fd667d6117d200afe353a2c81ea6888597761"
  end

  on_linux do
    depends_on arch: :x86_64
    url "https://github.com/andresayac/gpuzip/releases/download/v0.2.1/gpuzip-0.2.1-linux-x64.tar.gz"
    sha256 "63406953de1da17ee2183d494742fcd1c81e54e5ca9608251e565f55e1ef7cf4"
  end

  def install
    libexec.install Dir["*"]
    # gpuzip finds its libraries next to its own file: a script that runs it from there, not a
    # link to it (macOS gives a program run through a link the link's path)
    (bin/"gpuzip").write_exec_script libexec/"gpuzip"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/gpuzip --version")
    # the rescue library found next to the program; doctor's exit says whether a GPU is usable
    assert_match "rescue: gzrescue loaded", pipe_output("#{bin}/gpuzip doctor 2>&1", nil, nil)
  end
end
