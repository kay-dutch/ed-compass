# Changelog

## v0.6.3 — beta

* Renamed to ED-Compass.
* New installer design.
* Bugfixes.

## v0.6.2 — beta

* Bugfixes.

## v0.6.1 — beta

* Installer design fixes.

## v0.6.0 — beta

* **Updates from inside the app.** When a new version is out, a banner at the top
  of the window offers it. One click downloads it, checks it against the release,
  installs it and reopens ED-Compass. Nothing happens until you click.
* **Listen to Elite alone** (Windows 11). Choose *Elite Dangerous* under
  *Programs* in the device list and ED-Compass hears only the game.
* ED-Compass keeps a log file, `ed-compass.log`, beside its settings — the thing to
  attach to a bug report.
* Bugfixes

## v0.5.0 — beta

* First beta. ED-Compass runs through long sessions without trouble, and from here
  your settings and recordings carry forward from one version to the next. What it
  detects is still provisional.
* Bugfix: the Landscape Signal was often detected but shown as an ordinary anomaly
  instead of lighting SIGNAL.

## v0.4.9 — alpha

* SIGNAL is now reserved for known signals only.
* Bugfix: directional indicator reported full confidence for everything.

## v0.4.8 — alpha

* Directional indicator coloring changes. 

## v0.4.7 — alpha

* Bugfixes.

## v0.4.6 — alpha

* Start without active output device.

## v0.4.5 — alpha

* Simplified cleanup: clean-up button erases all recordings.

## v0.4.4 — alpha

* Simplified cleanup: Recordings are cleaned up oldest first. 
* Bugfix: the overlay animation is smooth again. 0.4.3 made it judder.

## v0.4.3 — alpha

* Bugfix: **The overlay no longer disappears when you Alt-Tab away.**
* Bugfix: SIGNAL and CYPHER no longer stay lit through ordinary flight.

## v0.4.2 — alpha

* SIGNAL stays lit while a detection is still on screen, so the lamp and the
  timeline strip always agree.

## v0.4.1 — alpha

* **Captures you kept by hand are never deleted to make room.** Worth updating
  for.
* Quieter signals are picked up, and anything found is outlined on the
  spectrogram and in the overlay.
* A timeline strip under both spectrograms shows when detections happened.
* Detection is more sensitive, and sensitivity is now adjustable.
* New icon. The overlay zoom is off by default.

## v0.4.0 — alpha

* Detection stays inside the frequency band you configure.
* Only sound output is ever listened to — never a microphone.
* Long signals no longer fade out of view the longer they last.
* The overlay zooms into whatever it finds.
* Direction finding is off by default. It needs a genuine 7.1 endpoint to mean
  anything; on stereo it reports the same bearing whatever is playing.
* The event list highlights the detection score rather than the bearing.
* Turn the in-game music off — see the README for why.
* Known limitation: **drawn-structure detection is unproven.** It does not
  reliably tell the Landscape Signal apart from ordinary ship noise. The period
  reading is the one to trust.
* Various fixes.

## v0.3.0 — alpha

* Detects Thargoid Sensor Morse, and keyed transmissions generally, reported
  through the SIGNAL lamp.
* Drawn-structure detection is much less prone to firing on ordinary ship noise.
* Fixed a phantom symbol rate in the keying readout.

## v0.2.0 — alpha

* Second alpha