# ED Compass

![Status](https://img.shields.io/badge/STATUS-PRE--RELEASE%20WORK%20IN%20PROGRESS-critical?style=for-the-badge)

> [!CAUTION]
> ## ⚠️ PRE-RELEASE — WORK IN PROGRESS
>
> **THIS IS NOT READY FOR PUBLIC DISTRIBUTION. PLEASE DO NOT SHARE OR
> REDISTRIBUTE IT YET.**
>
> This repository is published for development and testing only. Detector
> thresholds, file formats and the interface are all still changing, results are
> not yet dependable, and there are no published releases to install. Anything it
> reports should be treated as provisional.

**Elite Dangerous hides signals in its audio. This listens for them while you fly.**

There is something in the black that transmits. The
[Landscape Signal](https://canonn.science/codex/cartographics/the-landscape-signal/)
is a picture drawn in sound — mountains, a horizon, a repeating pattern — audible
from anywhere in the galaxy, discovered by commanders who happened to look at a
spectrogram. Nobody knows how many others there are.

ED Compass watches for them so you don't have to. Three lamps in your cockpit,
lit when something is out there.

<img src="docs/images/ed-compass.png" alt="The ED Compass analysis window: a live spectrogram of Elite's audio, with the direction compass, periodicity meter and detection log below it" width="820">

<sub>The full view — everything the tool heard in the last few minutes. While
flying you'd normally use the cockpit overlay instead.</sub>

<!-- More screenshots -->

---

## Install

> [!NOTE]
> **There is no release to download yet.** The section below describes how
> installing *will* work once the first build is published. Until then the only
> way to run it is to [build from source](docs/reference.md#building-from-source).

**[⬇ Download the latest release](../../releases/latest)** — run
`ED-Compass-Setup.exe` and you're done. No administrator rights, nothing else to
install.

Windows will say *"unknown publisher"*. Click **More info → Run anyway**. That
warning appears for every small free tool that hasn't paid for a signing
certificate.

Then start Elite. The overlay appears by itself whenever the game is in front of
you, and gets out of the way when it isn't.

> **Elite must run in borderless mode**, not exclusive fullscreen — no overlay of
> any kind can draw over exclusive fullscreen.

## What you'll see

Three indicators, in the order you should trust them:

| | |
|---|---|
| **LANDSCAPE** | The Landscape Signal itself — its 109.5-second cycle, measured. This one is checked against a known recording, so it means what it says. |
| **TRANSMIT** | Something is keying tones on and off, the way a transmission does. |
| **STRUCTURE** | The spectrogram has thin diagonal strokes in it — something *drawn* rather than noise. |

Beside them, a live spectrogram of what the game is playing. When something
fires, the audio is saved automatically, tagged with the star system and
coordinates you were at.

**TRANSMIT and STRUCTURE also light on ordinary ship ambience.** They are hints,
not verdicts. LANDSCAPE is the one that has been checked.

## Is this allowed?

Yes. It listens to sound your speakers are already playing and reads the journal
files the game writes for exactly this purpose — the same things EDDiscovery,
EDMC and every other companion tool use. It never touches the game process, its
memory, or its files, and it gives you no advantage over any other commander. It
only notices things you could have noticed yourself with headphones and patience.

## Does it actually work?

Yes, and you don't have to take our word for it. Given CMDR Serbanstein's
published recording of the real Landscape Signal — one exact cycle, 109.63
seconds — ED Compass measures **109.67 seconds at 0.98 confidence**, with no
template and nothing to match against. It found the period from the audio alone.

It costs about **a quarter of one percent of a CPU core** and 42 MB of memory, so
you can leave it running.

## Finding something

If you catch a signal nobody has catalogued, that's a find worth sharing.

1. Note the system and where you were pointing.
2. Export the spectrogram (**Export PNG** in the full view).
3. Take it to the [Canonn Research Group](https://canonn.science/) — they are the
   people who found the Landscape Signal in the first place.

Every detection keeps a small JSON record with the system, coordinates, scores
and period — kept forever, even after the audio itself is cleaned up, so your
observations accumulate into something you can triangulate from.

## Questions

**Will it fill my disk?** No. Recordings are FLAC and capped (about 2 GB by
default, roughly 350 captures). When it's full the *weakest* detections are
cleaned up first, never the best ones, and the written record of every
observation is kept regardless. There's a usage bar and a **clean up** button in
the control panel.

**Do I need 7.1 surround?** No. Direction finding is an optional extra that needs
it; detection works fine in stereo, which is what almost everyone should use.

**Does it make any sound?** No, deliberately — an audio alert would be picked up
by its own microphone-equivalent and detected as a signal.

**Can I just unzip it?** Yes, there's a portable zip on the releases page.
Settings and recordings stay in that folder.

## More

- **[Technical reference](docs/reference.md)** — how it works, how it was
  validated, every setting, and what it deliberately does not do.
- **[The Landscape Signal](https://canonn.science/codex/cartographics/the-landscape-signal/)** —
  Canonn's write-up of the thing this was built to find.
- Bugs and ideas: [open an issue](../../issues).

## Credits

The research is the [Canonn Research Group](https://canonn.science/)'s. The
Landscape Signal was found by CMDR PublicStaticVoid in 2019 and triangulated by
CMDR Seventh_Circle; the reference recording that this tool was validated
against is CMDR Serbanstein's. None of their material is redistributed here.

MIT licensed — see [LICENSE](LICENSE). Elite Dangerous is a trademark of Frontier
Developments plc; this is an unofficial tool, not affiliated with or endorsed by
Frontier.

o7

[![Licence](https://img.shields.io/github/license/tbma2014us/ed-compass?style=for-the-badge)](./LICENSE)
[![Rust](https://img.shields.io/badge/Rust-%23000000.svg?style=for-the-badge&logo=rust&logoColor=white)](#)