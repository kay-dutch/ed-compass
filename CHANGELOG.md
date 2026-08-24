# Changelog

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