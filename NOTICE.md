# Third party components

Yamato is MIT, see `LICENSE`. It ships alongside two components that are
not ours, listed here.

## PawnIO modules: LpcACPIEC.bin and LpcIO.bin

Two PawnIO modules. `LpcACPIEC.bin` permits access to the ACPI embedded
controller ports 0x62 and 0x66, which is where most ThinkPads keep their fan
control. `LpcIO.bin` discovers LPC base address windows at runtime, which is
how Yamato reaches the EC on the machines that keep it at 0x1600/0x1604
instead. Both are installed on every machine; Yamato probes at startup for
which one this machine needs.

- Part of PawnIO Modules, Copyright (C) 2023 namazso <admin@namazso.eu>
- Licensed under the GNU Lesser General Public License, version 2.1 or later
- `SPDX-License-Identifier: LGPL-2.1-or-later`
- Full license text: `LICENSE.LGPL-2.1.txt`
- Sources: `LpcACPIEC.p` and `LpcIO.p`, shipped alongside them and installed
  with them
- Source: <https://github.com/namazso/PawnIO.Modules> (`LpcACPIEC.p`,
  `LpcIO.p`)

The files shipped here are upstream's signed release, byte for byte
unmodified. Their signatures are what the PawnIO driver checks before loading
them, so they cannot be altered without breaking them.

They are separate files loaded at runtime by a driver, not linked into Yamato
in any form. Replacing either with your own build is a matter of overwriting
the file.

## PawnIO

**Not redistributed.** The driver is GPL-2.0-or-later and Yamato does not ship
it. Install it yourself from <https://pawnio.eu>; Yamato offers a button that
takes you there.

Yamato talks to PawnIO only over `DeviceIoControl`. PawnIO's license grants an
explicit exception for exactly that:

> ...free software programs or libraries that are released under the GNU LGPL
> and with independent modules that communicate with PawnIO solely through the
> device IO control interface.

## Rust crates linked into yamato.exe

All permissive, and all compatible with MIT. Where a crate offers a choice,
Yamato takes the MIT arm, which avoids the Apache-2.0 question entirely.

| License | Crates |
| ------- | ------ |
| MIT or Apache-2.0 | equivalent, hashbrown, indexmap, itoa, proc-macro2, quote, serde, serde_core, serde_derive, serde_json, serde_spanned, syn, toml, toml_datetime, toml_parser, toml_writer, version_check, windows, windows-core, windows-implement, windows-interface, windows-result, windows-strings, windows-sys, windows-targets, windows_aarch64_gnullvm, windows_aarch64_msvc, windows_i686_gnu, windows_i686_gnullvm, windows_i686_msvc, windows_x86_64_gnu, windows_x86_64_gnullvm, windows_x86_64_msvc |
| MIT | winnow, winresource, zmij |
| MIT or Unlicense | memchr |
| MIT or Apache-2.0, and Unicode-3.0 | unicode-ident |

Copyright in each remains with its authors. Their license texts and copyright
notices are reproduced in full in `THIRD-PARTY-LICENSES.txt`, installed
alongside the program. The Rust standard library is MIT or Apache-2.0, copyright The Rust
Project Developers.

## On the prior art

Yamato is a fresh implementation and contains no code from TPFanControl,
TPFanCtrl2, or FanDjango's fork of it. No source was copied: no functions, no
identifiers, no comments.

  TPFanControl   https://github.com/ThinkPad-Forum/TPFanControl
  dual-fan mod   https://github.com/byrnes/TPFanControl
  TPFanCtrl2     https://github.com/Shuzhengz/TPFanCtrl2
  FanDjango      https://github.com/FanDjango/TPFanCtrl2

Some of its behavior was arrived at by studying how they behave, and the source
comments name the reference where that happened. None of them would have forbidden it either: the original
TPFanControl is placed in the public domain by a dedication in its own source
headers, and TPFanCtrl2 is under the Unlicense. The distinction is drawn for
accuracy, not obligation.

What it shares with them is knowledge of the hardware: which EC register holds
the fan level, what 0x80 and 0x40 mean, where the temperature sensors live.
Those are facts about a ThinkPad, independently documented in the Linux
`thinkpad_acpi` driver and on ThinkWiki, and facts are not copyrightable.
