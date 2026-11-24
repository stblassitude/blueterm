# blueterm

A serial console over Bluetooth Low Energy.

- **`blueterm-agent`** runs on a Linux machine. It advertises the Nordic UART
  Service (NUS) and gives each connecting client a fresh `login` prompt on a
  pseudo-terminal, as if a serial cable were plugged in.
- **`blueterm`** runs on macOS or Linux. It finds agents nearby and connects
  your terminal to one. Terminal emulation is whatever terminal app you
  already use.

Unlike a real serial line, blueterm passes the terminal type (`TERM`) and window
size, including resizes, to the remote session. It also reports the session's
exit status.

Any generic NUS app (nRF Connect, Bluefruit Connect, …) also works as a basic
terminal, just without those extras.

## Install

### Linux agent

```sh
sudo apt install ./blueterm-agent_<version>_<arch>.deb
```

The package installs and starts the `blueterm-agent` systemd service. Its
configuration is in `/etc/blueterm/agent.toml`. It needs BlueZ 5.50 or newer
and runs on Debian 11+, Ubuntu 22.04+ and Raspberry Pi OS (bookworm).

### Linux client

```sh
sudo apt install ./blueterm_<version>_<arch>.deb
```

### macOS client

Download `blueterm-<version>-macos-universal.zip` from the releases page.
Unzip it and move `blueterm` somewhere in your `PATH`, for example
`/usr/local/bin`. The binary is signed and notarized.

The first time you run it, macOS asks whether your *terminal app* (Terminal,
iTerm2, …) may use Bluetooth. If you said no, change it under **System
Settings → Privacy & Security → Bluetooth**.

## Pairing

By default, the agent only lets a device start a session if it is bonded with
MITM protection (`security = "authenticated"`). Anyone else in radio range
cannot even see a login prompt. To pair a new client:

1. On the Linux machine, start a pairing window:

   ```sh
   sudo blueterm-agent pair            # stays pairable for 120 s
   ```

2. On the client, connect:
   - **macOS:** run `blueterm <hostname>`. macOS shows a pairing dialog. Enter
     the passkey that `blueterm-agent pair` prints, or confirm that the numbers
     match.
   - **Linux:** run `blueterm pair <hostname>` and follow the prompts. Then use
     `blueterm <hostname>` as usual.

To manage bonds on the agent:

```sh
sudo blueterm-agent devices     # list paired devices
sudo blueterm-agent unpair AA:BB:CC:DD:EE:FF
```

## Usage

```console
$ blueterm --list
NAME                      RSSI  ID
raspberrypi                -58  2C:CF:67:01:02:03
$ blueterm raspberrypi
Looking for raspberrypi...
Connecting to raspberrypi (2C:CF:67:01:02:03)...
Connected to raspberrypi (agent reports: encrypted, authenticated). Escape character is '~'; type ~? for help.

Debian GNU/Linux 12 raspberrypi pts/1

raspberrypi login:
```

The ID column shows the Bluetooth address on Linux. On macOS, it shows a UUID
that CoreBluetooth assigns to the device, which is only valid on that Mac.
Agents are usually addressed by name. Use the ID if several agents share a
name.

Escape sequences work like ssh's and are recognized at the beginning of a line:

| Keys | Action |
|---|---|
| `~.` | disconnect |
| `~^Z` | suspend `blueterm` (resume with `fg`) |
| `~?` | help |
| `~~` | send a literal `~` |

Use `-e '^]'` to pick a different escape character, or `-e none` to turn
escapes off.

When the remote shell exits, `blueterm` exits with that shell's exit status. It
exits with 255 if the connection is lost.

## Agent configuration

See [`packaging/linux/agent.toml`](packaging/linux/agent.toml) for every option
and its default. The most important ones:

| Key | Default | Meaning |
|---|---|---|
| `name` | hostname | advertised name (at most 29 bytes) |
| `adapter` | BlueZ default | e.g. `hci1` |
| `security` | `"authenticated"` | `"open"` lets *anyone* in range get a login prompt; only for testing |
| `max_sessions` | `1` | advertising stops while all sessions are in use; more than 1 is not fully supported (see `agent.toml`) |
| `login_command` | `["/bin/login"]` | runs on the session's pty |

Restart the service after changing the configuration:
`sudo systemctl restart blueterm-agent`. Logs go to the journal:
`journalctl -u blueterm-agent`. For more detail, set `BLUETERM_LOG=debug` in a
drop-in override.

## Security

This section describes the default, `security = "authenticated"`. With
`security = "open"`, none of the Bluetooth protection applies: anyone in range
gets a login prompt, and passwords may travel unencrypted.

### Who can get in

Two independent checks stand between a nearby device and a shell:

1. **Pairing.** BlueZ only accepts writes to the agent over an encrypted link
   from a device that paired with protection against a man in the middle
   (MITM). The agent starts `login` only after such a write.
2. **Login.** The session runs `login`, so the machine's accounts, passwords
   and PAM rules apply. Pairing alone does not log anyone in.

The agent does not make the adapter pairable. `blueterm-agent pair` does so for
a limited time, and it refuses pairing without a passkey or a number
comparison. Compare the numbers: that is what protects pairing against a man in
the middle.

Any device that has an authenticated bond with the machine gets a login prompt,
even if it was not paired with `blueterm-agent pair`. Check the list with
`sudo blueterm-agent devices`, and remove lost or retired devices with
`sudo blueterm-agent unpair`.

