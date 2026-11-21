# blueterm — Implementation Plan

Log in to a Linux machine from a Mac over Bluetooth Low Energy, as if it had a
serial console. It uses the Nordic UART Service (NUS) GATT profile.

- **Linux side:** `blueterm-agent`, a systemd service shipped as a `.deb`. It
  advertises NUS. When a client connects, it runs `login` on a fresh PTY.
- **Client side (macOS and Linux):** `blueterm`, a single CLI binary. It finds
  agents, connects to one, and passes the user's terminal through in raw mode.
  Terminal emulation is left to whatever terminal app the user runs.
  - macOS: a signed and notarized universal binary.
  - Linux: a separate `.deb` (`blueterm`). A machine can install the client,
    the agent, or both.

Language: Rust (edition 2024), async on `tokio`.

---

## 1. Goals and non-goals

**Goals**

- Behaves like a serial console: a login prompt, a full interactive session,
  and job control. Full-screen apps (vim, htop) work.
- Improves on a real serial line in two ways: the client sends the window size
  and `TERM`.
- No runtime dependencies beyond the OS:
  - Linux: BlueZ and D-Bus, which any Bluetooth-capable system has.
  - macOS: system frameworks only.
- Secure by default. Only bonded, authenticated devices can open a session.
- Stays compatible with plain NUS. Generic NUS apps (nRF Connect, Bluefruit
  Connect) still work as dumb terminals, just without resize or TERM.

**Non-goals (for now)**

