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
$ blueterm scan
NAME                      RSSI  ID
raspberrypi                -58  2C:CF:67:01:02:03
$ blueterm raspberrypi
Looking for raspberrypi...
Connecting to raspberrypi (2C:CF:67:01:02:03)...
Connected to raspberrypi. Escape character is '~'; type ~? for help.

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
| `max_sessions` | `1` | advertising stops while all sessions are in use |
| `login_command` | `["/bin/login"]` | runs on the session's pty |

Restart the service after changing the configuration:
`sudo systemctl restart blueterm-agent`. Logs go to the journal:
`journalctl -u blueterm-agent`. For more detail, set `BLUETERM_LOG=debug` in a
drop-in override.

## Troubleshooting

- **`blueterm scan` finds nothing.** Check `systemctl status blueterm-agent`
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

In `authenticated` mode, RX and Control require an encrypted, authenticated
link. BlueZ can't protect notification subscriptions the same way, so a
session only starts after the client's first write. This means an unpaired
device can connect, but it never gets a session.

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
