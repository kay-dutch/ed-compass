# ED Compass — technical reference

Everything about how the tool works, how it was validated, and every setting it
has. For "what is this and how do I install it", see the
[README](../README.md).

It listens to what the game is playing through WASAPI loopback and answers two
questions continuously: **is something transmitting**, and **is there a picture
drawn in the spectrogram**. When either fires it lights an indicator in the
cockpit and keeps the raw audio — tagged with the star system and coordinates
read from the game's own journal.

It makes no sound. An audible alert was tried and removed: it played through the
same endpoint being captured, so the tool fed its own chirp back into its own
detector.

It costs about a quarter of one percent of a CPU core and 42 MB.

It never touches the game's audio. It is an observer.

## Install

See the [README](../README.md#install). This document assumes it is already
installed, or that you are building it yourself.

## Validated against the real signal

CMDR Serbanstein's high-fidelity recording of the Landscape Signal, linked from
[Canonn's codex entry](https://canonn.science/codex/cartographics/the-landscape-signal/),
is exactly one cycle long. Run it through the tool and the period it reports
agrees with the 109.5 seconds the community documented, having had no template
for it and never having seen it:

```sh
ed-compass --headless --input landscape_reference.flac --loop --duration 40
```

Looping is required: a single cycle cannot evidence its own period, because
autocorrelation needs the lag to appear at least twice.

### Where to look, and why the defaults are what they are

The signal's strokes live in the low hundreds of hertz to a couple of kilohertz,
which is why `spectrogram_min_hz` and `spectrogram_max_hz` default to 200 and
2400 rather than to the full audible range. Showing 20 Hz to 22 kHz leaves most
of the image blank and shrinks the strokes to near-invisibility.

Cropping the band magnifies frequency and so steepens every stroke, which would
make images incomparable with the community's. `export_match_published_aspect =
true` scales the export height to cancel that exactly, so exports stay comparable
whatever band you are viewing.

## Reading a result

Three indicators, in descending order of how much they are worth trusting.

**`SIGNAL`** covers every recognised signal, and the detail line names which.
Two are recognised so far.

**Keyed transmission (on/off)** — one tone switched on and off deliberately.
Thargoid Sensor Morse is the signal it was built from, but the scoring does
**not** require a Morse-like 3:1 ratio: a detector that insists on 3:1 can only
confirm what is already catalogued. Any tone keyed into two well-populated,
resolvable lengths lights the lamp, and the ratio is reported as evidence rather
than demanded as a condition.

It needs its own band (`morse_min_hz`/`morse_max_hz`, 60–200 Hz), because the
low-frequency floor that stops ship rumble triggering the other detectors is
exactly what would hide this one. Detection only: turning marks into letters, and
letters into the coordinate pairs that draw a picture, is a separate job.

**The Landscape Signal** — the period matches 109.5 s within 2 s, at confidence ≥ 0.80 and
prominence ≥ 0.50. This is the only indicator that reliably separates the real
signal from ordinary ship ambience, and it is the one to act on. It needs about
four minutes of history before it means anything.

**`transmission`** — binary keying. Fast, and the only detector that can catch a
one-shot signal such as a Thargoid probe, which transmits once and never repeats.

**`structure`** — drawn structure in the spectrogram. It looks for ink that forms
long, thin, connected strokes that *turn*, and separately for faint lines
recovered by integrating along them.

> **Unproven, and currently unreliable.** Measured against a real in-game capture
> of the Landscape Signal — as opposed to a synthetic one — this does not
> separate the signal from ordinary ship ambience, which scores as highly or
> higher. Do not act on this lamp. The period reading under SIGNAL is the
> trustworthy indicator.

The period remains the most trustworthy discriminator of the three. Ship ambience
readily produces keying scores and periods scattered across a wide range, while
the genuine recording returns a stable 109.5 s at high confidence.

### Noise removal

Steady noise is the enemy of a faint signal, and there are two independent tools:

- `spectrogram_median_subtract` (default on) removes each frequency row's median
  across the visible window. Anything constantly present — rumble, life support,
  a drone — vanishes; a stroke crossing that row barely moves its median.
- `spectrogram_show_excess` uses the detector's learned background instead. That
  only knows the past and adapts over ~60 s, so median subtraction is the better
  choice for a rendered image, which can see the whole window at once.

## What it detects

Two primary detectors, plus direction finding as a secondary.

**Binary keying** — "something is transmitting". The Thargoid Probe tightbeam is
a data stream: triplets of high and low tones on a symbol clock. The detector
looks for energy parking on a **small alphabet of discrete tones**, **alternating**
between them, with **dwell times clustered on a period**. It contains no template:
it reports whatever tones and symbol rate it finds. This is the only detector that
can catch a one-shot signal, and **a Thargoid probe transmits once** — it is not
periodic, so nothing else would see it.

**Drawn structure** — "there is a picture here". Line art in a spectrogram is
simultaneously locally oriented, sparse, directionally diverse, and — the property
that actually separates it from ship ambience — made of strokes with *extent*.
Sustained tones and broadband transients are removed before anything else looks,
because those two things are what ambience is made of.

### What it will miss

- **Amplitude keying** (one tone switched on and off, Morse-style). The keying
  detector assumes frequency-shift keying; if the peak bin never moves, it scores
  zero. Not covered.
- **Symbols shorter than two STFT frames** (~85 ms at defaults). Reduce `hop`.
- **Anything below the novelty threshold** — both detectors read bins standing
  above the learned background, which is why the field procedure says disable
  thrusters.
- **Drawings smaller than ~16×16 cells**, or spanning more than the scan window.

When both fire at once, believe the keying number: it is the more specific claim.

## What it is looking for

The reference case is the **Landscape Signal** — an ambient signal in Elite
Dangerous, found in 2019 by CMDR PublicStaticVoid, audible from anywhere in the
galaxy when the ship points toward Sagittarius A*. Its volume does not fall off
with distance, it stays audible up to 75° off boresight, and it repeats every
**109.5 seconds**. Its name comes from what it looks like: plotted as time
against frequency, it draws a wireframe mountain range.

See [Canonn's codex entry](https://canonn.science/codex/cartographics/the-landscape-signal/)
for the full account, including CMDR Seventh_Circle's triangulation of the
apparent source to empty space about 12 ly from Sgr A*.

**This is not steganography in the usual sense.** No payload is hidden in the
sample bits — the picture lives in the time-frequency plane, the same family as
SSTV or spectrogram art. That decides the whole design: the STFT is the
instrument, and the amplitude histogram most audio tools lead with is blind to
every phenomenon here. It survives in this tool only as a signal-health readout.

The detector is generic, not a template match. It learns what the background
sounds like and flags departures from it, so it can surface signals nobody has
catalogued. The 109.5 s period is checked separately, and a match is labelled as
such rather than being the thing that triggers detection.

## Windows

One main window, plus an overlay that comes and goes by itself — the same model
SrvSurvey uses, and for the same reason: there is never a state you have to kill
the process to leave.

The main window holds everything: the waterfall, compass, periodicity and event
panels, and the arming controls (listening, detectors, overlay, disk usage).
There used to be a separate small "compact" panel; the overlay made it
redundant and it was removed. `--compact` and `--view` are still accepted, and
ignored, so old shortcuts launch.

The overlay is a second window that is created once and never destroyed. When it
should not be seen its **whole-window opacity is set to zero** with
`SetLayeredWindowAttributes` — the same thing SrvSurvey does, whose plotters set
`Form.Opacity = 0` rather than hiding or closing. The window keeps its position
and keeps rendering; it simply composites to nothing.

Two properties of the window make that safe. It already carries
`WS_EX_LAYERED`, because winit sets that alongside `WS_EX_TRANSPARENT` for a
click-through window, so no style has to be changed. And winit implements
transparency with `DwmEnableBlurBehindWindow` rather than `UpdateLayeredWindow`,
so a constant alpha composes with the per-pixel alpha instead of replacing it.

Until that call succeeds — on the first frame the window does not exist yet —
the overlay is parked off-screen at (32000, 32000) instead, so there is never a
frame where it is wanted-hidden and visible.

Hiding is done this way rather than by destroying or un-showing the window,
because in egui 0.35 both of those are broken: destroying a viewport can wipe the
*main* window's GPU surface, and a window hidden with `with_visible(false)` stops
receiving redraws for good. The overlay is click-through, never activates, and
holds no taskbar entry.

By default it is **fitted into the gap SrvSurvey's top plotters leave free**,
sized from the game window each time rather than from a fixed width. SrvSurvey
anchors its top-edge overlays as `left:8` (PlotBodyInfo is the widest at 320,
reaching x=328) and `center:0` (PlotJumpInfo is the widest at 600, spanning
centre ± 300), so the free band runs from 328 to `centre - 300`, less 8 px of
clearance at each end:

| game width | band | overlay |
|---|---|---|
| 2560 | 336 → 972 | 636 px wide |
| 1920 | 336 → 652 | 316 px wide |
| 1280 | none | falls back to `overlay_width` |

Set `overlay_fit_between_plotters = false` to use `overlay_width` and
`overlay_x_offset_px` directly instead. Without SrvSurvey installed the setting
is harmless — that band is empty screen either way.

It is shown only when Elite is the foreground window. A **minimized** Elite
counts as no game window at all, the same check SrvSurvey makes, because Windows
still reports a rectangle for it and the overlay would otherwise appear over the
desktop.

An earlier version also showed it while the control window had focus, so the
toggles could be seen to act. That made it appear at every launch — the control
window always has focus the moment it opens — and then vanish a second later.
The overlay belongs to the cockpit; if you are not looking at the cockpit there
is nothing for it to annotate.

Untick **In-game overlay** in the main window to stop it appearing at all.

### Desktop shortcuts

```sh
ed-compass.exe --install-shortcut
```

Creates **ED Compass** on your Desktop. (`--overlay` is still accepted and
ignored, so a shortcut left over from an earlier version still launches.)

### The in-game overlay

Three indicator lamps — `SIGNAL`, `TRANSMIT` and `STRUCTURE` — in a column down
the left, with the live spectrogram filling the rest of the panel at its full
height — the lamp column is measured from the text it draws rather than
configured, so no width is ever left unused.

The lit colours carry the trust order: **`SIGNAL` is green**, because the period
is the one measurement checked against a known recording, and **`TRANSMIT` and
`STRUCTURE` are blue**, because both also light on ordinary ship ambience and
are hints rather than findings. A suspect transmission overrides to amber. If
all three shared a colour, green would come to mean "maybe". The panel's border turns amber while the background model is still
learning and red if the audio endpoint is lost — so dark lamps can never be
mistaken for a tool that has quietly stopped listening. With direction finding
enabled, a small bearing rose sits between them —
up is the ship's nose, needle length is confidence, and a stereo mix's
front/back-ambiguous bearing shows a dimmer mirrored ghost. A bearing within 3°
of the nose is drawn **red** instead of green: balanced ambience pans dead
centre, so a centred needle is the null result, and keeping green for genuine
off-axis bearings is what stops the instrument being ignored.

Two bearings are computed, and they mean different things. A **detection**
records the bearing of the material that triggered it — accumulated only over
bins above `novelty_threshold_db`, so it describes the signal and not the room.
The **compass and the overlay's rose** show a separate live bearing, taken over
the whole detection band every frame and smoothed over about a second. Smoothing
is applied to the powers rather than the angle, because bearings wrap at ±180°
and averaging across the wrap point produces a needle pointing at nothing.

The live one exists because the display used to read the detection accumulator,
which only updates while an event is open. Nothing clears that threshold in
ordinary listening, so the needle sat dead — indistinguishable from a broken
instrument.

Measured cost, from `cargo run --release --example throughput`:

| endpoint | direction finding | CPU | memory |
|---|---|---|---|
| stereo | off | 0.22% of one core | 34.8 MB |
| stereo | on | 0.26% of one core | 62.2 MB |
| 7.1 | on | 0.45% of one core | 227.0 MB |

The cost is memory, not CPU — the PCM ring holds every channel instead of a
mono mixdown. Lower `pcm_ring_seconds` if 220 MB matters on a 7.1 endpoint;
it must still exceed one 109.5 s cycle for periodicity to work. It sits **flush in the game window's top-left corner**, the one large area
Elite's own HUD leaves empty, so it costs no cockpit visibility.

The palette is sampled from a cockpit screenshot rather than guessed: HUD orange
`#D16E00` on black for the labels, Elite's contact cyan `#CBF9FB` for a lit lamp,
so it reads as part of the interface instead of as a window sitting on top of it.
It is click-through, so it can never steal a click meant for the cockpit, and it
dims to near-invisible when there is nothing to report.

It works the same way every Elite overlay does — SrvSurvey, EDMCOverlay and the
rest: a borderless always-on-top window positioned over the game's window. Nothing
is injected, no graphics API is hooked, and the game process is never opened.

> **Elite must run in BORDERLESS mode.** An exclusive-fullscreen application owns
> the display outright and nothing can be drawn above it. That is a property of
> the technique, not of this tool.

> **Turn the in-game music off** — Elite's Audio settings.
>
> This matters more than any threshold in this file. Loopback capture hears
> whatever the endpoint is playing, and it cannot tell the soundtrack from the
> void: a scored cue is broadband, sustained, and full of held notes and
> glissandi, which is to say it is indistinguishable from a held tone and a
> frequency sweep by construction. The suppression pass removes what it can, but
> a detector cannot recover a faint signal buried under music that is thirty
> decibels louder than it.
>
> Ship, effects and voice audio are fine to leave on. They are impulsive and
> short, which is the one thing every detector here is built to discard.

Reposition it in `config.toml`:

```toml
overlay_x_fraction = 0.0     # horizontal centre as a fraction of window width;
                             # clamped inside, so 0.0 pins it to the left edge
overlay_y_fraction = 0.0     # top edge, as a fraction of window height
overlay_width = 440.0
overlay_height = 104.0       # the whole panel, spectrogram included
```

Your position and size survive upgrades. If the layout itself is ever redesigned,
`overlay_layout_revision` is bumped and the geometry — and only the geometry — is
reset once, so the window is never left sized for an arrangement that no longer
exists.

## Quick start

```sh
# Build
cargo build --release

# See what endpoints exist
./target/release/ed-compass --list-devices

# Run against system audio (the default output endpoint, in loopback)
./target/release/ed-compass

# Prove the whole chain works with no game and no audio hardware
./target/release/ed-compass --headless --test-landscape --azimuth -55 --duration 340
```

The first run writes `config.toml` next to the executable with all defaults.

## Choosing what to listen to

Endpoints are listed in one flat list. Output endpoints are tagged
`[LOOPBACK]` — those are the ones that hear the game.

```
DEVICE                                    ID
Speakers (Realtek) [LOOPBACK] (default)   {0.0.0.00000000}.{...}
HDMI Output [LOOPBACK]                    {0.0.0.00000000}.{...}
Microphone (default)                      {0.0.1.00000000}.{...}
```

- **System audio** — pick a `[LOOPBACK]` entry, or leave `device` empty in the
  config and the default output is used. This is what you want for Elite.
- **Microphone** — pick a capture entry. Useful for testing, not for the hunt.

Switching devices in the UI restarts capture and persists the choice back to
`config.toml`. Nothing else in the config is written back.

## Direction finding (secondary, on by default)

This is a nice-to-have, not the point, and it is the most expensive thing in the
application — so it ships disabled. Turn it on with `direction_finding = true`.

Direction finding works by comparing levels between channels, and two channels
can only place a sound on a front arc; it cannot tell ahead from astern at all.
**Eight channels can.**

### Set your output to 7.1

You do not need surround hardware. Elite Dangerous renders into whatever the
Windows *endpoint* declares, and loopback captures every channel it renders. So
a stereo headset on an endpoint configured for 7.1 gives you an eight-channel
capture with real directional content.

1. Right-click the speaker icon → **Sound settings**
2. **More sound settings** → **Playback** tab
3. Select your output device → **Configure**
4. Choose **7.1 Surround** → Next through the tests → Finish
5. In Elite Dangerous, set audio output to match

The header shows how many directional channels you actually got. If it says
fewer than three, you are on stereo and the compass will say `front/back
ambiguous`.

### In-game procedure

This is the Independent Raxxla Hunters' documented method, not guesswork. Follow
it exactly — the signal is faint and every one of these steps matters.

1. **Exit supercruise into normal space.** Not supercruise — normal space.
2. **Come to a complete stop** in deep space.
3. **Modules → Disable Thrusters.** Thruster noise is the loudest thing in the
   cockpit and it sits right on top of the signal.
4. **Options → Audio:**
   - Sound Effects: **maximum**
   - Music: **muted**
   - Voice: **muted**
   - Audio mode: **Full Range** (not the compressed/night setting)
5. **Aim at the point you want to test** — centre the target dot on screen.
6. For a pristine recording, take a screenshot of your aim point (F10), then use
   the **Free Camera** (Ctrl+Alt+Space, then Num 0) and fly it in front of the
   ship, lining up against the screenshot. This removes the targeting reticle
   from the shot without losing your aim.
7. Record for **6 minutes** to judge signal strength, **20 minutes** for proper
   analysis. Note the system you are in and what you were facing.

Detection works to roughly **72° off boresight in Audacity** and **75° in Sonic
Visualiser** — the cone is a property of the analysis software's visual
threshold as much as the game, which is why the numbers differ.

## Reading the display

```
ED Compass                                    ● CAPTURING
Device:  Speakers (Realtek) [LOOPBACK]    48000 Hz · 8 ch (7.1) · F32
System:  Stuemeae JM-W c1-5825  [0.0, 0.0, 25899.0]
```

**Status** is one of: `STARTING`, `LEARNING BACKGROUND` (detection suppressed
until the background model settles — about a minute by default), `CAPTURING`,
`NO SIGNAL` (essentially silence), `ANOMALY` (something fired recently), or
`DEVICE LOST`.

### The waterfall

Time runs left to right, ending at *now*. Frequency is **logarithmic**, because
that is where the structure is — a linear axis would spend three quarters of its
height above 6 kHz where there is nothing to see. Brighter is louder.

Detected events are outlined: **yellow** for detected, **green** for detected and
written to disk.

**Drag vertically on the waterfall to mute a frequency band.** Muted bands are
excluded from detection and listed under the channel meters.

### The compass

Bearing in the **ship's frame**: 0° is the nose, positive to starboard. Needle
length carries confidence, so a weak estimate looks weak.

Be clear about what this does and does not tell you:

- It is a **ship-relative** bearing. Elite Dangerous does not expose ship
  attitude in supercruise through the journal or `Status.json`, so there is no
  way to convert it to a galactic vector automatically.
- The Landscape Signal's own directionality is a **heading gain** — a 75° cone
  around where your nose points — not only stereo panning. Getting a galactic
  bearing still means rotating the ship and peaking the signal.

What the tool does is make that sweep **quantitative** instead of by ear: watch
the band level and the bearing while you turn, peak it precisely, and every
detection gets stamped with the system and `StarPos`. That is the same method
CMDR Seventh_Circle used to reach a 0.075 ly sphere, just instrumented.

On stereo the compass also draws a faint mirror bearing and labels itself
ambiguous, because a source ahead and the same source astern are genuinely
indistinguishable with two channels.

### The periodicity panel

Autocorrelation of the long-term spectral summary, over lags from 30 s to 600 s.
The 109.5 s marker is always drawn, so a near miss reads as a near miss. A peak
close to it with decent confidence and prominence is labelled *consistent with
the Landscape Signal*.

This needs at least two full cycles of history — about four minutes — before it
says anything.

## Captures

Captured audio is **FLAC**, which on real captures from this tool measured 76%
smaller than the 32-bit float WAV it replaces — 25.0 MB became 5.9 MB. FLAC is an
integer format, so the float stream is quantised to 24 bits first; the
compression after that point is lossless. Set `capture_format = "wav"` if you
would rather have a file every tool can open without thinking about it.

### What is kept when the disk fills

Two rules, both deliberate departures from the obvious policy.

**The record outlives the recording.** Every capture is audio plus a small JSON
sidecar carrying the system, the coordinates, the detector scores and the period.
Measured on a real session, 54 captures were 946 MB of audio and 40 KB of
sidecars — the record is four thousandths of one percent of the payload. So
sidecars are *never* deleted. When the budget reclaims a recording it sets
`audio_evicted` in the sidecar and leaves it in place. Trilaterating a source
needs the coordinates and the score, not the waveform, so the observations keep
accumulating long after the audio is gone.

**The weakest go first, not the oldest.** Evidence does not lose value by ageing.
A plain oldest-first policy deletes the strongest thing you ever recorded to make
room for a weak detection from this evening. Captures are ranked by detector
score — with a confirmed Landscape Signal outranking everything, because the
period is the one measurement here that has been checked against a known
recording — and the lowest-ranked audio is reclaimed first, oldest first among
equals. The best `protect_best_captures` are held back entirely, until nothing
else is left: a protected set that could overrun the disk would not be a budget,
so the policy eats into it rather than let an unattended session fill the drive.

Exports get no ranking. A PNG is a rendering of data still held elsewhere, so the
only thing separating two of them is which you asked for more recently; they are
trimmed oldest-first under `export_budget_mb`.

The control panel shows both budgets and the number of observations held, with a
button to apply them immediately rather than waiting for the next capture.


Nothing is written to disk during normal operation. The rolling PCM buffer lives
in memory and is the pre-roll.

When a detection scores above `trigger_score`, the tool writes multichannel audio
plus a JSON sidecar containing the system, coordinates, band, excess, bearing,
confidence, and any period estimate. The sidecar is the actual research record.

Three guard rails stop an unattended overnight session filling the drive:
`capture_cooldown_seconds`, `max_captures_per_hour`, and `disk_budget_mb`, the
last of them enforced by the retention policy described below.

## Configuration

`config.toml` beside the executable. CLI flags override it. Defaults:

| key | default | what it does |
|---|---|---|
| `device` | `""` | endpoint id; empty means the default output, in loopback |
| `capture_format` | `"flac"` | container for captures; `"wav"` for uncompressed |
| `disk_budget_mb` | `2048` | ceiling on captured audio; sidecars are exempt |
| `protect_best_captures` | `20` | best-scoring captures held back from eviction |
| `export_budget_mb` | `512` | ceiling on exported PNGs, trimmed oldest-first |
| `overlay_enabled` | `true` | show the overlay when Elite has focus |
| `overlay_fit_between_plotters` | `true` | size and place the overlay in SrvSurvey's free top band |
| `overlay_x_offset_px` | `220` | pixels in from the left edge when not fitting; clears Elite's info icons |
| `pcm_ring_seconds` | `150` | raw audio held in memory — one 109.5 s cycle plus margin |
| `fft_size` / `hop` | `4096` / `2048` | STFT resolution; 23.4 frames/s at 48 kHz |
| `waterfall_seconds` | `300` | how much history the display keeps |
| `longterm_fps` / `longterm_bands` | `1` / `256` | the cheap tier periodicity runs on |
| `novelty_threshold_db` | `8.0` | how far above background counts as an event |
| `background_time_constant_seconds` | `60` | how fast the background model adapts |
| `background_max_freeze_seconds` | `300` | how long a loud bin may resist adaptation |
| `min_event_seconds` | `2.0` | shorter blips are ignored |
| `trigger_score` | `0.6` | score above which audio is written to disk |
| `analysis_update_hz` | `10` | snapshot rate, independent of frame rate |
| `detect_keying` / `detect_structure` | `true` | the two primary detectors |
| `spectrogram_min_hz` / `max_hz` | `200` / `2400` | measured band of the signal |
| `detect_min_hz` / `detect_max_hz` | `180` / `2600` | what the *detectors* look at, separate from the display |
| `spectrogram_median_subtract` | `true` | remove each frequency row's steady level |
| `spectrogram_show_excess` | `false` | subtract each bin's learned background |
| `export_width` / `export_match_published_aspect` | `8192` / `true` | export size and angle correction |
| `keying_min_hz` | `400` | below this is ship rumble, not a transmission |
| `keying_threshold` / `structure_threshold` | `0.5` / `0.35` | report-present thresholds |
| `direction_finding` | `false` | secondary; costs one transform per channel |

## Cost

Measured on a 7.1 endpoint at 48 kHz, both detectors running, with the
`throughput` example (`cargo run --release --example throughput`):

| | CPU per audio second | share of one core | resident |
|---|---|---|---|
| default (direction finding off) | **2.52 ms** | **0.25%** | **42 MB** |

Three things make that possible, each of which was measured rather than assumed:

- Signal-health statistics are **accumulated per block**, not by rescanning the
  ring. Rescanning 150 seconds twice per snapshot at 10 Hz cost 243 ms per audio
  second — 24% of a core, and 98% of the application's entire cost — for a level
  meter.
- **Direction finding is opt-in.** It costs one transform *per channel* instead of
  one, and forces the ring to hold every channel: 220 MB against 27.5 MB.
- The structure scan pools the spectrogram to a **256-row log-frequency image**
  before sweeping it, rather than working over 2049 raw bins. Cheaper, and closer
  to how the decode guides say to read these anyway.

Turning direction finding on roughly doubles CPU and multiplies ring memory by
the channel count. It is one config line, and its acceptance test still asserts a
bearing within 10°.

## Test modes

The entire analysis chain runs with no game, no audio hardware, and no Windows.

```sh
--test-silence
--test-noise
--test-sine 1200
--test-sweep 200 8000 2
--test-landscape            # the important one
--azimuth -55               # pan any synthetic source
--channels 8                # 8 exercises the surround direction finder
```

`--test-landscape` synthesizes a signal whose spectrogram draws a mountain range,
on the documented 109.5 s cycle with features at 0:25, 0:31, 1:20, 1:23 and 1:28,
panned to a chosen azimuth. It is a structural stand-in, not a reproduction — its
job is to give the detector, the periodicity estimator, and the direction finder a
ground truth to be measured against.

Offline analysis of recorded material goes through the identical chain:

```sh
ed-compass --input reference.flac --headless
ed-compass --input reference.wav --loop        # short clips, for periodicity
```

## Building from source

Only needed if you want to change something; the
[Releases](../../../releases) page has ready-made builds.

Windows 11 x64 is the target. On the Windows machine:

```powershell
winget install Rustlang.Rustup
winget install Microsoft.VisualStudio.2022.BuildTools --override `
  "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
cargo build --release
```

The result is a standalone `target\release\ed-compass.exe`.

Developing from macOS or Linux works too. Everything except `audio::capture` and
`audio::device` is platform-independent, so the full test suite and every
synthetic mode run anywhere:

```sh
cargo test
cargo check --target x86_64-pc-windows-msvc    # needs only `rustup target add`; no linking
```

### Cutting a release

Bump `version` in `Cargo.toml`, commit, then push a matching tag:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

GitHub Actions runs the tests, builds the installer and the portable zip, and
publishes them. The workflow refuses to build if the tag and `Cargo.toml`
disagree, which is the mistake that otherwise ships an installer reporting the
wrong version. To build the installer by hand you need
[Inno Setup 6](https://jrsoftware.org/isdl.php):

```powershell
cargo build --release
iscc /DAppVersion=0.1.0 installer\ed-compass.iss
```

## Architecture

```
WASAPI loopback  ─┐
synthetic source ─┼─→ bounded channel ─→ analysis thread ─→ snapshot ─→ UI
WAV / FLAC file  ─┘                            │
                                               └─→ writer thread ─→ WAV + JSON
```

```
src/
    main.rs                CLI and headless mode
    app.rs                 runtime: capture, journal, writer, event list
    pipeline.rs            the analysis engine
    config.rs
    journal.rs             Elite Dangerous journal tailing
    capture_writer.rs      triggered capture and disk budget
    audio/
        device.rs          endpoint enumeration, loopback tagging
        capture.rs         WASAPI, timeline integrity
        format.rs          sample conversion, channel mask → azimuth
        ring_buffer.rs     fixed-capacity multichannel PCM ring
        file_input.rs      WAV / FLAC
        synthetic.rs       test signals, including the Landscape generator
    analysis/
        stft.rs            windowing, FFT, magnitude in dB
        spectrogram.rs     display waterfall and long-term summary tiers
        novelty.rs         background model, event grouping, scoring
        periodicity.rs     autocorrelation
        direction.rs       pan-law, GCC-PHAT, velocity/energy vectors
        statistics.rs      RMS, peak, ZCR, amplitude histogram
    ui/
```

The capture subsystem knows nothing about the UI, and the analysis engine owns no
threads. Adding a VAD, Whisper, or an LLM later means adding another consumer of
the PCM ring, not touching `audio/`.

## Not implemented, deliberately

Speech-to-text, LLMs, classification, continuous recording, template matching
against known signals, bit-level decoding of the Unknown Artefact / Thargoid Link
class, and any network communication. The interfaces are shaped to allow them; the
code is not there.

## Two things worth knowing

**The background is a median, not an average.** A moving average adapts to a long
signal and erases the very thing it is meant to find, and the Landscape Signal's
mountain feature is long enough for that to matter. A median is decided by the
middle of the distribution, so a signal cannot train the detector to ignore
itself, while a genuine permanent change in the room — a fan switching on — is
absorbed as background once it has been there long enough. Each bin also carries
its own scale, which is why ambience being a different shape in different
frequency bands does not disturb it.

**The timeline is never spliced.** Loopback delivers nothing at all while the
endpoint is idle, and devices drop packets. Both cases insert the exact
equivalent amount of silence rather than joining the two sides together, because
autocorrelating for a 109.5 s period falls apart if the clock lies. Gaps are
counted, and any detection spanning one is flagged `GAP` in the event list and in
its sidecar.

## Licence

MIT — see [LICENSE](../LICENSE). Use it, fork it, ship it.

Elite Dangerous is a trademark of Frontier Developments plc. This is an
unofficial tool, not affiliated with or endorsed by Frontier. It reads audio the
game plays and the journal files the game writes, both of which the game offers
to any application; it does not modify, inject into, or read the memory of the
game process. See [Direction finding](#direction-finding-secondary-off-by-default)
for what it does and does not do.

The research it is built on is the Canonn Research Group's, and the commanders
credited throughout are theirs; this repository redistributes none of their
material.