- File transfer, port forwarding, or several channels over one connection.
- Terminal emulation on the client.
- Windows client. The BLE transport sits behind a trait, so a Windows backend
  (btleplug's WinRT support) could be added later.

---

## 2. Repository layout

Cargo workspace:

```
Cargo.toml                    # [workspace], shared lints/profile
crates/
  blueterm-proto/             # UUIDs, control-message encode/decode, constants. no_std-friendly, no I/O
  blueterm-agent/             # Linux daemon (bluer, pty, systemd)
  blueterm/                   # client CLI (macOS + Linux); raw terminal, session logic
    src/transport/
      mod.rs                  # Transport trait
      macos.rs                # btleplug (CoreBluetooth); objc2-core-bluetooth if needed (R1)
      linux.rs                # bluer (BlueZ, central role)
packaging/
  linux/
    blueterm-agent.service
    agent.toml                # default config → /etc/blueterm/agent.toml (conffile)
  macos/
    Info.plist                # embedded into the binary via __TEXT,__info_plist
    entitlements.plist        # hardened runtime (empty or minimal)
.github/workflows/
  ci.yml
  release.yml
LICENSE                       # MIT
PLAN.md
README.md
```

**License: MIT.**
- Set `license = "MIT"` in `[workspace.package]`, and have each crate inherit
  it with `license.workspace = true`.
- Add a `LICENSE` file at the repo root, copyright Stefan Bethke.
- Each `.deb` installs `/usr/share/doc/<pkg>/copyright`. cargo-deb generates
  it from `LICENSE` (`license-file` in the deb metadata).

`blueterm-agent` builds only on Linux (`#![cfg(target_os = "linux")]`, plus
target-specific deps). `blueterm` builds on macOS and Linux:

- Shared code: the CLI, the session logic (escape handling, relay, control
  messages), and the terminal handling, which is plain Unix termios and
  SIGWINCH.
- Platform-specific code: only the BLE transport, chosen with
  `[target.'cfg(target_os = "…")'.dependencies]`.

---

## 3. Wire protocol

### 3.1 GATT layout

One primary service, the standard NUS service. Clients only need to scan for
one UUID.

| Role | UUID | Properties | Direction |
|---|---|---|---|
| NUS service | `6e400001-b5a3-f393-e0a9-e50e24dcca9e` | — | — |
| RX (data in) | `6e400002-b5a3-f393-e0a9-e50e24dcca9e` | write, write-without-response | client → agent |
| TX (data out) | `6e400003-b5a3-f393-e0a9-e50e24dcca9e` | notify | agent → client |
| Control (blueterm extension) | `37b7340b-aa46-4885-b0b3-2a07e496dc05` | write, notify | both |

In secure mode, RX and Control require encrypted, authenticated writes
(`encrypt-authenticated-write`, set through bluer's `CharacteristicWrite`).
BlueZ has no equivalent for notification subscriptions, and bluer doesn't
expose one, so anyone can *subscribe* to TX. For that reason a session only
starts after an authenticated *write* (§3.4). Generic NUS clients ignore the
Control characteristic.

### 3.2 Data channels

- RX and TX carry raw bytes with no framing. This keeps generic NUS
  compatibility.
- Chunk size is the negotiated ATT MTU − 3:
  - Agent: bluer reports the MTU when the client acquires the write/notify
    socket.
  - Linux client: bluer reports the MTU from `AcquireWrite`/`AcquireNotify`.
  - macOS client: use CoreBluetooth's
    `maximumWriteValueLength(for: .withoutResponse)` if btleplug exposes it.
    Otherwise fall back to a conservative 182 bytes.
- **Client → agent:** write-without-response.
  - macOS: must respect CoreBluetooth's `canSendWriteWithoutResponse` flow
    control (see risk R1).
  - Linux: the acquired write socket blocks or returns `EAGAIN`, which gives
    backpressure for free.
- **Agent → client:** notifications. Read from the PTY master in chunks of up to
  MTU − 3 bytes. When the notification socket can't keep up, stop reading the
  PTY. The kernel PTY buffer then throttles the session, which is the
  flow-control behaviour we want.

### 3.3 Control messages

Each message is one GATT write or notification: a 1-byte type followed by a
payload. Integers are little-endian. Encoding and decoding live in
`blueterm-proto` and get unit tests.

| Type | Name | Direction | Payload |
|---|---|---|---|
| `0x01` | `Hello` | C→A | `u8 proto_version`, `u16 rows`, `u16 cols`, `u8 term_len`, `term[term_len]` (ASCII, e.g. `xterm-256color`) |
| `0x02` | `Resize` | C→A | `u16 rows`, `u16 cols` |
| `0x81` | `HelloAck` | A→C | `u8 proto_version`, `u8 flags` (reserved) |
| `0x82` | `SessionEnded` | A→C | `i32 exit_status` |
| `0x83` | `Error` | A→C | UTF-8 message |

Ignore unknown types, so the protocol can grow.

### 3.4 Session lifecycle

1. The client connects, subscribes to TX and Control, and writes `Hello`.
2. The agent waits until TX has a subscriber, then for a start trigger:
   - **`Hello` arrives:** it uses the client's TERM and window size and replies
     with `HelloAck`.
   - **The first data write arrives** (a generic NUS client; the user pressed
     Enter): it uses `TERM=vt100` and 80×24. That keystroke only wakes the
     console, like on a serial line, and isn't forwarded, so it doesn't end up
     as an empty login name.
   - **`open` mode only:** if nothing arrives within `hello_timeout_ms`
     (1 s), it starts with the defaults.
   - **`authenticated` mode:** without a write within `start_timeout_s`
     (60 s), the agent disconnects the client. Notifications are unprotected,
     so an unpaired device can subscribe but never get a session.
3. The agent spawns the login command on a new PTY (see §4.3).
4. Bytes are relayed until one side ends the session:
   - **The login process exits:** the agent sends `SessionEnded`, waits briefly,
     then disconnects the device.
   - **The client disconnects:** the agent closes the PTY master, so the session
     gets SIGHUP. It then reaps the child.
5. Every new connection gets a fresh session. There's no reattach in v1.

---

## 4. Linux agent (`blueterm-agent`)

### 4.1 Dependencies

`bluer` (features `bluetoothd`, `l2cap` optional), `tokio`, `nix` or `rustix`
(pty, fork/exec, ioctl, signals), `serde` + `toml`, `clap`, `tracing` +
`tracing-journald`, `anyhow`/`thiserror`.

### 4.2 Bluetooth

- Pick the adapter from the config, falling back to the default. Power it on if
  needed.
- Register the GATT application: the NUS service with RX, TX and Control.
  - Use `CharacteristicWriteMethod::Io` and `CharacteristicNotifyMethod::Io`.
    These map to BlueZ `AcquireWrite`/`AcquireNotify`: socket-based,
    per-device, and they carry the MTU.
- Register an LE advertisement:
  - Advertisement data: the NUS service UUID.
  - Scan response: the local name, which defaults to the hostname.
  - Background: a 128-bit UUID plus flags uses 21 of the 31 bytes, so the name
    can't fit in the primary advertisement. Check how BlueZ splits the data and
    set it explicitly if needed.
- Advertise all the time while the number of sessions is below the limit
  (default 1). Stop advertising when the limit is reached.

### 4.3 PTY and login

- `openpty`, then set the window size from `Hello`.
- Fork the child. In the child:
  - `setsid()`, then `ioctl(TIOCSCTTY)`.
  - `dup2` the slave onto fds 0, 1 and 2, and close every other fd.
  - Reset the signal mask and dispositions.
  - Set a clean environment: `TERM`, plus `PATH` and `LANG` from the config.
  - `execve` the login command. The default is `["/bin/login"]`.
- In the parent:
  - Make the master non-blocking and wrap it in `tokio::io::unix::AsyncFd`.
  - Reap the child with `waitpid` on SIGCHLD, using `tokio::signal::unix`.
- `Resize` → `TIOCSWINSZ` on the master. The kernel sends SIGWINCH to the
  foreground process group.
- utmp/wtmp: `login` writes these itself when it runs on a real tty, and a PTY
  counts. Check that `who`/`last` show the session. If they don't, write utmp
  entries ourselves.
- Optionally, the login command can be `agetty` on the slave's path, for an
  issue banner. Document it, but default to `login`.

### 4.4 Security and pairing

There are two security modes, set by `security` in the config:

- **`authenticated`** (default): characteristics need an encrypted link with
  MITM protection, so only bonded devices can talk to the agent. The adapter is
  **not pairable** by default.
- **`open`**: Just Works / no encryption. Meant for bench testing. The agent
  logs a loud warning at startup.

Pairing a new Mac:

- An admin runs `sudo blueterm-agent pair [--timeout 120]` locally or over SSH.
  This makes the adapter pairable and discoverable for the timeout period. It
  also registers a bluer `Agent` with `DisplayYesNo` IO capability. It prints
  the passkey (passkey entry) or asks the admin to confirm it (numeric
  comparison). It refuses Just Works pairing, because that would not give an
  authenticated bond.
- macOS shows its system pairing dialog. The user types or confirms the
  passkey.
- When the timeout ends or pairing succeeds, the adapter goes back to
  non-pairable.
- The running daemon and the `pair` command coordinate over a Unix socket in
  `/run/blueterm/`, or the `pair` command just drives BlueZ over D-Bus directly.
  Prefer the second option, since it needs no IPC, as long as the daemon's
  advertisement doesn't interfere.
- Also add `blueterm-agent devices` (list bonded devices) and
  `blueterm-agent unpair <addr>`.

Other security notes:

- PAM: Debian's `/etc/pam.d/login` may include `pam_securetty`, which can stop
  root logging in on `pts/*`. Document this rather than working around it.
- The agent runs as root, because `login` needs it. Use what systemd hardening
  is compatible with that: `ProtectHome=read-only` does **not** work, because
  the login session needs home directories. Keep the unit minimal and correct
  rather than over-hardened.

### 4.5 Configuration (`/etc/blueterm/agent.toml`)

```toml
# name = "myhost"            # advertised name; default: hostname
# adapter = "hci0"           # default: BlueZ default adapter
security = "authenticated"   # or "open"
max_sessions = 1
login_command = ["/bin/login"]
hello_timeout_ms = 1000
# idle_timeout_s = 0         # 0 = never
```

### 4.6 CLI

```
blueterm-agent [run]            # default; what systemd invokes
blueterm-agent pair [--timeout N]
blueterm-agent devices
blueterm-agent unpair <ADDR>
blueterm-agent --config PATH ...
```

### 4.7 systemd unit

```ini
[Unit]
Description=blueterm BLE serial console agent
Requires=bluetooth.service
After=bluetooth.service

[Service]
ExecStart=/usr/bin/blueterm-agent run
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

---

## 5. Client (`blueterm`, macOS and Linux)

### 5.1 Dependencies

- Common: `tokio`, `rustix::termios` (raw mode, `TIOCGWINSZ`),
  `tokio::signal::unix` (SIGWINCH), `clap`, `anyhow`. The client only targets
  Unix, so crossterm isn't needed.
- macOS: `btleplug`. Fall back to `objc2-core-bluetooth` if the spike shows
  that's needed (R1/R2).
- Linux: `bluer`, in central role:
  - Discover devices through `Adapter::discover_devices` with a UUID filter.
  - Connect with `Device::connect`.
  - Look up the characteristics, then use `Characteristic::write_io` and
    `notify_io` to get socket-based I/O with a known MTU.
  - Pairing goes through bluer's `Agent` (see §5.5).

`btleplug` would also run on Linux. Native `bluer` is preferred there because
it gives the MTU, socket backpressure and pairing control. It's also the same
library the agent uses, so we only need to learn one BlueZ binding.

**Transport trait.** It lives in `crates/blueterm/src/transport/mod.rs`. Each
platform implements it:

- `scan(timeout) -> Stream<Discovered { name, id, rssi }>`
- `connect(id) -> Link`
- On `Link`: `mtu()`, `send_data`, `recv_data`, `send_control`,
  `recv_control`, `disconnect`

The session logic and the tests (§8) depend only on the trait.

### 5.2 CLI

```
blueterm scan [--timeout 5s]          # list agents: name, RSSI, id (macOS: peripheral UUID, Linux: MAC)
blueterm [connect] <NAME|ID>          # connect and attach
    --term <TERM>                     # default: $TERM
    --escape <CHAR|none>              # default: ~
    --scan-timeout <DUR>
    --adapter <hciN>                  # Linux only; default: BlueZ default adapter
    -v / --verbose                    # logs to stderr before raw mode, to a file afterwards
blueterm pair <NAME|ID>               # Linux only: pair/bond with an agent (see §5.5)
```

- Identify targets by advertised name, or by platform ID:
  - macOS: CoreBluetooth hides MAC addresses and gives each peripheral a UUID
    that is only stable on this Mac.
  - Linux: the Bluetooth address. Detect it from the `XX:XX:XX:XX:XX:XX`
    format.
- If several agents share a name, fail and list the candidates with their IDs.
- Optional: cache name → ID so a reconnect can skip the scan.
  - macOS: `~/Library/Caches/blueterm/`, using
    `retrievePeripherals(withIdentifiers:)`.
  - Linux: `$XDG_CACHE_HOME/blueterm/`. Already-known BlueZ devices can be
    connected without scanning.

### 5.3 Session behaviour

- Scan, connect, discover services, and check that RX, TX and Control exist.
  Subscribe to TX and Control, then send `Hello` with `$TERM` and the current
  size.
- Put the local tty into raw mode. Restore it on every exit path: a drop guard
  plus a panic hook.
- On SIGWINCH, send `Resize`.
- **stdin → RX:** chunk to the MTU and respect write-without-response flow
  control.
- **TX notifications → stdout:** write them through unchanged.
- Escape sequences: `~` is only special right after a CR/LF, or at the start of
  the session.
  - `~.` disconnects.
  - `~~` sends a literal `~`.
  - `~?` shows help.
  - Possibly `~B` to send a break, mapped to a control message later.
- On `SessionEnded` or a disconnect, restore the tty and print
  `Connection to <name> closed.`
  - The exit code is the remote exit status when known. It's 255 when the
    connection is lost.
- If the connection fails because of insufficient authentication, tell the user
  to run `blueterm-agent pair` on the target.
  - macOS: CoreBluetooth starts pairing itself when it hits the encrypted
    characteristic, so the error is usually either a timeout or a user cancel.
  - Linux: also suggest `blueterm pair <name>` on the client.

### 5.4 macOS specifics

- **Bluetooth permission:** macOS asks for Bluetooth access on behalf of the
  *responsible* app, i.e. the terminal emulator. The binary must still contain
  an Info.plist with `NSBluetoothAlwaysUsageDescription`. Without it, macOS
  kills the process when it first touches CoreBluetooth.
  - Embed `packaging/macos/Info.plist` from `crates/blueterm/build.rs` using
    `cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,<path>`.
  - Include `CFBundleIdentifier` (`io.github.stblassitude.blueterm`),
    `CFBundleName`, `CFBundleShortVersionString` (filled from
    `CARGO_PKG_VERSION` at build time), and the usage description.
- Detect "Bluetooth off" and "permission denied" (CBManagerState) and print
  clear messages rather than hanging.
- Minimum macOS version: 12. Set `MACOSX_DEPLOYMENT_TARGET`.
- `build.rs` embeds the Info.plist only when
  `CARGO_CFG_TARGET_OS == "macos"`.

### 5.5 Linux specifics

- **Permissions:** BlueZ central operations over D-Bus work for ordinary users
  under the default policy. On Debian/Ubuntu, a local session (or membership of
  the `bluetooth` group) is enough. No root is needed. Document the
  `org.bluez` D-Bus policy for headless or SSH sessions, where there may be no
  "active seat" and access gets denied.
- **Pairing:** unlike macOS, BlueZ needs a registered pairing agent before it
  can bond.
  - Desktop machines usually have one (GNOME or KDE), and pairing pops up
    there.
  - Headless machines have none. That's what `blueterm pair <NAME|ID>` is for:
    1. Register a bluer `Agent` with `KeyboardDisplay` capability. It prompts
       for or confirms the passkey on the terminal.
    2. Call `Device::pair()`, then set `trusted = true`.
    3. Unregister the agent.
  - The agent side runs `blueterm-agent pair` at the same time, and the two
    sides compare passkeys.
  - `blueterm connect` registers no agent. A Linux desktop's own agent
    (GNOME/KDE) handles pairing inline. On headless machines, connecting
    without a bond fails with a hint to run `blueterm pair`.
- **Adapter state:** detect a powered-off adapter or an rfkill block and say so
  clearly. Don't power the adapter on silently.
- **Client and agent on the same machine:** allowed. If they share an adapter,
  ignore the machine's own advertisement when scanning.

---

## 6. Packaging

### 6.1 Debian package (`cargo-deb`)

Metadata in `crates/blueterm-agent/Cargo.toml` under
`[package.metadata.deb]`:

- `depends = "$auto, bluez (>= 5.50)"`. `$auto` pulls in `libdbus-1-3` and libc
  through `dpkg-shlibdeps`.
- Assets:
  - The binary → `/usr/bin/`.
  - `agent.toml` → `/etc/blueterm/agent.toml`, marked as a conffile.
  - README → `/usr/share/doc/blueterm-agent/`.
- `systemd-units = { unit-name = "blueterm-agent", enable = true, start = true }`.
  cargo-deb generates the postinst/prerm/postrm snippets.
- Section `admin`, priority `optional`, plus copyright and license.

**glibc baseline.** Build on the oldest distro we support, so the binary runs
on Debian 12 (glibc 2.36) and Ubuntu 22.04 (2.35):

- Use `ubuntu-22.04` and `ubuntu-22.04-arm` runners, or a `debian:bookworm`
  container.
- Don't build on `ubuntu-latest`, since its glibc is too new.

**Architectures:** `amd64` and `arm64` built on native runners, which avoids
cross-compiling libdbus. Add `armhf` (older Raspberry Pis) later through
`cross` if anyone asks.

### 6.2 Debian package for the client (`blueterm`)

The client gets its own package, with metadata in
`crates/blueterm/Cargo.toml` under `[package.metadata.deb]`:

- `depends = "$auto, bluez (>= 5.50)"`. The client needs `bluetoothd` running
  locally.
- Assets: the binary → `/usr/bin/blueterm`, plus the README and (later) a man
  page.
- No systemd unit and no conffiles.
- It doesn't conflict with `blueterm-agent`, so both can be installed together.

It uses the same glibc baseline and architectures as §6.1, and is built by the
same matrix jobs.

Optional extra: a plain `blueterm-<ver>-linux-<arch>.tar.gz` for non-Debian
distros. It still links libdbus dynamically, which is present wherever BlueZ
is.

### 6.3 macOS binary

- Universal binary: build `aarch64-apple-darwin` and `x86_64-apple-darwin`,
  then `lipo -create`.
- Sign with `codesign --force --options runtime --timestamp
  --entitlements packaging/macos/entitlements.plist --sign "Developer ID
  Application: …"`.
  - Hardened runtime needs no special entitlement for Bluetooth, because
    `com.apple.security.device.bluetooth` only applies to sandboxed apps.
- Notarize: zip the binary, then `xcrun notarytool submit --wait`, using an App
  Store Connect API key.
  - A bare Mach-O binary can't be stapled. Gatekeeper checks the notarization
    online on first run.
- Release artifacts:
  - `blueterm-<ver>-macos-universal.zip`: the signed and notarized binary.
  - Optional, phase 2: a `.pkg` signed with "Developer ID Installer" and
    stapled, which installs to `/usr/local/bin`.
  - Optional, phase 2: a Homebrew tap formula pointing at the zip.

---

## 7. GitHub Actions

### 7.1 `ci.yml` (push and PR)

| Job | Runner | Steps |
|---|---|---|
| lint | ubuntu-22.04 | `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` (proto, agent, client with Linux transport) |
| test-linux | ubuntu-22.04 | `cargo test --workspace` |
| test-macos | macos-14 | `cargo clippy -p blueterm -p blueterm-proto -- -D warnings`, `cargo test -p blueterm -p blueterm-proto` |
| deb-smoke | ubuntu-22.04 | `cargo deb -p blueterm-agent` and `cargo deb -p blueterm`, `lintian` (warnings allowed initially), `dpkg -i` of both in a `debian:bookworm` container to check installability |

Cache with `Swatinem/rust-cache`. Pin the toolchain with `rust-toolchain.toml`.

### 7.2 `release.yml` (tag `v*`)

1. `build-linux` matrix `{amd64: ubuntu-22.04, arm64: ubuntu-22.04-arm}`:
   - `cargo deb -p blueterm-agent` and `cargo deb -p blueterm`.
   - Optionally, a client tarball.
   - Upload the artifacts.
2. `build-macos` on `macos-14`:
   - Build both targets and `lipo` them together.
   - Import the signing certificate into a temporary keychain.
   - `codesign`, then `codesign --verify --strict`, then
     `spctl --assess --type execute` (after notarization).
   - Notarize, zip, upload.
3. `release`:
   - Create the GitHub Release and attach four `.deb` files (agent and client,
     each for two architectures), the macOS zip, and `SHA256SUMS`.
   - Run `actions/attest-build-provenance` on every artifact.

**Required secrets** (Settings → Secrets and variables → Actions → Secrets):

| Secret | Purpose |
|---|---|
| `MACOS_CERT_P12_BASE64` | Developer ID Application cert + private key (.p12, base64) |
| `MACOS_CERT_PASSWORD` | p12 password |
| `MACOS_KEYCHAIN_PASSWORD` | temp keychain password (any random value) |
| `APPLE_API_KEY_P8_BASE64` | App Store Connect API key for notarytool (.p8, base64) |

**Required variables** (same page, Variables tab), since these are identifiers,
not secrets:

| Variable | Purpose |
|---|---|
| `APPLE_TEAM_ID` | team ID; selects the signing identity |
| `APPLE_API_KEY_ID` | App Store Connect API key ID |
| `APPLE_API_ISSUER_ID` | App Store Connect issuer ID |

**Deb signing.** Individual `.deb` signatures (`dpkg-sig`) are effectively dead,
and apt doesn't check them. To get authenticated packages:

- Phase 1: GitHub Release assets with SHA256SUMS and build provenance
  attestations, which can be checked with `gh attestation verify`.
- Phase 2: an apt repository on GitHub Pages:
  - `apt-ftparchive` or `reprepro` generates the repo, with a signed
    `InRelease` (secret `APT_GPG_PRIVATE_KEY`).
  - The README documents the `signed-by=` keyring setup.

---

## 8. Testing strategy

- **Unit tests:**
  - `blueterm-proto` encoding and decoding, including round-trips and
    malformed input.
  - The client's escape-sequence state machine.
  - MTU chunking.
- **Transport abstraction:** both binaries put their session logic behind a
  small trait (`send_data`, `recv_data`, `send_control`, `recv_control`). This
  allows in-memory tests:
  - Agent: run a session against an in-memory transport, with
    `login_command = ["/bin/sh", "-c", "echo hi; exit 3"]`. Assert the output
    and that `SessionEnded(3)` is sent.
  - Agent: check that `Resize` reaches the PTY (the child runs `stty size`).
  - Client: run against a fake transport. The raw-mode code sits behind a flag
    so tests can run without a tty.
- **Manual hardware matrix** before each release:
  - Raspberry Pi OS (bookworm, arm64) and Ubuntu 24.04 (amd64, USB dongle).
  - macOS on Apple Silicon, plus one Intel Mac if available.
  - The Linux client on Ubuntu 24.04 desktop (GNOME pairing agent), and on a
    headless Debian 12 box over SSH (`blueterm pair`).
  - Linux client → agent on a second Linux box, and client + agent on the same
    host with separate adapters.
  - Check the following:
    - Pairing.
    - Reconnecting after a reboot on both ends.
    - vim, htop and `less` with resizing.
    - Pasting a large block (flow control).
    - Ctrl-C and Ctrl-Z.
    - Logout.
    - Moving out of range (disconnect handling).
    - A generic NUS app (nRF Connect).

---

## 9. Milestones

0. **Spike (de-risk first).** Throwaway examples in the agent and client crates:
   - A bluer NUS echo server on Linux.
   - A btleplug client on the Mac that measures MTU, throughput both ways, and
     whether write-without-response drops data under load.
   - A bluer central on a second Linux box, measuring the same things.
   - The result decides R1 and R2 below.
1. **Workspace skeleton.**
   - `Cargo.toml` workspace, the three crates, `rust-toolchain.toml`, and
     `ci.yml` with lint and test.
   - `blueterm-proto` with UUIDs and control messages, plus tests.
2. **Agent MVP.** GATT and advertising, the PTY plus login session, `open`
   security mode, logging to journald.
3. **Client MVP.**
   - The transport trait plus shared session logic: `scan`, `connect`, raw
     mode, the relay, the `~.` escape.
   - **3a:** the macOS transport and the embedded Info.plist.
   - **3b:** the Linux transport (bluer), with `--adapter` and `blueterm pair`.
4. **Control channel.** `Hello`/`HelloAck`, `Resize`, `SessionEnded`, the
   fallback for generic NUS clients.
5. **Security.** `authenticated` mode as the default, plus the `pair`,
   `devices` and `unpair` subcommands and the pairing UX on macOS.
6. **Packaging.** cargo-deb metadata for both packages, the systemd unit, the
   config conffile, the deb-smoke CI job.
7. **Release pipeline.** `release.yml`: agent and client debs for both
   architectures, the macOS binary signed, notarized and made universal,
   attestations.
8. **Docs.** README covering install, pairing, troubleshooting (adapter power,
   rfkill, pam_securetty), and the protocol summary. Man pages via `clap_mangen`
   (optional).
9. **Later.**
   - An apt repository.
   - A Homebrew tap and a `.pkg` installer.
   - An L2CAP CoC transport for throughput, if needed.
   - A Windows client.
   - A `~B` break control message.

---

## 10. Risks and open questions

- **R1: write-without-response flow control on macOS. Resolved.** btleplug
  0.13 queues writes-without-response until CoreBluetooth reports
  `canSendWriteWithoutResponse` / `peripheralIsReady(toSendWriteWithoutResponse:)`,
  so no data is dropped. Still check with a large paste on real hardware.
- **R2: MTU visibility on macOS. Resolved.** btleplug derives the ATT MTU from
  `maximumWriteValueLength(for: .withoutResponse)` and exposes it as
  `Peripheral::mtu()`.
- **R3: How BlueZ splits advertisement and scan-response data** for the name.
  Check in the spike that `blueterm scan` sees full hostnames.
- **R4: Pairing UX on headless hosts.** Passkey display needs the admin running
  `blueterm-agent pair`. Check how macOS reacts to each IO capability: does it
  show "enter passkey" or "confirm"? Pick the combination that gives MITM
  protection without a display on the Linux side, beyond the admin's terminal.
- **R6: Linux client pairing and D-Bus access.**
  - Headless or SSH sessions may lack D-Bus permission for `org.bluez` (polkit
    or seat rules differ by distro). They may also have no pairing agent.
  - `blueterm pair` and the fallback agent cover the second problem.
  - The first needs documentation and maybe a shipped
    `/etc/dbus-1/system.d` snippet that grants the `bluetooth` group access.
    Check on Debian 12 and Ubuntu 22.04/24.04.
- **R5: Multiple clients at once.** v1 limits this to `max_sessions = 1`. BlueZ
  `AcquireWrite`/`AcquireNotify` are per-device, so raising the limit later
  should be simple.
- **Open:** Minimum supported BlueZ version. Assume ≥ 5.50, which covers Debian
  11 and later and Ubuntu 20.04 and later.

---

## 11. Implementation status (2026-11-21)

Milestones 1–8 are implemented. Everything is built, linted and unit-tested
on macOS and in a Debian 12 container. **None of it has been tested on real
Bluetooth hardware yet**, so milestone 0 (the spike) now serves as the first
hardware test. Verified so far:

- Protocol encoding: unit tests.
- Agent session logic: tests run `bash` on real PTYs and cover TERM and window
  size, resize, exit status, hang-up on disconnect, authenticated start, the
  swallowed wake-up keystroke, and a missing login command.
- Client relay, escape sequences and CLI parsing: unit tests.
- Both `.deb`s build and install on a clean Debian 12 image, with dependencies
  `bluez (>= 5.50), libc6 (>= 2.34), libdbus-1-3`. The systemd
  postinst/prerm scripts are generated.
- The macOS binary embeds the Info.plist, and permission-denied is detected
  and explained.

Still open:

- **Hardware test** (§8 matrix).
- **lintian:** there's no changelog yet.
- **Release:** the signing and notarization secrets and variables (§7.2)
  must be added to the repository. Without them, the release workflow still runs and publishes
  an `-unsigned` macOS zip.
- **R7: control writes with response through `AcquireWrite`.** The agent
  receives every write through BlueZ's acquired socket, including writes with
  response such as `Hello`. BlueZ is expected to route both kinds of write
  there; confirm this on hardware.
