# Brazilian local-source presets

The recipe supplies eight banks with sixteen Sounds each. Six banks hold 96 complete source loops or phrases. Two banks hold 32 one-shot slots, including pandeiro and cuíca alongside drums from Brazilian funk packs. These are 128 slots with 124 distinct source files. The JSON records each original relative path, pack ID, source page, hash, duration and classification. All loop banks use 130 BPM private WSOLA derivatives. Originals stay intact.

| Bank | Source material |
| --- | --- |
| A | JF No Beat, fourteen Tamborzão-folder loops and two Batidas Variadas loops |
| B | JF No Beat, Atabaque folder, sixteen source percussion phrases |
| C | DJ Nino, sixteen source beat loops |
| D | Beats Mandelão 2023 and DF Kit Fim de Ano, sixteen source phrases |
| E | DJ Rique Sales Mandelão, sixteen source phrases including points, drums and vocal material |
| F | Rohhsa Funk Brasileiro Esotérico, sixteen source arrangements or beat phrases |
| G | Masta Funk drum one-shots plus Kit de Pontos cuíca and pandeiro |
| H | GringoBeats808 TrapFunk drum one-shots plus Kit de Pontos cuíca and pandeiro |

Bank names follow source labels. The library makes no claim of performer-verified regional authenticity. Original beat loops preserve their source rhythm and instrumentation. They are played as whole phrases, with one trigger at the beginning of a phrase-length pattern. Atabaque phrases, melodic points, vocals and complete arrangements are recorded as loops because their full source phrase is used. They are never sequenced as repeated snare hits.

Each loop bank has sixteen separate patterns, each playing a different source Sound. Its four presets open patterns 1, 5, 9 and 13, with all sixteen patterns available after loading. Each one-shot bank has four original two-bar demonstration patterns using alternate Sound slots and rhythmic placement. These demonstrations carry source-bank names and have no regional rhythm-authenticity claim.

The source document starts with eight groups of sixteen Sounds. Native presets export the selected bank as an isolated group, following the application's own preset format. Loading one uses the application's normal selected-group preset action. Installation creates preset JSON files and private derived audio.

## Installation

Build the `opendj-install-brazilian-presets` binary with the normal OpenDJ package build. It uses existing application dependencies and DECKARD APIs.

```sh
opendj-install-brazilian-presets \
  /path/to/res/presets/brazilian/library.json \
  "$HOME/.local/share/laptopan/opendj/sample-packs" \
  "${XDG_CONFIG_HOME:-$HOME/.config}/opendj/Instruments/Presets/Groove Box"
```

The three arguments are the recipe, local collection root and exact preset output directory. The shared `--content RECIPE_JSON PRESET_OUTPUT_DIR` command and GUI selector also accept this bank recipe, finding the sample collection through `OPENDJ_MASCHINE_SAMPLE_ROOT` or the default XDG data location. All sources must already exist. The installer verifies SHA-256, decodes WAVs, checks duration, finite audio and nonzero signal, then reports measurements as JSON on stdout. It publishes each new preset atomically with a hard link. Existing filenames are preserved, including user edits. Rerunning fills missing presets. Private sample audio stays in the collection. No audio is embedded in the repository or preset JSON.

Rescan the preset list through the normal instrument selection or restart the app. Choose a preset explicitly to load its bank into the selected group. The installer never loads a preset, modifies a project or changes preferences.

## Tempo and launch behavior

Rohhsa tempos come from the source filenames. Other source beat counts are 130 BPM phrase-grid hypotheses with original duration within 80 ms of the grid, recorded individually in `tempo_basis`. Preset names show the fitted 130 BPM playback tempo. Source BPM hypotheses remain in the embedded recipe. Duration fitting cannot establish musical meter or resolve half/double-time ambiguity.

Native Groove presets preserve the deck's mix tempo. Set the deck to the displayed fitted 130 BPM. The installer uses the same SDK WSOLA stretch implementation as Fit, with zero pitch shift, and saves content-addressed 24-bit stereo derivatives under `sample-packs/.opendj-derived/brazilian-v1`. The original WAVs are preserved. Other playback tempos require a further explicit Fit operation. Loop sources use envelope type 0 and polyphony 1, matching Retrigger. Pressing a pad again restarts its single voice. One-shot banks also use Retrigger. Sampler play ranges cover the complete selected WAV and looping remains disabled. Per-Sound gain only attenuates to a maximum peak of 0.35. One-shot demonstration groups have another 9 dB of headroom.

Offline checks cover source identity, audio measurements, native document round trips, group import, sixteen Sounds per bank, launch settings one-note loop patterns, and real SDK engine renders of a loop preset and a one-shot preset. GUI playback, subjective musical review and controller/hardware behavior require runtime validation.
