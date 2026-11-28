# The Homebrew formula. CI fills in the version and checksum for every
# release, attaches the result to the release, and puts it into the tap
# (danijar2000/homebrew-tap) when the TAP_TOKEN secret is set:
#
#   brew install danijar2000/tap/monitor-switch
class MonitorSwitch < Formula
  desc "Share one monitor, keyboard and mouse between two Macs with one hotkey"
  homepage "https://github.com/danijar2000/monitor-switch"
  url "https://github.com/danijar2000/monitor-switch/releases/download/v@VERSION@/monitor-switch-macos.tar.gz"
  version "@VERSION@"
  sha256 "@SHA256@"
  license "MIT"

  depends_on :macos

  def install
    # The whole release: `monitor-switch setup` installs from it on both Macs.
    libexec.install Dir["*"]
    bin.install_symlink libexec/"bin/monitor-switch"
  end

  def caveats
    <<~EOS
      Nothing runs yet. Install on both Macs, then run on each:
        monitor-switch setup
      The Macs pair by a code you confirm on both.
      After `brew upgrade monitor-switch`, run `monitor-switch setup` on both.
      Before `brew uninstall`, run `monitor-switch setup uninstall`.
    EOS
  end

  test do
    assert_match "monitor-switch setup", shell_output("#{bin}/monitor-switch help")
  end
end
