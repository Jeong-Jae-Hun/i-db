# Proposed cask for Jeong-Jae-Hun/homebrew-tap (Casks/i-db.rb). Copy it there at the cutover (docs/cutover.md):
# set version to the release and sha256 to the i-db_darwin_arm64.zip line of that release's checksums.txt.
#
# The app is ad-hoc signed (codesign -s -) and not notarized, so Gatekeeper refuses a quarantined copy with
# "i-db is damaged". mindow.rb has no quarantine step; this cask drops the flag in postflight instead.
cask "i-db" do
  version "1.0.0"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"

  url "https://github.com/Jeong-Jae-Hun/i-db/releases/download/v#{version}/i-db_darwin_arm64.zip"
  name "i-db"
  desc "Database GUI for DataGrip projects (PostgreSQL, Redis, read-only AWS sources)"
  homepage "https://github.com/Jeong-Jae-Hun/i-db"

  depends_on arch: :arm64
  depends_on macos: ">= :big_sur"

  app "i-db.app"

  postflight do
    system_command "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "#{appdir}/i-db.app"]
  end

  # ~/.config/db holds remote.json shared with the db CLI, so only the GUI's own files go
  zap trash: [
    "~/.config/db/gui.json",
    "~/Library/Preferences/io.github.jeong-jae-hun.i-db.plist",
    "~/Library/Saved Application State/io.github.jeong-jae-hun.i-db.savedState",
  ]
end
