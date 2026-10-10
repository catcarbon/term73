# D-STAR data probe: test procedure

These tests find out how a TH-D75 carries DV Fast Data on its Bluetooth and USB links. The answers decide how term73's D-STAR data mode will be built.

Menu numbers are the TH-D75's. Before changing a setting, check that the name on the radio's display matches.

## What you need

- **Station A:** the TH-D75 under test. Connect it to term73 over Bluetooth, and also by USB cable to the same computer (for test 3).
- **Station B:** a second D-STAR radio that does Fast Data, where you can see what it receives. Either:
  - the TM-D750, if it has Data Mode (see [Station B settings](#station-b-settings)), as radio B in the same term73, or
  - an Icom radio with Fast Data (ID-52, ID-50, IC-705, IC-9700, ...) and a serial terminal program that shows hex (picocom, CoolTerm, RealTerm). Don't select an Icom in term73: term73 sends Kenwood rig commands when it connects, and an Icom in DV Data mode would transmit them.
- **A simplex frequency** where your band plan allows digital data, away from repeaters and APRS.
- **Low power**, and a dummy load on A if you have one. A few metres between the radios is plenty.
- **A stopwatch** (a phone is fine) for test 6.
- **A note of your current settings** for the menus below, so you can put them back afterwards.

term73 from this branch:

```
git fetch origin claude/elegant-euler-827lu1
git checkout claude/elegant-euler-827lu1
cargo run --release
```

One term73 can run both stations. Station A is the radio you select as usual. Station B is a second radio: put `/b` in front of a command to run it on B, and B's lines start with `[B]`. In the tests, "B: `/advanced dv sweep`" means typing `/b /advanced dv sweep`. Each radio has its own transmit permission, so B needs its own `/b /transmit on`.

## Station A settings (TH-D75)

| Setting | Value | Why |
|---|---|---|
| Band mode | DV (not DR), on the band you transmit on | DR routes through repeaters |
| Frequency | the test frequency, simplex, no offset | |
| UR call | CQCQCQ | simplex |
| Digital squelch | Off | the radio passes on everything it receives |
| Power | Low | |
| 610 My Callsign | your callsign | identifies each transmission |
| 611 TX Message | Off | the message text would mix into the data |
| 614 Data TX End Timing | 0.5 s | at Off the radio accepts data but never transmits |
| 618 Data Frame Output | All | test 4 also tries DATA Mode |
| 630 GPS Info in Frame | Off | position data would mix into the data |
| 632 Auto TX (GPS) | Off | no position beacons keying the radio |
| 650 DV Gateway Mode | Off | Reflector TERM takes over the link |
| 980 USB Function | COM+AF/IF Output | the USB serial port only exists in this mode |
| 981 PC Output (GPS), 982 PC Output (APRS) | whichever of USB and Bluetooth you are not using for data, or the GPS's PC output off | NMEA sentences would mix into the data |
| 984 DV/DR (PC Input/Output) | Bluetooth to start; the tests change it | where data goes in and out |

**Data Mode is not a menu item.** In DV, press [F], then [MODE], and choose Data Mode. The DATA icon appears next to DV. Do the same again to turn it off. After every menu change, check that the DATA icon is still there.

## Station B settings

- **TM-D750:** match station A wherever it has the same setting: Data Mode, Data TX End Timing 0.5 s, Data Frame Output All, GPS info in frame off, TX message off, digital squelch off, DV on the test frequency, UR CQCQCQ. The menu numbers differ from the TH-D75's, so look them up in the TM-D750 manual. Set its DV/DR data interface to the link its term73 uses.
- **Icom:** DV mode on the test frequency, Fast Data on, DV Data TX Auto, and the data port (USB or Bluetooth) set to DV Data. Also GPS TX off, TX message off and digital squelch off. Menu paths differ by model.

## Start term73

**Connect before turning Data Mode on.** term73 sends rig commands as it connects (ID, TN, BC, FQ...), and a TH-D75 in Data Mode transmits them as data when menu 984 points at that link (seen in testing). If the radio does not answer when term73 connects, term73 stops after `ID` and sends no more rig commands to it. Turn Data Mode off, then select the radio again.

Station A:

```
/radio select <n>          the TH-D75 over Bluetooth
/config callsign <CALL>    if not set yet
/advanced dv monitor on
```

Station B, if it is the TM-D750 as radio B:

```
/b /radio select <n>
/b /advanced dv monitor on           or: /b /advanced dv monitor on <port>, if its data goes to another port
```

term73 will not let both radios open the same device. If B's profile was saved under another address, `/radio select` offers it.

Now turn Data Mode on at both radios.

**Wire logs.** Every byte also goes into a wire log in term73's `logs` folder: Linux `~/.config/term73/logs`, macOS `~/Library/Application Support/term73/logs`, Windows `%APPDATA%\term73\logs`. A separate data port gets its own file, whose name starts with `dv_`. Each radio has its own log.

**If a radio stays keyed:** run `/advanced dv monitor off`, which drops anything term73 has not sent yet. If the radio still transmits, turn Data Mode off or switch the radio off.

## Test 0: baseline, Data Mode off

This shows what the radio does with data bytes when it is not in Data Mode.

1. A: Data Mode off. Menu 984 = Bluetooth.
2. A: `/transmit on`, then `/advanced dv send #hello`.
3. Expected: A does not transmit, and A's monitor shows `3f 0d` (`?.`). That is the radio answering `#hello` as an unknown command. Note what you actually see.

## Test 1: data on the same link as rig control

Already seen on the TH-D75: with Data Mode on and menu 984 pointing at term73's link, the radio answers no rig commands (`(no reply)`, "unknown radio") and transmits them as data. So don't send rig commands (`/advanced cat`, `/frequency`, `/listen`...) to a radio in Data Mode on that link. Run anything that needs rig commands with menu 984 set to USB (tests 2 and 3).

What is left to check is the data itself:

1. A: connect with Data Mode off, then turn Data Mode on. Menu 984 = Bluetooth.
2. A: `/advanced dv send #hello`, then check:
   - Does A transmit, and does B receive `#hello`?
   - Does anything come back on A's monitor?

On macOS: MacRats found that DV data did not flow through `/dev/cu.TH-D75`. If test 1 shows nothing at all on a Mac, that may be why; try test 3 (USB).

## Test 2: rig commands with data on USB

1. A: menu 984 = USB, with the USB cable connected and Data Mode still on.
2. A: `/advanced cat ID`, then check:
   - Is there an answer?
   - Does A transmit?
   - Does anything arrive at B?

## Test 3: separate data port

1. Find A's USB serial port:
   - Linux: `ls /dev/ttyACM*`
   - macOS: `ls /dev/cu.usbmodem*`
   - Windows: Device Manager, Ports
2. A: `/advanced dv monitor off`, then `/advanced dv monitor on <port>`.
3. A: `/advanced dv send #hello over usb`. Does A transmit, and does B receive it?
4. B: `/transmit on`, then `/advanced dv send #hello back`. Does A's monitor on the USB port show it?
5. A: `/advanced cat ID`. Does Bluetooth still answer, and does A transmit?

## Test 4: what the radio outputs on receive

A monitors the link that worked in tests 1-3. Menu 984 points at that link.

1. A: menu 618 = All. B: `/advanced dv send #rx all`.
   - Note A's hex dump. Is it exactly `23 72 78 20 61 6c 6c 0d`?
   - Or are there extra bytes before or after it (callsigns, headers, `00`, `0a`)?
2. A: menu 618 = DATA Mode. B: `/advanced dv send #rx data mode`. Note the dump.
3. Optional: A: menu 618 = Related to DSQ. B: `/advanced dv send #rx dsq`.
4. Optional: B turns GPS on, with GPS Info in Frame on, then sends `/advanced dv send #rx gps`. Do NMEA or `$$CRC` lines appear at A? Turn B's GPS info off again afterwards.

Set A's menu 618 back to All.

## Test 5: which byte values survive

1. A: `/advanced dv sweep`. When the reception ends, B's monitor shows a `sweep: ...` line. Note it.
2. The other way: B: `/advanced dv sweep`. Note A's `sweep: ...` line.

## Test 6: burst size and key-down time

For each size, wait for B's reception summary before sending the next one.

1. A: `/advanced dv burst 200`. Time how long A's TX indicator stays on. Note B's `burst:` and `reception:` lines.
2. Repeat with 400, 800 and 1600.
3. A: `/advanced dv rate 300`, then `burst 800` and `burst 1600`. Then `rate 400` and `burst 1600`.
4. A: `/advanced dv rate off`.

Fast Data runs at about 3480 bit/s, or about 435 bytes/s. So 800 bytes should take about 2 s on air, plus the start of the transmission.

## Test 7: can term73 see Data Mode, and does it stay on?

Set A's menu 984 to USB first, so that the rig commands below can't go on air. If test 2 showed rig commands being transmitted with 984 on USB as well, skip steps 2 and 3.

1. A: `/advanced dv monitor off`. Data Mode off.
2. A: `/advanced watch FO <band>` (0 for band A, 1 for band B). Turn Data Mode on, wait 4 s, turn it off, wait 4 s. Note any `changed:` lines, then run `/advanced watch off`.
3. Do the same with `/advanced watch MD <band>`.
4. With Data Mode on, switch the radio off and on. Is Data Mode still on? Switch the band to FM and back to DV. Is it still on?

## Test 8: My Callsign slots

These commands only read.

1. A: `/advanced cat DS`, then `/advanced cat DC 1` through `/advanced cat DC 6`.
2. Compare the replies with menu 610.

## Afterwards

- Turn Data Mode off on both radios.
- Put the menus back as you noted them, or leave them. Leaving Data Mode on, with 614 at 0.5 s and 984 on term73's link, may put term73's connect commands on air until test 1 is answered.
- `/transmit off`.

## Results to send back

| Test | Record |
|---|---|
| 0 | Did A transmit? A's monitor dump |
| 1 | Did A transmit `#hello`; did B receive it; did anything come back on A's monitor |
| 2 | The reply; did A transmit; did B receive anything |
| 3 | Port name; did A to B work; did B to A show on USB; did Bluetooth still answer `ID` |
| 4 | A's dump for each menu 618 setting |
| 5 | Both `sweep:` lines |
| 6 | For each size: B's `burst:` and `reception:` lines, and A's key-down time |
| 7 | Changed fields; Data Mode after power-off, and after FM and back |
| 8 | `DS` and `DC` replies compared with menu 610 |

Also send the wire logs from tests 1-4 (the `logs` folder), and copy term73's screen text where it helps.
