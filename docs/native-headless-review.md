# Native headless review — 2026-09-25

Supervisor independently ran current native smoke on DISPLAY=:1: 12 unit and2 smoke tests pass. The proposed CI default xvfb-run is not yet valid.

## Reproduction
- Default Xvfb + default Vulkan driver fails window surface creation with No DRI3 support / PlatformNotSupported.
- VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json + Xvfb1280x900x24 creates window and input/probe logs, but existing xwd -id capture returns a black PNG (<1KB), failing the screenshot gate.
- Independently launched app with same lavapipe environment, waited5s, windowmap/windowfocus, waited3s, captured both xwd -id and xwd -root. Root screenshot visibly contains correct actual app UI. Files: /tmp/snip-native-headless-probe/root.png, window.png, window.txt, app.log. Root screenshot inspected visually.
- This evidence distinguishes rendering from screenshot capture. A later root screenshot is correct; do not assume window surface pixels are readable by xwd -id on this server. Also do not claim timing ruled out until testing capture methods side-by-side.
- Native test could capture root and crop using actual translated window client geometry (scaling/coordinates checked), or another verified screen-read method. No fake content/screenshot, no removing image-content checks, no unconditional delay-only readiness.
- Configure explicit software Vulkan ICD for Linux headless tests using discovered package path (Ubuntu24.04 may use lvp_icd.x86_64.json; host uses lvp_icd.json). Fail if unavailable. No production renderer override.
- Xvfb has no window manager: windowactivate emits unsupported errors. Use appropriate explicit X11 focus for private headless server, verify click/key behavior; do not suppress failed input.
- Only an actual full headless test pass, screenshots checked and owned process cleanup verified closes this finding.
