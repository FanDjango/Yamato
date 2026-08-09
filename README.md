<div align="center">

<img src="assets/yamato-256.png" width="144" alt="Yamato">

# Yamato

**Fan control software for ThinkPads.**
<br>
**Not affiliated with, endorsed by, or supported by Lenovo.**

[![Release](https://img.shields.io/github/v/release/mackid1993/Yamato?style=flat-square&color=ce1b22&label=release)](https://github.com/mackid1993/Yamato/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/mackid1993/Yamato/total?style=flat-square&color=ce1b22)](https://github.com/mackid1993/Yamato/releases)
![Platform](https://img.shields.io/badge/windows-10%201809%2B%20x64-ce1b22?style=flat-square)
![License](https://img.shields.io/badge/license-MIT-ce1b22?style=flat-square)

</div>

> **Yamato writes directly to your ThinkPad's embedded controller.**
> Setting a manual fan level switches the firmware's own thermal management
> off, which is what makes fan control possible and also what makes it worth
> taking seriously. Yamato hands the fan back to the firmware on exit, on
> crash, on shutdown, and when a watchdog notices the control loop has stalled.
> It refuses the disengaged setting outright, and refuses level 0 as a held
> manual mode. Use it on hardware you are willing to look after.

Named after IBM's Yamato Lab, where the ThinkPad and the TrackPoint were
designed.

---

## Why

[TPFanControl][tpfc], [TPFanCtrl2][tpfc2] and [FanDjango's fork][fandjango]
have kept ThinkPads quiet for twenty years. They still work. This is not a
replacement for something broken.

The line runs from the original TPFanControl through [byrnes' dual-fan
mod][byrnes], which is where support for a second fan came from, to
[TPFanCtrl2][tpfc2]. FanDjango's fork carries a good deal of work beyond that
and is the one to look at first. Yamato owes all of them.

They figured out what this job actually needs. Discrete levels, because that's
what the controller speaks. A step at the top that gives the fan back to the
firmware, which reacts faster than any polling loop. Hysteresis, or the fan
hunts. An escape from a held manual level when things get hot. Handing the fan
back when the controller stops answering instead of leaving it stuck. All of
that is here because they showed it was needed.

What's different here is the interface. You drag the curve instead of writing
`Level=50 0 0 3` in an ini file, temperatures are on screen instead of in a
log, and the driver underneath is signed and current rather than `tvicport`,
whose certificate expired in 2007.

No code was copied. Not a line, not an identifier. What is shared is knowledge
of the hardware, which register holds the fan level and what `0x80` means, and
that is a fact about a ThinkPad rather than anyone's code. It's documented in
the Linux [`thinkpad_acpi`][acpi] driver and on ThinkWiki.

Some behavior was learned from watching how they work, and the comments say so
where it happened: how many times to retry a read, how long to wait for the
controller, treating a declined write differently from a dead one. Those were
their answers before they were mine.

Thanks to everyone who worked on both.

## What it does

- A curve you drag. Double-click to add a point, right-click to remove one. The
  current temperature is drawn on the graph so you can see where you sit.
- All twelve sensors on screen. Right-click one to leave it out of the
  decision.
- Profiles. Create, rename, duplicate and delete them from the tray or the
  window.
- Three modes: firmware control, your curve, or a fixed level.
- Hotkeys: Ctrl+Shift+B for firmware, Ctrl+Shift+S for your curve,
  Ctrl+Shift+P for the next profile. Nothing sets a fixed level from a keyboard
  on purpose.
- Celsius or Fahrenheit.
- Click a curve point and two rows show how far it has to cool before the fan
  slows, and how far past the point before it speeds up. The shaded bands on
  the graph are those waits. New points copy their neighbor instead of
  arriving with different numbers to everything around them.
- The poll interval, watchdog, starting mode and the temperature that ends a
  held manual level are all rows you click.
- Sleep is handled without asking you anything. Close the lid on a dock and the
  curve keeps running; put the machine to sleep and the fan goes back to the
  firmware. See below.
- The temperature in the tray icon, or just the dot if you prefer.
- Imports a `TPFanControl.ini` curve, hysteresis and all. Every curve in the
  file, not just the first. Nothing else in it is touched.
- Optional CSV history in `%ProgramData%\Yamato\`, off by default, rotated once
  so it can't grow forever.
- Starts with Windows, no prompt at logon.
- Hands the fan back to the firmware on exit, on crash, on shutdown, on sleep,
  and when a watchdog notices the control loop has stalled.

It won't log anything unless you ask, and it won't offer the unregulated `0x40`
mode. `config.json` is read back without a restart if you'd rather use a text
editor, but you shouldn't need to.

## Sleep

A dark screen doesn't mean much. A laptop docked with the lid shut is working.
So is one whose screen blanked on a timer during a long build. So, from the
outside, is one asleep in a bag, and that last one is the one that matters: a
manual level switches the firmware's thermal management off, and Windows can
refuse port access during standby, so a curve held there means nobody is
managing the fan at all.

There used to be a setting for this, which meant guessing. The lid doesn't
answer it, and neither does the power cord: you can be asleep and plugged in,
then unplug and drop the thing in a bag, and the decision would have been made
at the one moment that can't see what happens next.

Two things answer it instead. Modern Standby doesn't broadcast anything when a
session begins, but it does log it: Kernel-Power writes 506 on entry and 507 on
exit, and that can be subscribed to like any other notification. TPFanControl
does the same, and it's a better signal than measuring, because it arrives as
standby starts rather than once it's over.

Behind that, in case the subscription can't be made, Windows keeps two counts of
the same clock. One runs with real time; the other stops while the machine is in
a low-power state. Subtract them and what's left is exactly how long the machine
was parked. Between them:

- Screen goes off. The fan goes back to the firmware straight away, because at
  that instant nothing is known yet.
- A few passes later, if the machine has been running the whole time, the curve
  comes back. Docked with the lid shut, that's a few seconds.
- Any pass that finds real sleep in it hands the fan back again, and the poll
  slows down so we're not waking a parked CPU to read a temperature that isn't
  moving.

On hardware old enough to still have S3, Windows sends a suspend broadcast
before the power goes, and the fan is back with the firmware before the machine
goes down. Modern Standby sends nothing at all, which is what the clocks are
for. Hibernating sends it on every machine.

One case the clocks can't answer: undock a laptop whose lid has been shut the
whole time, and Windows lights the internal panel behind the closed lid. One
display, console display on, no standby session, no notification. The machine
sits there fully awake with its screen facing the keyboard, and every
measurement agrees it's working, because it is.

So the lid switch answers that one, because it's hardware and doesn't have an
opinion. **Lid closed and running on battery counts as a dark screen**, whatever
the display state claims, and goes through the same three steps above. The fan
goes to the firmware while nothing is known, comes back to the curve if the
machine turns out to still be running, and goes back to the firmware for good
once the clocks find real sleep.

That last part matters, because a closed laptop on battery stays awake until
Windows' own idle timers expire, and those are often half an hour. Holding it on
the firmware that whole time would be trading real noise for no safety: while
the machine is awake the controller answers, so the curve is doing its job. What
isn't safe is a level held once the controller stops answering, and that is what
the clocks catch.

Lid closed on wall power is a dock, and behaves as it always did.

## Settings

Every row in the window cycles through a short list on a click. Hovering one
puts a sentence about it under the list, so you shouldn't need this table, but
here it is anyway.

| Row | Default | Range | What it does |
| --- | --- | --- | --- |
| **Poll** | 5 s | 1–60 s | How often the sensors are read and the fan level decided again. Lower follows the temperature more closely and wakes the machine more often. |
| **Watchdog** | 30 s | 30–180 s | If no decision happens for this long, the fan goes back to the firmware. A fixed level with nothing updating it is the state worth escaping, and this is the escape. Held to at least three times the slowest poll so it can't fire during normal running. |
| **Manual escape** | 80 °C | 60–90 °C | A fixed level is held whatever the temperature does, with the firmware's own management off. This is the temperature that ends one. There's no off switch, and the range is narrow on purpose: below 60 it would fire during ordinary work, above 90 it stops meaning anything. |
| **Standby poll** | 30 s | 5–120 s | The poll interval, but only once the machine is believed asleep. Separate because waking a parked CPU every few seconds to read a temperature that isn't moving is how you ruin standby battery. Not used while the screen is merely off — that's where the working-or-asleep decision gets made, and it should be quick. |
| **Start in** | Smart | BIOS / Smart | Which mode the engine begins in when it starts. Nothing here touches the fan now; it's what happens at the next boot. |
| **Logging** | Off | On / Off | Writes what was read and what was asked for to `%ProgramData%\Yamato\`, as CSV. Rotated once at the size below so it can't fill a disk. Off unless you ask, because a program resident all day shouldn't write to disk every few seconds. |
| **Open at start** | Tray only | Window / Tray only | Whether this window opens at launch or Yamato goes straight to the tray. |
| **Fans** | Dual | Single / Dual | Whether this machine has a second fan. Some single-fan ThinkPads answer the second selector with a value that doesn't track what was written, so every write looks declined and every handback looks failed. Set Single and that selector is never touched. |

Click a curve point and two more rows appear for that point alone:

| Row | Default | Max | What it does |
| --- | --- | --- | --- |
| **Slows down** | 4° cooler | 15° | How far below the point the machine has to cool before the fan steps down. Larger holds the fan higher for longer: louder, but never hot, so it has room. |
| **Speeds up** | 0° hotter | 5° | How far above the point before the fan steps up. Larger delays the fan on a machine that's already heating, which is the dangerous direction, so it gets a few degrees and no more. |

Both are what stop the fan hunting between two levels at a threshold. The
shaded bands on the graph are these waits drawn out.

Three more live in the tray menu rather than here, because they're about the
tray: **Fahrenheit**, **Temperature in the tray icon**, and **Tray shows**,
which picks the sensor the icon reports. That last one is display only — the
fan is always driven by the hottest sensor, since a curve following one
nominated sensor would sit still while another part of the machine cooked.

The one setting with no control anywhere is how large the history file may grow
before it's rotated, which is 8 MB. All of it is in `config.json` beside the
log, re-read without a restart if you'd rather use a text editor.

## Requirements

- Windows 10 1809 or later, 64-bit. Windows 11 included.
- A ThinkPad whose embedded controller answers on the usual ACPI ports
- [PawnIO][pawnio], installed separately

On models: Yamato talks to the embedded controller at ports `0x62` and `0x66`,
which is where the ACPI specification puts it and where ThinkPads have had it
for a very long time. Confirmed working on a P1 Gen 7. TPFanControl's own
testing covers T430 and X230T on those same ports.

That is the whole of what is known. TPFanCtrl2 carries a second set of
addresses, `0x1600`/`0x1604`, found on a P53, and probes that pair first, so on
any machine where it answers the standard ports were never tried. Whether those
models also respond at `0x62`/`0x66` is simply untested, and no year or product
line predicts it: a P53 and a P1 Gen 7 are five years apart in the same range.

Trying it costs nothing. The PawnIO module refuses any port other than those
two, so a machine that keeps its controller elsewhere gets no reading and a
tray icon saying the controller cannot be reached. There is no address it could
write to by mistake.

The hard floor is 1703. That's when `SetProcessDpiAwarenessContext` appeared,
and it's linked statically, so anything older won't start at all rather than
starting badly. Between 1703 and 1809 it should run, but the title bar will be
light against a dark window because the dark-mode attribute isn't there yet.
Rounded corners are Windows 11 only and are skipped quietly everywhere else.
Neither is worth caring about, so 1809 is the number to go by.

Yamato does not bundle PawnIO. It's GPL-2.0, and shipping the driver would
mean shipping its source; pointing at the download doesn't. The installer
checks for it and offers to open the page, and the tray keeps a link to it.

## How it is put together

Two processes, one controller.

The engine runs as a Windows service under `SYSTEM`, so the fan is controlled
from boot, before anyone logs in. It's the only thing that opens the port
driver.

The window attaches to it. It never opens the driver at all, so it can't write
the fan register no matter what it's asked to do. That's a property of the
code, not a promise. It reads what the engine publishes into a shared section,
and mode changes go back the same way.

Two `yamato.exe` in Task Manager is expected. A service can't show a tray icon
and a tray icon can't control hardware before logon, so one process can't do
both.

The service isn't optional. Running `yamato.exe` on its own gives you a tray
icon with nothing behind it. There's deliberately no path for the tray to drive
the fan itself: a second way in would be a second thing that has to get the
handback right on every exit, and getting that wrong leaves a fan stuck with
the firmware switched off.

Two programs writing `0x2f` at once is the thing worth avoiding. A manual level
takes the firmware out of the loop, so two controllers arguing means a fan
thrashing with nothing underneath. Whoever holds the engine lock is the engine,
and nothing else has a handle that can write.

## Fan levels

Same model the ThinkPad EC has always used.

| Level | Meaning |
| ----- | ------- |
| 1 - 7 | Increasing speed |
| BIOS  | Hand the fan back to the firmware |

The firmware step is worth having at the top of a curve. During a boot or a
sudden load spike the firmware reacts faster than any polling loop can, and it
is better at that job than we are.

The controller has two levels beyond those, and Yamato treats them very
differently from one another.

Level `0` stops the fan. A curve may use it, and the default one does: below
46 °C the fan is off, which is most of why the machine is silent on a desk. In
a curve that is safe because the curve is watching. The point is bound to a
temperature, and the step above it takes over as the machine warms. What is
refused is level 0 as a *manual* mode, over the channel the window talks to the
engine on: a manual level is held indefinitely no matter what the temperature
does, and a fan held off indefinitely with the firmware switched out of the
loop is a different thing entirely. That floor is in the engine, at the point
where a command arrives.

`0x40` runs the blower unregulated. It is documented as potentially unsupported
and damaging, it is extremely loud, and it does not cool better than level 7 in
practice. That one is refused everywhere a level can enter: curve validation,
config loading, the editor's axis, and the channel. No curve, no saved file
and no message can produce it.

## Building

```
powershell -ExecutionPolicy Bypass -File bootstrap.ps1   # check and install what is needed
build.cmd                                                # release build, staged into dist\
build.cmd test                                           # run the tests
build.cmd installer                                      # plus the Inno Setup installer
build.cmd --ver patch                                    # bump the version, then build
```

A release build runs the tests first and refuses to build if they fail. For a
program that drives cooling hardware that seemed a better trade than a faster
build.

## Layout

| Crate | What it is |
| ----- | ---------- |
| `yamato-ec` | Reaching the embedded controller: the PawnIO IOCTL client and the ACPI EC protocol |
| `yamato-core` | Curves, profiles, config, and the control loop |
| `yamato` | The service, the tray icon, and the window |

## License

MIT. See [LICENSE](LICENSE).

`LpcACPIEC.bin` is a PawnIO module by namazso, LGPL-2.1-or-later, shipped
unmodified. See [NOTICE.md](NOTICE.md).

[tpfc]: https://github.com/ThinkPad-Forum/TPFanControl
[tpfc2]: https://github.com/Shuzhengz/TPFanCtrl2
[fandjango]: https://github.com/FanDjango/TPFanCtrl2
[pawnio]: https://pawnio.eu
[acpi]: https://www.kernel.org/doc/html/latest/admin-guide/laptops/thinkpad-acpi.html
[byrnes]: https://github.com/byrnes/TPFanControl