blueterm is a separate way in, like a serial port. Firewall rules and
`sshd_config` settings, such as key-only logins or `PermitRootLogin no`, do not
apply to it. The agent runs as root, because `login` needs that.

### Encryption

Encryption is done by Bluetooth; blueterm adds none of its own. The link is
encrypted with AES-CCM, using keys agreed on during pairing. The first write
forces encryption, so everything in a session is encrypted in both directions:
keystrokes, passwords, output and control messages. The data is only protected
on the radio link. It is in the clear on both computers.

How strong the pairing is depends on the method the two devices use:

- **LE Secure Connections** (Bluetooth 4.2 and newer) uses an elliptic-curve
  key exchange. Someone who records the pairing cannot work out the keys.
- **LE legacy pairing** (older devices) is weak: someone who records the pairing
  can find the six-digit passkey by trying them all, and then decrypt the
  traffic.

blueterm does not require Secure Connections. To see what a device used, run
this on the agent:

```sh
sudo grep -H -E '^(Authenticated|EncSize)' /var/lib/bluetooth/*/*/info
```

`Authenticated=3` means Secure Connections with MITM protection, and `1` means
legacy pairing with MITM protection. `EncSize` is the key length in bytes; 16 is
the full 128 bits. To rule out legacy pairing, set `SecureConnections = only` in
`/etc/bluetooth/main.conf` and restart Bluetooth. This affects every device that
uses the adapter.

### What is not protected

- **The client does not verify the agent.** An agent's name can be copied by
  anyone, and `blueterm` does not check that the link is encrypted or that it
  reached a device you paired with. A fake agent could show a login prompt and
  collect your password. Be suspicious if a known machine asks to pair again, if
  the prompt looks different, or if `blueterm` reports a plain NUS device.
  The `agent reports: encrypted, authenticated` in the "Connected" line is only
  what the agent says, so a fake agent can claim it too. It tells you that a
  genuine agent is set up securely. `encryption: not reported by agent` means an
  agent in `open` mode, an older agent, or a device that is not an agent.
- **Presence is public.** The agent advertises its name (by default the
  hostname), the service and its Bluetooth address to everyone in range.
- **Availability.** An unpaired device can connect and occupy a session slot
  without logging in, for up to 65 seconds at a time with the default settings.
  With `max_sessions = 1`, nobody else can connect meanwhile. Bluetooth can also
  be jammed. Do not rely on blueterm as the only way to reach a machine.

When the connection ends, the agent hangs up the session's terminal. If the
client just vanishes, that happens once Bluetooth notices that the link is gone.
The agent logs connections and sessions, but not what is typed or displayed.
Logins are logged as usual.

## Troubleshooting

- **`blueterm --list` finds nothing.** Check `systemctl status blueterm-agent`
  on the agent. Check that the adapter isn't blocked: `rfkill list`, then
  `bluetoothctl show`. On macOS, check the Bluetooth permission (see above).
- **Connecting fails with an authentication or pairing error.** The client
  isn't bonded. Pair it again (see above). If you reinstalled the OS or reset
  Bluetooth on either side, first remove the old bond on both sides: on the
  agent with `blueterm-agent unpair`, on macOS with "Forget This Device" in
  System Settings → Bluetooth.
- **root cannot log in.** Debian's `/etc/pam.d/login` may include
  `pam_securetty`, which limits root logins to the terminals listed in
  `/etc/securetty`. blueterm sessions run on `pts/*` terminals. Log in as a
  normal user and use `sudo`, or change your PAM configuration on purpose.
- **Linux client over SSH / headless.** BlueZ's D-Bus policy may refuse
  non-root users without an active local session. Add your user to the
  `bluetooth` group, or run as root.
- **Plain NUS apps.** In `authenticated` mode, the session starts with your
  first keystroke. Press Enter to get the login prompt.

## How it works

The agent offers one GATT service, the standard NUS service
`6e400001-b5a3-f393-e0a9-e50e24dcca9e`:

| Characteristic | UUID | Use |
|---|---|---|
| RX | `6e400002-…` | client → agent terminal input (write / write without response) |
| TX | `6e400003-…` | agent → client terminal output (notify) |
| Control | `37b7340b-aa46-4885-b0b3-2a07e496dc05` | blueterm control messages (write / notify) |

Terminal data is sent as raw bytes, with no framing. Control messages are a
single type byte followed by a little-endian payload:

- `Hello` (type, size, TERM)
- `Resize`
- `HelloAck`
- `SessionEnded` (exit status)
- `Error`

See [`crates/blueterm-proto`](crates/blueterm-proto/src/lib.rs) for the exact
encoding.

In `authenticated` mode, writing to RX and Control requires an encrypted,
authenticated link. Subscribing to notifications does not, so a session only
starts after the client's first write. This means an unpaired device can
connect and subscribe, but it never gets a session. See [Security](#security).

## Building

The workspace builds with stable Rust:

```sh
cargo build --release -p blueterm          # client: macOS or Linux
cargo build --release -p blueterm-agent    # agent: Linux only
```

On Linux, building needs `libdbus-1-dev` and `pkg-config`. To build Debian
packages:

```sh
cargo install cargo-deb
cargo deb -p blueterm-agent
cargo deb -p blueterm
```

## License

MIT, see [LICENSE](LICENSE).
