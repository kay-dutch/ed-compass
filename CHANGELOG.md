# Changelog

## v0.4.2 — pre-release

* **SIGNAL is lit while the evidence is still on screen**, rather than for a
  fixed time after the tool noticed it. The lamp and the timeline strip now
  answer the same question, so they cannot disagree — when the last mark scrolls
  off the left edge, the lamp goes out.

  The fixed hold was wrong twice over. It was an invented number, and it
  described *when the software noticed* rather than when anything happened, so
  two detections moments apart produced two lit periods that pointed nowhere near
  them. How long a signal stays visible is a real quantity; fifteen seconds was
  not. `signal_hold_seconds` is gone.

## v0.4.1 — pre-release

* **Captures you took by hand are no longer deleted.** When the disk filled, the
  budget ranked recordings by what the detectors made of them — and a capture you
  took deliberately, of something the software could not see, scores zero. So the
  one file a person judged worth keeping was the first thing removed. It cost a
  recording of a real signal before it was noticed. Manual captures now outrank
  everything automatic. **This is the reason to update.**
* **The detector had gone silent.** Once detection was confined to the configured
  band, nothing in that band ever cleared the noise bar, and across four real
  recordings it produced no detections at all. There is now a `novelty_sigmas`
  setting, lowered by default, and the same recordings produce events again —
  including in the band the Landscape Signal occupies.
* **Faint strokes are followed rather than thresholded.** A new pass seeds on
  confident ink and follows a line down to a much weaker level, which reaches
  parts of a stroke no single threshold can. Followed strokes are outlined on the
  waterfall and in the overlay.
* **A timeline strip** along the bottom of both spectrograms, showing when
  detections happened on the same axis as the picture that produced them. A lamp
  tells you about now; this tells you about the last two minutes.
* SIGNAL now stays lit for fifteen seconds after something triggers it, instead
  of reporting only the instant.
* Repeating signals are searched for by folding the long-term view against its
  own period — the technique radio astronomy uses for pulsars.
* A new icon, and the overlay zoom is off by default.

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