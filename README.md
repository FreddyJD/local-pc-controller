# Localnet

One Rust app for controlling Windows and macOS computers on your **trusted personal network**. Open Localnet on each computer, click a nearby computer, and control its screen. No accounts, pairing codes, approval dialogs, or cloud services.

## Trust Model

**Local access is enabled automatically while the app is open. Any reachable private-network peer can request screen viewing and keyboard/mouse control. Installing Localnet is not proof of device ownership.** Use only on a network where you trust every connected device. Do not expose the ports to the Internet, guest networks, shared Wi-Fi, or untrusted VPNs. Private IP ranges do not prove physical LAN membership, and Localnet does not enforce the Windows network profile.

TLS 1.3 encrypts connections and verifies the exact certificate received through discovery, but unauthenticated mDNS can be spoofed. This does **not** authenticate the intended computer against an active attacker. Transient, peer-bound, single-use session tickets coordinate the control/video connections internally; they are not user pairing credentials or an authorization barrier against a LAN peer. Nothing is stored in the clipboard or as a permanent access code.

**Disable local access / disconnect incoming viewer** stops incoming access and discovery without closing outgoing sessions. **Stop all** or closing the app stops both directions. The app does not run at startup or install a background service, firewall rule, or cloud relay.

## Connect

1. Install the same **Localnet 0.2.0** build on Windows and Mac, then open both apps. The old 0.1.0 pairing build is incompatible and appears as **Update required**.
2. Click **Connect to [computer]** under **Nearby computers**. The target starts sharing automatically, without typing a code or clicking Allow.
3. Click inside the remote image to send input. Use **Ctrl+Shift+F12**, **Release input**, or click outside the image to release remote input. Focus loss and disconnect also attempt to release held keys/buttons.
4. Click **Disconnect selected computer** when finished. Sharing ends automatically. Click the computer again to reconnect; a disconnected tab is replaced, and clicking an existing connected computer selects it instead of launching a duplicate session.

Up to four outgoing sessions fit in one window; each target accepts one viewer. If a target is busy, the viewer receives a readable busy error rather than a silent reset. Inactive tabs still consume video bandwidth/CPU. Outgoing input pauses while someone controls this computer to prevent control loops; video continues.

Names and discovery certificates are unverified network data. The computer list is not saved. Discovery uses only the selected private interface. Change interfaces under **Network and video settings**, then **Apply interface / restart local access**. Use **Enable local access and discovery** to resume after stopping it.

Windows Firewall must allow Localnet on **Private** networks. Discovery uses UDP **5353**, automatic connection startup TCP **45778**, and screen/input TCP **45777**. Wi-Fi client isolation, separate VLANs, firewall policies, and macOS Local Network permission can block discovery or connection. The app never changes these policies automatically.

## Windows Build

Requires Rust 1.92+, Visual Studio C++ build tools/Windows SDK, CMake, and NASM. CMake/NASM build a statically linked SIMD JPEG codec and are not runtime requirements.

```powershell
powershell -ExecutionPolicy Bypass -File scripts/build-windows.ps1
```

The build is `target/release/localnet.exe`. The packaged copy is `dist/Localnet-Windows.exe`, and the local per-user installation is `%LOCALAPPDATA%/Programs/Localnet/Localnet.exe` when installed. Updating the build does not automatically replace an already installed copy.

## Mac Build

Requires Rust 1.92+, Xcode Command Line Tools, and CMake (also NASM on Intel Macs). Build natively on the Mac; the Windows toolchain cannot generate the Mac executable.

```sh
sh scripts/bundle-macos.sh
open dist/Localnet.app
```

Install the app in a stable location, such as `/Applications/Localnet.app`, before granting permissions. Under **System Settings > Privacy & Security**, enable **Screen & System Audio Recording** (or Screen Recording) and **Accessibility** for Localnet. On macOS 15+, allow **Local Network** too. Use **Network and video settings > Check / request OS permissions** on the Mac for permission prompts, then restart Localnet. These OS permissions cannot be removed or bypassed by disabling application pairing. Viewing another computer does not need Screen Recording/Accessibility on the viewer.

Local builds are ad-hoc signed. Rebuilds can invalidate macOS privacy approvals. Set `LOCALNET_SIGNING_IDENTITY` to your Developer ID Application identity for signed distribution; notarization and stapling require your Apple developer account and are not done by the bundle script. Intended minimum macOS is 12, but a native Mac build and runtime testing are still required.

## Connection Errors

The earlier implementation could advertise a computer that was already sharing and then silently drop the discovery connection, producing Windows error **10054**. That is one code-supported cause, not a confirmed diagnosis of a particular Mac session.

