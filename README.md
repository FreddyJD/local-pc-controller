# Localnet

One Rust LAN remote desktop app for Windows and macOS. Open it on both computers; click a **Nearby computer**, enter its six-digit on-screen code, then approve **Allow viewing and control** there. Sharing starts automatically after approval. The same app works in either direction: Mac to Windows, Windows to Mac, or between computers using the same OS.

## Status

This is a functional prototype, not a production remote-desktop product. Windows screen capture, encrypted loopback viewing, rejection of wrong credentials, reconnects, and the desktop UI were tested in the development workspace. Live keyboard/mouse injection and a real two-machine LAN session have not been verified. macOS must be built and tested on a Mac. Do not assume Apple Silicon/Intel, Retina permissions, or input fidelity have been verified until tested on your hardware.

## Run on Windows

Requires Rust 1.92+, Visual Studio C++ build tools/Windows SDK, CMake, and NASM when compiling. CMake/NASM build a statically linked native SIMD JPEG encoder; they are not runtime dependencies.

```powershell
powershell -ExecutionPolicy Bypass -File scripts/build-windows.ps1
```

The packaged executable is `dist/Localnet-Windows.exe`; after compiling, it is also available as `target/release/localnet.exe`. No installer or background service is required. Allow it through Windows Firewall on **Private** networks when sharing this computer. The app does not modify firewall rules.

## Run on Mac

Requires Rust 1.92+, Xcode Command Line Tools, and CMake (plus NASM on Intel Macs). Build locally on the Mac (Apple Silicon or Intel). Windows cannot produce the Mac app with the local Windows toolchain.

```sh
sh scripts/bundle-macos.sh
open dist/Localnet.app
```

Install the resulting app in a stable location such as `/Applications/Localnet.app` before granting permissions. To **share the Mac**, enable Localnet under **Screen & System Audio Recording** (or Screen Recording on older macOS) and **Accessibility**, in **System Settings > Privacy & Security**. These two permissions are not needed just to view/control Windows from the Mac. On macOS 15+, allow **Local Network** access if prompted for either direction. Restart Localnet after granting permissions, then retry Share this computer.

Local builds are ad-hoc signed. Rebuilds can invalidate privacy approvals. For distribution, set `LOCALNET_SIGNING_IDENTITY` to your Developer ID Application identity before bundling, then notarize and staple using your Apple developer account. The script does not notarize, bypass Gatekeeper, or grant privacy permissions. Mac support is intended for macOS 12+, but the deployment target and OS compatibility still need a native Mac build/test.

## Connect

1. Open Localnet on both computers. The app automatically advertises public metadata and browses `_localnet._tcp.local.` on its selected private interface. Discovery does not capture your screen or grant input access.
2. On the computer at your keyboard, click **Connect to [computer]** under **Nearby computers**.
3. A request appears on the other computer with a random six-digit code. Enter those digits on your computer and click **Verify code**. Then click **Allow viewing and control** on the other computer. Sharing starts automatically; no long pairing string needs copying.
4. Click inside the remote image to take control. Typing, left/right/middle click, dragging, scrolling, navigation keys, F1-F12, and primary-modifier letter shortcuts are supported.
5. Use **Ctrl+Shift+F12**, click outside the image, or click **Release input** to release remote control. **Disconnect selected computer** closes only that remote session. Approved sharing automatically ends on disconnect and unclaimed approvals expire after 30 seconds. **Stop sharing this computer** revokes incoming access. **Stop all** or closing the app stops sharing, viewing, discovery, and pending requests. Focus loss, leaving the viewer window, and disconnect also release input.

Every discovery connection requires a fresh code and approval. Persistent trusted-device enrollment and unattended one-click authorization are **not implemented**. Do not approve unexpected requests or disclose codes to unexpected callers. Names are unverified network data.

If discovery is blocked, use **Share this computer**, copy its long private pairing code, and enter it under **Manual pairing (advanced)**. This older manual mode remains available and allows reconnects while sharing stays enabled. Only this manual mode keeps sharing enabled between sessions.

Windows Firewall must allow the executable on **Private** networks. Discovery uses UDP 5353 (mDNS), consent TCP 45778, and sharing TCP 45777. Wi-Fi client isolation, separate VLANs, or firewalls can prevent discovery. Interface/address changes require restarting discovery; select an interface then disable/re-enable it. Nothing here modifies firewall, router, startup, or privacy settings automatically.

On a Windows viewer, Ctrl acts as the host's primary shortcut modifier: Command on Mac, Ctrl on Windows. On a Mac viewer, Command maps to Ctrl when controlling Windows and to Command when controlling another Mac. Copy/cut/paste use the **remote clipboard**, not your local clipboard. Ctrl+C from Windows into a Mac terminal therefore means Command+C, not SIGINT; full native Control shortcut support needs a future key-mapping setting. OS-reserved shortcuts are not intercepted.

