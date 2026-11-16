# Groove Box

The Groove Box is a small drum machine that takes the place of a track on a
deck. It plays in time with the mix, so you can add a beat under a track,
keep a groove going between two tracks, or practise mixing without a second
track.

## Start a groove

1. Click **Jam** in the rail at the far left (the keyboard icon)
   and drag a kit from **Groove Box** onto a deck. A track on that deck is
   stopped and ejected first.
2. Press **PLAY** in the Groove Box. It starts on the next downbeat of the
   playing deck, so it is in time from the first hit.
3. Pick a genre (Basics, Techno, House, Drum & Bass and more) and then a
   groove from the menu beside it. Each groove has a line saying what it is.

To go back to a normal deck, load a track onto the deck, or press EJECT
on the stopped Groove Box.

## Sequencer, Synth and Mixer

The Groove Box fills the whole deck. Along the top are PLAY, the kit menu,
the genre, the groove, **Chain ▾**, **A** to **D**, the tempo and three
tabs.

**SEQUENCER** shows eight tracks, each in its own color, with 16 squares
for the 16 steps of a bar: kick, snare, closed hat, open hat, clap, rim,
cowbell and cymbal. The cymbal track plays the crash or the ride, whichever
the groove you load uses. The icon on the left shows the track's sound; click it
to hear the sound, and hover over it to see its name. A lit square plays the
sound on that step. Click a square to add or remove a hit; when the Groove
Box is stopped, the click also plays the sound. Right-click a lit square to
make the hit accented (brighter, a little louder), then soft (paler, half as
loud), then **50%** (it plays about every other time round), then normal again. A track with no hits stays
dimmed, and a track's icon lights up as it plays. The brighter column is the
step that's playing. **M** mutes a track, **S** solos it.

### Dice: new ideas from the groove

The **⚄** button in the top bar varies every track that isn't locked; right-click a
track's icon to vary only that track, or to lock it. Each change waits for the next bar
while the Groove Box plays, so it always lands in time:

- **New groove** (or **New track**): fresh hits in the style of the genre picked, learnt
  from how that genre's grooves use each sound step by step.
- **Mutate**: moves a hit or two to a nearby step, and now and then adds or drops one.
  The kick's downbeat and the snare's backbeat stay put.
- **Busier** and **Calmer**: one hit more on the strongest free step, or one fewer, the
  weakest first.
- **Even out**: the same number of hits spread as evenly as they go (a Euclidean rhythm).
- **Shift later** and **Shift earlier**: every hit a step later or earlier.

**VARY**, at the top of the ⚄ menu, lets the groove change by itself as it plays: turned
up, weak hits sometimes drop out, soft ghost notes come in where the genre puts them, and
every hit wavers a little in loudness. The kick's downbeat and the snare's backbeat always
play. The ⚄ button is outlined while VARY is up. **Basics** has two grooves made to breathe
this way: **Living house** and **Rolling breaks**.

**Undo** in the ⚄ menu puts back the groove as it was, one change at a time.

**SYNTH** shows each track's sound. Click the sound's name to pick another
from the kit. **PITCH** tunes it up or down by up to an octave, **DECAY**
makes it shorter, and **TONE** darkens it (turned left) or thins it (turned
right).