The new version starts sharing off the UI thread, returns explicit permission/startup/busy errors, polls during slow startup instead of holding a three-second packet open indefinitely, and reports incompatible builds before attempting the old code-based protocol. Control admission/input initialization and video capture errors have explicit replies. The first observed terminal error is retained rather than always replaced by sibling-channel shutdown noise.

If the Mac still closes a connection, check the stage in the Windows error and the Mac's Localnet status. Confirm both copies are 0.2.0, close duplicate instances, grant OS permissions, and verify the firewall and selected interface. Native screen-capture calls are not cancellable by socket shutdown and can still block shutdown until the OS call returns. Unclaimed internal session tickets expire after 30 seconds; canceled startup results are discarded.

If connecting reports `invalid peer certificate: BadSignature` after the other app restarts, the discovered TLS certificate may be stale. Update the connecting computer, click **Refresh nearby computers**, then retry after the computer reappears. Certificate failures also trigger rediscovery automatically; the failed connection remains rejected. This is separate from Screen Recording or Accessibility permission. Refresh does not stop local sharing or disconnect existing viewers.

## Video and Input

- Windows uses persistent native DXGI Desktop Duplication with a reusable staging texture and screenshot fallback. Mac still uses xcap's deprecated screenshot API, not ScreenCaptureKit.
- Native libjpeg-turbo SIMD encoding/decoding uses reusable buffers and full chroma resolution for colored text. This is CPU JPEG acceleration, **not hardware H.264/HEVC/AV1**.
- Video uses one requested frame at a time and a latest-frame-only decoded mailbox. Decode completes before the next request, allowing explicit terminal-error acknowledgment and avoiding an unbounded stale-frame queue.
- Input uses a separate TLS/TCP connection with TCP_NODELAY. Adjacent pointer moves coalesce without crossing button events. Input batches and backlogs are bounded.
- Normal packet deadlines are three seconds, with 250 ms idle input heartbeats. Startup polls separately for up to 60 seconds. Partial packets terminate the session rather than replaying bytes.
- Retina/scaled pointer mapping uses host logical bounds separately from capture pixel dimensions and accounts for letterboxing.

Defaults are 1920px maximum width, quality 70, and a **60 FPS cap**, not guaranteed 60 FPS. Lower width/quality if slow and use Ethernet where possible. UI input ACK round-trip and receive/decode timings are not physical input-to-photon latency.

Previous local Windows release benchmarks at 1920x1080 measured original JPEG encoding around 72-75 ms versus native encoding around 12-15 ms, DXGI capture plus encoding typically 14-23 ms, and decoding/image conversion around 11-20 ms. These are small local pipeline measurements, not Mac/LAN/motion-video/60 FPS or near-zero-latency guarantees. A static desktop may reuse its last JPEG. Hardware encoding/decoding and ScreenCaptureKit remain future work.

On a Windows viewer, Ctrl maps to Command on a Mac target and Ctrl on Windows. On a Mac viewer, Command maps to Ctrl on Windows and Command on Mac. Copy/cut/paste use the **remote clipboard**, not your local clipboard. Ctrl+C from Windows into a Mac terminal is Command+C, not SIGINT. Full physical-keyboard fidelity, non-US shortcut mapping, native Control/Alt shortcuts, IME behavior, and game-style key holds remain incomplete. OS-reserved shortcuts stay local.

Primary monitor only. No audio, file transfer, secondary monitors, login/lock screen control, Windows UAC secure-desktop, Ctrl+Alt+Delete, or control of elevated apps from a normal process. Protected content/cursor capture can be absent. Keep the target awake, signed in, and unlocked. Held input release is best effort when permissions or OS injection fail.

## Verification

This remains a prototype, not a hardened remote-desktop product. Windows loopback capture/network paths are testable here; Mac builds, real two-computer Windows/Mac sessions, and live input fidelity must still be verified on your hardware. Native codec internal allocations and the mDNS library cache are not enforced process memory budgets; protocol packets, UI device counts, batches, and decoded output dimensions are bounded.

With CMake/NASM available on PATH:

```sh
cargo fmt --check
cargo check --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --release --locked -- --ignored --test-threads=1 --nocapture
```

Ignored tests require an unlocked desktop/private LAN and capture frames to memory only. They do not inject keyboard/mouse input. Test permission denial/revocation, host busy, disconnect/reconnect, cable removal, app closure, display changes, Retina/Windows DPI corners, dragging out of the image, focus loss with held input, and native Mac permissions before normal use.
