# Localnet

One Rust LAN remote desktop app for Windows and macOS. On the computer you want to see and control, choose **Share this computer**. On the computer at your keyboard, choose **Connect**. The same app works in either direction: Mac to Windows, Windows to Mac, or between computers using the same OS. Sharing and remote sessions can run together in one window.

## Status

This is a functional prototype, not a production remote-desktop product. Windows screen capture, encrypted loopback viewing, rejection of wrong credentials, reconnects, and the desktop UI were tested in the development workspace. Live keyboard/mouse injection and a real two-machine LAN session have not been verified. macOS must be built and tested on a Mac. Do not assume Apple Silicon/Intel, Retina permissions, or input fidelity have been verified until tested on your hardware.

## Run on Windows

Requires Rust 1.92+ and Visual Studio C++ build tools/Windows SDK when compiling.

```powershell
cargo run --release --locked
```

The packaged executable is `dist/Localnet-Windows.exe`; after compiling, it is also available as `target/release/localnet.exe`. No installer or background service is required. Allow it through Windows Firewall on **Private** networks when sharing this computer. The app does not modify firewall rules.

## Run on Mac

Requires Rust 1.92+ and Xcode Command Line Tools. Build locally on the Mac (Apple Silicon or Intel). Windows cannot produce the Mac app with the local Windows toolchain.

```sh
sh scripts/bundle-macos.sh
open dist/Localnet.app
```

Install the resulting app in a stable location such as `/Applications/Localnet.app` before granting permissions. To **share the Mac**, enable Localnet under **Screen & System Audio Recording** (or Screen Recording on older macOS) and **Accessibility**, in **System Settings > Privacy & Security**. These two permissions are not needed just to view/control Windows from the Mac. On macOS 15+, allow **Local Network** access if prompted for either direction. Restart Localnet after granting permissions, then retry Share this computer.

Local builds are ad-hoc signed. Rebuilds can invalidate privacy approvals. For distribution, set `LOCALNET_SIGNING_IDENTITY` to your Developer ID Application identity before bundling, then notarize and staple using your Apple developer account. The script does not notarize, bypass Gatekeeper, or grant privacy permissions. Mac support is intended for macOS 12+, but the deployment target and OS compatibility still need a native Mac build/test.

## Connect

1. On the computer you want to control (Windows or Mac), choose its private LAN interface, then **Share this computer**.
2. Select **Copy private pairing code**. Transfer it to the computer at your keyboard through a trusted private channel, such as an encrypted message or USB drive. Anyone holding the code can view and control the host while sharing is enabled.
3. On the computer at your keyboard, enter the code under **Connect to a computer**, optionally give it a name, then **Connect**.
4. Click inside the remote image to take control. Typing, left/right/middle click, dragging, scrolling, navigation keys, F1-F12, and primary-modifier letter shortcuts are supported.
5. Use **Ctrl+Shift+F12**, click outside the image, or click **Release input** to release remote control. **Disconnect selected computer** closes only that remote session. **Stop sharing this computer** revokes only incoming access. **Stop all** or closing the app stops both. Focus loss, leaving the viewer window, and disconnect also release input.

On a Windows viewer, Ctrl acts as the host's primary shortcut modifier: Command on Mac, Ctrl on Windows. On a Mac viewer, Command maps to Ctrl when controlling Windows and to Command when controlling another Mac. Copy/cut/paste use the **remote clipboard**, not your local clipboard. Ctrl+C from Windows into a Mac terminal therefore means Command+C, not SIGINT; full native Control shortcut support needs a future key-mapping setting. OS-reserved shortcuts are not intercepted.

To control several computers, run the same app on each and enable sharing there. Connect to up to **four computers in one window**, with a separate private pairing code per computer. Use the named session tabs to choose which screen gets your input; switching releases input on the previous computer and drops stale events. Inactive sessions keep receiving video and consume bandwidth/CPU. Each shared computer currently accepts one viewer. Names/sessions exist only while the app is running; no saved computer list or persistent credentials are implemented.

