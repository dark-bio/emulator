# Release template filled with the version and downloaded artifact digests.
cask "ark-emulator" do
  arch arm: "arm64", intel: "amd64"

  version "@VERSION@"
  sha256 arm:   "@ARM64_SHA256@",
         intel: "@AMD64_SHA256@"

  url "https://github.com/dark-bio/emulator/releases/download/v#{version}/ark-emulator-#{version}-macos-#{arch}.dmg"
  name "Ark Emulator"
  desc "Emulated Ark enclave"
  homepage "https://dark.bio"

  depends_on macos: :sequoia

  app "Ark Emulator.app"
  # The launcher resolves its bundled QEMU and firmware from the path it was
  # started by, which a plain symlink would break.
  command_wrapper "ark-emulator",
                  executable: "#{appdir}/Ark Emulator.app/Contents/MacOS/launcher"
end