Connect to up to **four computers in one window**. Use the named session tabs to choose which screen gets your input; switching releases input on the previous computer and drops stale events. Inactive sessions keep receiving video and consume bandwidth/CPU. Each shared computer currently accepts one viewer. Names/sessions exist only while the app is running; no saved computer list or persistent credentials are implemented.

You can share your own computer while connected to others. If someone connects to control this computer, outgoing remote input is paused (video continues), to avoid forwarding their injected keyboard/mouse events into another machine or forming control loops. Disconnect the incoming viewer or stop local sharing before resuming outgoing control.

For **Mac controlling Windows** specifically: open Localnet on both, select the Windows PC under Nearby computers on Mac, enter the code shown on Windows, then approve on Windows. There is no separate controller/host download, just a build of Localnet for each OS.

## Latency Design

- Independent TLS/TCP channels for input and video, both with TCP_NODELAY. Large video frames cannot hold the input connection's lock.
- One next video request is sent before decoding the current frame, overlapping capture with decoding while allowing at most one prefetched frame. Decoded frames use a latest-frame-only UI mailbox instead of an unbounded queue.
- Windows uses persistent native DXGI Desktop Duplication with a reusable staging texture where available; startup/failure falls back to xcap screenshots. GPU frames are unmapped/released before network writes.
- Native libjpeg-turbo SIMD encoding uses reusable buffers and direct BGRA input when no resize is needed. Full chroma resolution preserves colored desktop text. This is CPU JPEG acceleration, **not GPU H.264 encoding**.
- Native SIMD JPEG decoding reuses the decoder and RGBA scratch buffer. Only baseline JPEG is accepted.
- Adjacent mouse movements coalesce without crossing clicks. Input order is preserved, with a bounded backlog and disconnect on overflow.
- Three-second absolute packet deadlines and 250 ms idle heartbeats. Partial packet timeouts terminate the connection, rather than replaying bytes.
- Pointer mapping uses logical host display bounds, separate from capture pixel resolution, with letterbox correction for Retina/scaled displays.

Defaults are 1920px maximum width, quality 70, and a 60 FPS cap; reduce width/quality if slow. Use Ethernet on both machines where possible. The UI reports **input acknowledgment round-trip** and **frame receive + decode duration**, not physical input-to-photon latency. Target FPS is a cap, not a performance promise. JPEG still uses substantial bandwidth and may not feel better than established remote-desktop tools yet. A static DXGI desktop reuses the last JPEG; this does not demonstrate new frames at 60 FPS.

On this development Windows PC at 1920x1080, release benchmarks measured the original JPEG encoder around 72-75 ms/frame versus native TurboJPEG around 12-15 ms/frame. New DXGI capture plus encoding typically measured 14-23 ms, with decode plus conversion to the viewer image around 11-20 ms. Static frame reuse can be faster, but is not fresh capture. These small local benchmarks are not an end-to-end LAN, Mac, motion-video, 60 FPS, or input-to-photon measurement.

The next performance phase should use ScreenCaptureKit + VideoToolbox on Mac and hardware encoding on Windows, hardware decode, and a tightly bounded streaming transport. This prototype does **not** provide H.264/HEVC/AV1, zero-copy rendering, smooth 60 FPS, audio, or measured near-native latency.

## Security and Limits

- Discovery starts with the app, but screen sharing requires explicit code verification plus Allow, or manual Share this computer. Nothing starts at login or runs covertly.
- TLS 1.3 with normal certificate/name/signature verification plus exact leaf pinning. Discovery certificates are provisional/untrusted: SPAKE2 and bidirectional HMAC confirmation bound to the TLS exporter authenticate knowledge of the short code before approval. The code is never published or transmitted as plaintext. The host's fresh sharing certificate/credential is transferred only on that verified connection. Manual codes enroll the certificate out of band. No insecure TLS verifier or OS-wide trust installation.
- The SPAKE2 implementation and this integration have not received an independent security audit. One random code, one attempt per request, 90-second approval expiry, five-second request throttling, bounded prompts/workers, and deny-by-default are safeguards, not a claim of production-hardening. Code verification alone does not start capture or permit input.
- Fresh in-memory certificate and 256-bit access credential whenever Host starts. Stopping/restarting sharing invalidates the old code. Credentials are not stored in configuration files or logged, but copying the code uses the local clipboard, which can retain it.
- Bind only to the selected private interface (manual sharing also supports loopback); reject public IP routes/peers. mDNS publishes only name/version/public certificate and excludes loopback candidates. No cloud relay, NAT traversal, shell commands, or file transfer. Private IP checks are not proof of the same physical LAN: VPN/private routed networks and router port forwarding still require care. Do not expose this prototype to the Internet.
- Resource-bounded connections, packets, input batches, discovered-device UI cache, and JPEG dimensions. Decoder output is checked before allocation (3840x2160 and 32 MiB maximum RGBA); progressive/lossless/arithmetic JPEG is rejected and a scan limit is configured. Native decoder internal allocations are not an OS-enforced memory budget. This is not hardened against a hostile LAN's denial-of-service traffic; the mDNS library's full internal cache is not bounded by our UI cache limit.
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