You can share your own computer while connected to others. If someone connects to control this computer, outgoing remote input is paused (video continues), to avoid forwarding their injected keyboard/mouse events into another machine or forming control loops. Disconnect the incoming viewer or stop local sharing before resuming outgoing control.

For **Mac controlling Windows** specifically: click Share this computer in Localnet on Windows, then enter its code and click Connect in Localnet on Mac. There is no separate controller/host download, just a build of Localnet for each OS.

## Latency Design

- Independent TLS/TCP channels for input and video, both with TCP_NODELAY. Large video frames cannot hold the input connection's lock.
- Only one video request/frame in flight. Decoded frames use a latest-frame-only UI mailbox instead of an unbounded queue.
- Adjacent mouse movements coalesce without crossing clicks. Input order is preserved, with a bounded backlog and disconnect on overflow.
- Three-second absolute packet deadlines and 250 ms idle heartbeats. Partial packet timeouts terminate the connection, rather than replaying bytes.
- Pointer mapping uses logical host display bounds, separate from capture pixel resolution, with letterbox correction for Retina/scaled displays.

Start around 1600px width, quality 70, 20 target FPS; reduce width/quality if slow. Use Ethernet on both machines where possible. The UI reports **input acknowledgment round-trip** and **frame request + decode duration**, not physical input-to-photon latency. Target FPS is a cap, not a performance promise. JPEG capture/CPU encoding is deliberately a baseline and may not feel better than established remote-desktop tools yet.

The next performance phase should use ScreenCaptureKit + VideoToolbox on Mac and Desktop Duplication/Windows Graphics Capture + hardware encoding on Windows, hardware decode, and a tightly bounded streaming transport. This prototype does **not** provide H.264/HEVC/AV1, zero-copy rendering, smooth 60 FPS, audio, or measured near-native latency.

## Security and Limits

- Explicit Start sharing and a visible host status. Nothing starts at login or runs covertly.
- TLS 1.3 with normal certificate/name verification. The host certificate is enrolled from the private pairing code, not accepted from untrusted network discovery. No insecure TLS verifier or OS-wide trust installation.
- Fresh in-memory certificate and 256-bit access credential whenever Host starts. Stopping/restarting sharing invalidates the old code. Credentials are not stored in configuration files or logged, but copying the code uses the local clipboard, which can retain it.
- Bind only to the selected private or loopback interface; reject public IP routes/peers. No cloud relay, discovery, NAT traversal, shell commands, or file transfer. Private IP checks are not proof of the same physical LAN: VPN/private routed networks and router port forwarding still require care. Do not expose this prototype to the Internet.
- Resource-bounded connections, packets, input batches, JPEG dimensions, and decoding allocations. This is not hardened against a hostile LAN's denial-of-service traffic.
- Primary monitor only. No secondary monitors, login/lock screen control, Windows UAC secure-desktop, Ctrl+Alt+Delete, or control of elevated Windows apps from a normal process.
- Held keys/buttons are tracked and release is attempted on session end. OS permission changes can prevent injection or release; this cannot guarantee recovery when the OS refuses input.
- Printable typing uses committed Unicode text; letter shortcuts use US-layout physical key positions. Full physical keyboard fidelity, non-US shortcut mapping, native control-only/Alt shortcuts, game key holds, left/right modifiers, numpad distinctions, and IME behavior remain incomplete.
- Screenshot capture may omit protected content or the remote cursor. The local cursor gives immediate feedback but does not prove remote cursor position. xcap's Mac screenshot API is deprecated; a ScreenCaptureKit backend is needed for long-term support.
- Keep the controlled computer awake, unlocked, and signed in. This app does not change power settings.

## Verify

```sh
cargo fmt --check
cargo check --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Before real use, test Windows-to-Mac and Mac-to-Windows over your LAN: permissions denied/revoked, Retina/Windows DPI corner targeting, text and shortcuts, dragging out of the image, focus loss while holding a key/button, cable removal, host revocation, display reconfiguration, and repeated connect/disconnect. Measure input-to-photon latency on the actual hardware before claiming a latency improvement.
