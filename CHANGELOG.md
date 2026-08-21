# Changelog

## v0.4.0 — pre-release

* The overlay zooms into what it finds.
* Long signals no longer train the detector to ignore them. 
* Turn the in-game music off.
* Detection now honours the configured frequency band. It never had: every
  detection in a field session came from outside the band the settings asked
  for, which is why the panel lit constantly on ship noise.
* Known limitation: **STRUCTURE does not reliably detect the Landscape Signal.**
  Measured against a real in-game capture rather than a synthetic one, it does
  not separate the signal from ordinary ship ambience. Treat that lamp as
  unproven; SIGNAL and the period reading are the ones to trust.
* Bugfixes

## v0.3.0 — pre-release

* Detects Thargoid Sensor Morse, and keyed transmissions generally. Reported
  through the SIGNAL lamp; validated against a reference recording.
* STRUCTURE no longer fires on ordinary ship ambience. Sustained tones and
  transients are removed before the scan, and those two things are what ambience
  is made of.
* Fixed keying reporting a phantom symbol rate that was really the analysis
  frame rate.

## v0.2.0 — pre-release

* Second pre-release