Under the kick, **KEY** tunes the kick to the playing track's key. Click it
to go from **OFF** to **ROOT** (the key's root note) to **5TH** (its fifth)
and back. The kick moves to the nearest such note, never more than half an
octave, and the button shows the note it plays. This suits long, boomy
kicks most (Boom, Analog); a short, punchy kick has little pitch to tune.

**MIXER** has a strip for each track: **PAN** places it left or right, the
fader sets its level beside a meter, and **M** and **S** mute and solo it.

**4x4**, at the bottom left, shows the Sequencer as 16 square pads laid out like a pad
controller: step 1 at the bottom left, step 16 at the top right, four steps to a row. The
tracks down the left pick which track the pads show, the same track a pad controller
edits. Click a pad to add or remove a hit, right-click it to change its level. Click
**4x4** again for the eight rows of 16.

### Fills and Remix

The bar under the tracks plays fills:

- **AUTO 1** plays a fill in the last beat of every 4th bar, the last two beats of every
  8th bar and the whole of every 16th bar. **AUTO 2** does the same twice as often (every
  2nd, 4th and 8th bar). The bars count with the leading track's beat, so the fills land at
  the end of its phrases. After a fill of two beats or more, the cymbal crashes on the
  next downbeat. **NO FILLS** turns them off.
- Hold **FILL** to play a fill now, for as long as you hold it.
- Hold the **REMIX** strip and slide along it: the groove plays one of 16 figures while you
  hold it, and goes straight back when you let go. From the left: hat rolls, rolls over
  two or three sounds, the tracks' patterns swapped between sounds, and at the far right
  16th rolls over everything.

Controller mappings can set `[GrooveN],auto_fill_style` to 1 for a gentler
fill derived from the current pattern. It adds percussion without changing the
written hits or the kick; 0 keeps the original fill. This applies to held and
automatic fills, not Remix.

`[GrooveN],follow_breaks` makes the sequencer and fills pause when the leading
track drops into a sustained quiet section. `break_sensitivity` is 0–1 (higher
detects smaller drops), with 0.5 as its default; `break_active` shows the pause.
The detector learns the level and ordinary gaps in the track, then resumes on
the next bar once the audio returns. It cannot anticipate a break, and a quiet
breakdown can resemble one. Turn the feature off to override it; played pads and
sound previews still work. With no playing track leader, including Link-only
or a Groove Box playing alone, the groove plays normally.

On a Maschine Mikro MK3, **PERFORM** turns the touch strip into the REMIX strip (press it
again for the knob's setting), hold **NOTE REPEAT** for FILL, and **AUTO** steps through
NO FILLS, AUTO 1 and AUTO 2.

## Kits and grooves

The kit menu at the top left has six kits, all synthesised, so they are
free to use anywhere:

- **Analog** and **Synthetic**: kick, snare, clap and hats from the drum
  models of Mutable Instruments Plaits, with two toms each.
- **Boom**: a deep, long kick and soft analogue drums, 808-style.
- **Punch**: a short, punchy kick and bright hats, 909-style, for house and
  techno.
- **Tin**: small, ticking rhythm-box drums, 606-style.
- **Dub**: a soft kick, a rimshot and a chord stab, for dub techno.

Every kit plays the same eight tracks, so any groove sounds right on any kit.
Kits with more sounds (toms, shaker, chord) offer them in each track's sound
menu in **SYNTH**.

**A**, **B**, **C** and **D** are four grooves you can switch between. A new
one starts at the next bar, so the switch is always in time. Picking a
groove loads it into the one you picked.

For libraries organized in sets of four consecutive grooves, controllers can
select the genre and set `[GrooveN],library_set` to the zero-based group number,
then pulse `load_set`. All four slots load together at the next bar, or
immediately while stopped; `set_pending` stays lit while waiting. An incomplete
group is ignored. The genre and group are captured when the pulse is received.
Brazilian sets also load their kit, swing, fill style, fill frequency and
per-row sound, level, pitch and decay at the same bar. Other genres keep
those settings as they were.

The grooves come in genres. **Basics** has openDJ's own six; the other
genres (techno, house, drum & bass, breakbeat, ambient, dubstep and more, 400
grooves in all) are drum patterns from
[mpump](https://github.com/gdamdam/mpump), used under its licence (AGPL-3.0).

**Chain ▾** picks a second groove from the same genre. It goes into the
groove after the one you picked (A's partner is B, C's is D), and the two
take turns, a bar each, until you set Chain to Off.

Your grooves, kit, sounds and mixer settings are kept when you quit.

## Timing

The Groove Box follows, in this order:

1. The deck that SYNC follows, or else the first playing deck with a tempo.
2. Ableton Link, when it is on (see [Ableton Link](link.md)).
3. Its own tempo, 120 BPM, when nothing else is playing.

The tempo button in the top bar shows the BPM, outlined while it follows the
mix. Click it for:

- **Follow the mix**: on by default. Turn it off to keep the Groove Box at
  its own tempo whatever plays.
- **−1**, **−.1**, **+.1** and **+1**: change the BPM. This turns following
  off, so the tempo stays where you put it.
- **Half time**, **Normal** and **Double**: follow the mix at half or twice
  its tempo (and turn following back on).
- **SWING**: delays every other step for a looser feel.

If
the leading deck jumps (a hot cue or beat jump), the Groove Box moves to the
new beat at its next bar rather than mid-bar.

The Groove Box plays through its deck's channel, so the EQ, filter, effects,
channel fader and crossfader work on it as they do on a track.

## In 4-Deck

Both arrangements of the 4-Deck layout show the complete Groove Box,
including its Sequencer, Synth, Mixer and fill controls. **2×2** places the
DJ decks above decks 3 and 4. **1×4** puts all four editors across the
window. The Looper's four layers and recording controls stay available in
both arrangements. [Layouts](layouts.md)

## Maschine with Deckard

Deck 3 can run the embedded Deckard instrument. Load a Groove Box onto
deck 3, open its **Preset ▾** menu and choose **Deckard · 16 Sounds**.
The **DECKARD ▾** menu returns to **Original Groove Box**. The original
kits, eight-row grooves and their saved settings are kept.

**A** through **H** choose a Group. Each Group has 16 Sound slots, shown as
four rows of four pads, with Sound 1 at the lower left. Click a pad to
select and play its Sound. Its level meter shows the audio from that
Sound. Click the selected Sound's name below the pads to choose its
instrument.

The instrument menu offers Sampler, Audio, Kick, Snare, Hi-Hat, Tom,
Percussion, Cymbal, Bass Synth and Poly Synth. These load Deckard's native
modules. The eight knobs below the editor show the focused channel or
module's parameters. **‹** and **›** move through their pages. A hosted
VST's separate editor window is currently unavailable in the embedded
instrument.

The Scene menu lists each Scene's Group-to-Pattern references. The Pattern
menu chooses the focused Group's Pattern in that Scene. **+ Scene** and
**+ Pattern** create entries in the project.

- **PATTERN** shows the 16 Sounds' events. Double-click an empty place to
  add a sixteenth note at that Sound's base key. Drag a note to move its
  time or Sound, and drag its right edge to change its length.
- **PIANO ROLL** shows the selected Sound's pitches. Drag a note vertically
  to change its pitch. **−** and **+** move the keyboard view by an octave.
- Scroll over a note to change its velocity. Right-click it to remove it.
  **↶** and **↷** use the document's Undo and Redo history.
- **●** records played pads into the Pattern. The playhead follows the
  instrument's playing position.

### Browse sounds and instruments

**BROWSE** shows the indexed library shared with the controller. Pick the
content kind, switch **FACTORY** or **USER**, and use the product, type,
sub-type and character menus to narrow the results. The top **★** filters
favorites. Select a row, press **PREHEAR** to hear it, or **LOAD** to load
it. Double-clicking a row loads it too. The lower **★** changes the
focused result's favorite status.

### Sampling

Open **SAMPLING** for the selected Sound. **LOAD** picks an audio file.
Under **CAPTURE**, choose the source deck, Mix or a stem, the number of
bars, and a single Sound or 16 slices. **CAPTURE** prepares the sample and
assigns it to the selected Group. **×** cancels a pending capture.

Under **EDIT**, drag across the waveform to select a range. **TRIM** sets
the play range, **NORMALIZE** prepares a normalized copy, and **REVERSE**
changes playback direction. Gain, pitch, fades and the loop switch edit
the selected sample's settings. **Grid** and **Detect** prepare 16 slices.
For **Manual**, click the waveform to add cut markers, then press
**SLICE** with 17 boundaries for 16 Sounds. **CLEAR** resets the markers.
Prepared audio is stored separately from its source file.

### Keep and export a project

The embedded document is saved automatically and restored when openDJ
starts. **OPEN** chooses a `.mxprj` project. **SAVE** asks for a folder and
writes a new `.mxprj` copy, adding a number when the name already exists.
The 20 most recent projects opened or saved successfully appear by filename
in the controller's **FILE → RECENT** list, newest first. Turn the wheel or
knob 8 to select one and press **LOAD** or the wheel to open it. The list is
kept between runs.
A missing file reports its path and keeps the current project available.

On the controller, **SAVE** writes to the current project's file. A new
project is saved in `maschine/projects` inside openDJ's configuration folder
with a safe filename and a number when needed. **SAVE COPY** creates another
file beside the current project, keeping relative sample paths usable. A
project without a file uses the private projects folder. These project
commands work directly from the controller.
**NEW** and **LOAD** preserve the current document through its automatic save
and backup before switching projects.

The controller's FILE page also saves the focused Group or Sound as
`.mxgrp` or `.mxsnd` after choosing a folder on the computer. **Export Audio**
renders the selected Scene to a new WAV file at the current Maschine tempo.

### Performance information on the deck

Original Groove Box and DECKARD share the tempo menu, Auto Fill buttons, held
FILL and REMIX strip. The tempo menu offers Follow the mix, half/normal/double
time, manual BPM adjustment, additive fills and track-break following with
sensitivity, plus Swing for the active group. The status line shows the engine BPM, external Mix / Link clock or
own tempo, transport state, active fill/remix, break-follow mode and break pause.
Its meter follows the deck output. The clock indicator reports external following
without identifying the individual leader.

DECKARD applies fill/remix to the focused group's eight mapped percussion roles.
Its REC QUANT menu toggles quantization of newly recorded notes and selects a
1/4–1/64 grid.

The Looper uses the same status line for BPM, clock, selected recording input,
new-recording length and output level. Its numbered layer buttons show armed,
recording, playing and overdubbing states, loop position and recorded bar count.
Layer level, mute, clear, capture, undo/redo and then-overdub retain their existing
controls in both four-deck arrangements.

### Group Pattern playback

Hold **PATTERN** to select one of the focused Group's Patterns on the 16 pads.
Release it to return to the previous view. Softkey 1 pins or unpins this picker.
**SHIFT + PATTERN** opens or closes the step editor. **STEP** toggles playhead
following, including while editing steps.

Selecting a Pattern assigns it to the focused Group in the selected Scene.
Softkey 2 in the Pattern picker stops or resumes that Group by removing or
restoring its Scene assignment. Other Groups keep their assignments. The global
transport remains the master playback control.

The GUI shows the Scene's assigned Pattern and offers **STOP GROUP / PLAY GROUP**.
The Pattern menu launches a Pattern. Its **Edit** button changes only the editor
focus, allowing another Pattern to remain assigned for playback.
