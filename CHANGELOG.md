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
* **It will no longer open a microphone.** With no output device present —
  headphones unplugged, say — it used to fall back to whatever endpoint existed,
  which meant it opened a microphone and wrote the room to disk as signal
  captures. It now listens to output endpoints only, and says so plainly when
  there is nothing to listen to.
* The capture thread can no longer be held in its drain loop by a misbehaving
  endpoint, which is what made one machine crawl.
* The score is now the emphasised number in the event list. The bearing used to
  be, coloured by its own confidence — which stereo pan law reports as 1.00
  whenever a source is centred, so the brightest number in every row was a
  constant.
* Direction finding is off by default. Measured across a full session on a
  stereo endpoint, every bearing it produced was the same value.
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