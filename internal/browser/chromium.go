package browser

import (
	"os/exec"

	"github.com/go-rod/rod/lib/launcher"
)

// systemChromium returns the path to the nix-packaged (or OS-installed)
// Chromium binary if one is on PATH, otherwise returns "".
//
// The nix Chromium has all its shared-library dependencies baked into its
// RPATH so it works without a custom LD_LIBRARY_PATH, unlike the Chromium
// binary that Rod auto-downloads.
func systemChromium() string {
	for _, name := range []string{"chromium-browser", "chromium", "google-chrome-stable", "google-chrome"} {
		if p, err := exec.LookPath(name); err == nil {
			return p
		}
	}
	return ""
}

// applySystemChromium sets the Bin on l to the nix/system Chromium when
// available.  The launcher is returned unchanged if no system binary is found
// (Rod will auto-download its own in that case).
func applySystemChromium(l *launcher.Launcher) *launcher.Launcher {
	if p := systemChromium(); p != "" {
		return l.Bin(p)
	}
	return l
}
