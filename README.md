# term73

> [!WARNING]
> This project is a work-in-progress and does not carry any guarantee. Using this project with a radio, even a model that's been tested, may be risky.
> Please use caution when operating to protect your radio from data loss or permanent damage.

term73 is a terminal program for packet radio that aims to quickly add support for new radios and control interfaces.

![term73](docs/screenshot.png)

> Status: Work in progress. Tested against the Kenwood TH-D75 and TM-D750 over Bluetooth, and against a simulated modem73.

## What it does

- **Finds your rig.** Scans Bluetooth, serial devices, and local ports for radio and control interfaces.
- **Learns the rig.** Probes the rig for capability and behavior, such as serial channel, power levels, frequency ranges, packet mode quirks.
- **Connects.** Connected-mode AX.25 sessions to stations, nodes and BBSes, with digipeater paths. term73 tries AX.25 v2.2 first and falls back to v2.0 when the other station does not answer it; saved BBSes remember which version worked.
- **Listens.** Shows packet traffic as it is heard and can save it to a file.
- **Winlink.** Lets a Winlink client (for example Pat) send and receive mail through the radio's own TNC, picking the nearest gateway and tuning to it.
- **Software modems.** A running modem73 can be used as the rig: term73 proxies application layer through modem73.

## Safety

- Tested with: Kenwood TH-D75A, Kenwood TM-D750A.
- Keying commands are disabled on each start. Use `/transmit on` to enable.
- Discovery only sends read commands from a known list.
- Reset and service commands are not allowed.
- When term73 is configured to use an interface that has control of a radio, term73 only communicates with the interface through their API.

## Requirements

- A radio with a built-in TNC and Bluetooth serial, or a running software modem that offers KISS over TCP (for example modem73)
- To build: Rust 1.88 or newer (`rustup`, then `cargo build --release`)

## Quick start

```
cargo run --release
```

or install it with `cargo install --path .` and run `term73`.

On first start term73 asks for your callsign. Then:

```
/radio scan                 find your radio, then type its number
/transmit on                allow transmitting for this session
/listen                     watch packet traffic on the current frequency
/connect N0BBS-3            talk to a station or BBS
/disconnect
```

Type `/help` for the everyday commands and `/help advanced` for raw rig-control and TNC commands. Tab completes commands.

Every command is also in the menu bar: press F10, or Alt with the highlighted letter (Alt+B for BBS), then the arrow keys or the item's letter. Items ending in "..." open a dialog for the details. F1 shows help and Alt+X quits.

## Commands

| Command | What it does |
|---|---|
| `/connect <CALL> [via D1,D2]` | Connected session. Lines you type go to the station. |
| `/disconnect` | End the session. |
| `/listen [MHz]`, `/listen save <file>`, `/listen off` | Show packet traffic, optionally saving it. |
| `/frequency <MHz>` | Tune the data band. |
| `/power high\|mid\|low` | Set the data band's power. |
| `/transmit on\|off` | Permission to transmit. |
| `/bbs add\|edit <name>\|list\|connect <name>\|remove <name>` | Saved BBSes: callsign, frequency, digipeaters, speed. |
| `/winlink setup\|gateways\|start\|stop` | Gateway list, nearby gateways, and the Winlink client connection. |
| `/radio scan [all]\|list [all]\|select <n>\|setup\|info\|off` | Find, choose and profile a rig. The last scan is kept across restarts. |
| `/config show\|callsign <CALL>\|grid <LOCATOR>` | Settings. |

term73 switches the radio's TNC in and out of packet mode by itself. You do not need to know about KISS or rig-control commands unless you want to.

## Winlink

`/winlink setup` asks for your grid locator and a Winlink API key, then downloads the list of gateways. The Winlink API does not serve the gateway list without a key; keys are issued by the Winlink Development Team. The key is stored in term73's own config folder.

`/winlink start` then listens on `127.0.0.1:8772`. Point your Winlink client's telnet connection there, for example with Pat:

```
pat connect "telnet://N0CALL:CMSTelnet@127.0.0.1:8772/wl2k"
```

term73 answers the telnet login itself, connects to the nearest gateway over the air (trying the next one if it gets no answer), and passes the Winlink session through unchanged. Your client still handles the Winlink protocol and your password.

## Software modems

modem73 is found by `/radio scan` when it is running (control port 8073, KISS port 8001) and can be selected like a radio. term73 then sends its packets through modem73, and tunes through modem73's own rig control when that is set up.

## Rig control for other programs

`/advanced rigctl start` runs a server on `127.0.0.1:4532` that speaks Hamlib's rigctld protocol. Programs that support "Hamlib NET rigctl" (for example WSJT-X, Gpredict or fldigi) can then read and set the frequency, read the mode and band, set the power (high, mid, low) and read the busy signal through term73, including for radios Hamlib itself does not support, such as the TM-D750. Transmit (PTT) is only offered once keying has been verified for the radio and `/transmit on` is set. Anything not yet confirmed for the radio answers "not available".

## Configuration

- Windows: `%APPDATA%\term73`
- macOS: `~/Library/Application Support/term73`
- Linux: `$XDG_CONFIG_HOME/term73` (usually `~/.config/term73`)

## Platform notes

- **Windows:** radios are reached directly by Bluetooth address (the serial channel is looked up from the radio) or through the COM port Windows creates when you pair.
- **macOS:** paired radios appear as `/dev/cu.*` serial ports.
- **Linux:** radios are reached by Bluetooth address (`bluetoothctl` and `sdptool` are used to find them) or through `/dev/rfcomm*`.

Color is enabled when the terminal supports it, this is overridable.

Available themes:
- Turbo Vision, default.
- `--theme seafoam` CDE SeaFoam style.
- `--theme modem73` a dark theme on the terminal's own background inspired by modem73.

## Development

```
cargo test
```

The tests are setup against a mock radio with KISS TNC, a simulated BBS and Winlink gateway on a lossy link (on a virtual clock), and a simulated modem73.

`cargo run --example preview -- out.html [turbo|seafoam|modem73] [menu|popup]` renders the screen against the mock radio.

## Thanks

- [modem73](https://github.com/RFnexus/modem73), whose KISS and control-port interfaces term73 builds on, and whose terminal style inspired ours.
- The default look follows Borland's Turbo Vision. The SeaFoam color scheme comes from the Common Desktop Environment.

## License

MIT. Copyright (c) 2026 term73 authors and contributors. See [LICENSE](LICENSE).
