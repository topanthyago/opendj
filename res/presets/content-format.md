# Native content recipe format 1

The preset installer also accepts a runtime content catalog. Catalogs, original projects and sample assets can live outside the application source and be added without recompiling OpenDJ.

```sh
opendj-install-brazilian-presets --content \
  /path/to/native-recipes.json \
  "${XDG_CONFIG_HOME:-$HOME/.config}/opendj/Instruments/Presets/Groove Box"
```

The existing three-positional-argument Brazilian library command remains available.

## Catalog

```json
{
  "format": 1,
  "projects": [{
    "name": "Original source pattern",
    "source": "original-project.flp",
    "project_metadata": "projects/source/project.json",
    "project_sha256": "original-project-hash",
    "tempo": 130,
    "ppq": 96,
    "velocity_max": 128,
    "sounds": [{
      "iid": 0,
      "name": "Original sample channel",
      "path": "objects/sample.wav",
      "sha256": "sample-file-sha256",
      "root_note": 60,
      "fine_tune": 0,
      "channel_pitch_shift": 0,
      "enabled": true
    }],
    "patterns": [{
      "name": "Original pattern name",
      "length": 384,
      "notes": [{
        "sound": 0,
        "position": 0,
        "length": 24,
        "key": 60,
        "velocity": 100
      }]
    }],
    "warnings": [],
    "unresolved_channels": []
  }]
}
```

Each project entry may represent an original project's resolved pattern subset. Sample references can be absolute or relative to the catalog's directory. `sound` is the zero-based index into `sounds`, independent of the source DAW's channel ID. `iid`, source references and additional metadata are retained in the preset's `source_content` object.

`ppq`, pattern length, note position and note length use the source clock. Native timing uses the SDK PPQ with nearest-tick conversion and reports rounding. Zero-length triggers use the native minimum of one tick. Their original values remain in `source_content`.

`key` and `root_note` are MIDI note numbers, with 60 representing C4. Source octave-display conventions require normalization in the extractor. FL's raw key 60 maps directly to MIDI 60. Note velocity is scaled from `velocity_max` to native 0–127 with nearest-integer rounding. The default `velocity_max` is 128 for the supplied FL extractor. MIDI/Ableton extractors should supply 127 when their velocities use the MIDI range.

`fine_tune` and `channel_pitch_shift` are cents and are summed into the sample zone tuning. `root_note` defaults to 60. `enabled` defaults to true and false preserves a muted channel. Other source parameters remain available as metadata. FL slide, per-note fine pitch, per-note pan, stretching mode, plugin processing and original mixer behavior require separate interpretation. Non-default per-note values and unresolved channels produce warnings. The converter preserves these source fields and creates no substitute instrument for unresolved plugins.

## Output and preservation

The installer verifies each resolved sample's SHA-256 and decoded finite audio. WAVs and other SDK-readable formats use their existing local files. Formats the SDK cannot read, including the acquired WV and MP3 sources, are decoded with `ffmpeg` into content-addressed 24-bit WAVs in the private `Converted audio` output subdirectory. This fallback requires `ffmpeg` in PATH. Original files remain intact.

A recipe supports 1–128 Sound entries split into groups of sixteen, up to eight groups. Every group contains sixteen native Sounds. Each original pattern gets a scene with all groups assigned. The converter exports a combined `.mxprj` under `Converted projects` and a Native Groove preset for each group. It keeps original notes, timing, pitch and normalized velocities. It uses polyphonic sample playback and reports the limits of source envelope/plugin reproduction. A fixed 12 dB group headroom leaves source sample dynamics unchanged.

Native preset loading follows the application's selected-group operation and preserves the deck's mix tempo. Preset names display the source BPM. Opening the converted `.mxprj` restores its source tempo and combined groups. The installer only publishes content files and derived audio. Selecting or loading them remains an explicit application action.

New files are published atomically and existing names are preserved, including user edits. Repeating the same catalog fills missing files. Content-derived identifiers keep different source recipes distinct. A failing project entry is reported while other entries continue.

## Machine-readable report

Stdout is a JSON object with `format`, `imported_projects`, `notes`, `presets`, `created`, `existing_preserved`, `projects` and `errors`. `imported_projects` counts accepted recipe entries. Each project report includes its name, tempo, groups, note count, skipped notes, Native preset count, file counts, converted project path and warnings. Errors contain the source entry name and error text. Callers should inspect `errors` and the imported count, including when the process exits successfully after a partial import.

The small native regression test covers relative sample paths, splitting seventeen resolved Sounds into two groups, opaque unresolved channels, root tuning, PPQ conversion, pitch, normalized velocity, zero-length triggers and preservation of modified preset files.
