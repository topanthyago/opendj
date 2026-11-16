#!/usr/bin/env python3
"""Generate original, sample-free native melodic recipes for OpenDJ IMPORT."""

import argparse
import json
from pathlib import Path

PPQ = 96
LENGTH = 16 * PPQ
NAMES = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]
MAJOR = [0, 2, 4, 5, 7, 9, 11]
MINOR = [0, 2, 3, 5, 7, 8, 10]


def project(tonic, minor):
    scale = MINOR if minor else MAJOR
    mode = "minor" if minor else "major"
    key_name = f"{NAMES[tonic]} {mode}"
    major_tonic = tonic + 3 if minor else tonic
    camelot = f"{(major_tonic * 7 + 7) % 12 + 1}{'A' if minor else 'B'}"
    progression = [0, 5, 3, 6 if minor else 4]

    def pitch(degree):
        return 60 + tonic + scale[degree % 7] + 12 * (degree // 7)

    def note(sound, beat, duration, degree, velocity):
        return {
            "sound": sound,
            "position": round(beat * PPQ),
            "length": round(duration * PPQ),
            "key": pitch(degree),
            "velocity": velocity,
        }

    def bass(syncopated=False, changes=False):
        events = []
        rhythm = (
            [(0, 0.5, 0, 94), (0.75, 0.25, 0, 72), (1.5, 0.5, 4, 84),
             (2.25, 0.5, 0, 92), (3, 0.25, 7, 80), (3.5, 0.25, 4, 76)]
            if syncopated else
            [(0, 0.75, 0, 94), (1.5, 0.25, 4, 78),
             (2, 0.75, 0, 90), (3.25, 0.5, 7, 82)]
        )
        for bar in range(4):
            degree = progression[bar] if changes else 0
            for beat, duration, interval, velocity in rhythm:
                events.append(note(0, bar * 4 + beat, duration, degree + interval, velocity))
        return events

    def chords(syncopated=False):
        events = []
        for bar, degree in enumerate(progression):
            rhythm = [(0.5, 1.0), (2.5, 0.75)] if syncopated else [(0, 3.5)]
            for beat, duration in rhythm:
                for voice, interval in enumerate([0, 2, 4]):
                    event = note(1, bar * 4 + beat, duration, degree + interval, 61 - voice * 3)
                    # Keep neighbouring chords in a compact mid register.
                    while event["key"] > 76 + tonic:
                        event["key"] -= 12
                    events.append(event)
        return events

    pivot = []
    for bar in range(4):
        for beat, degree in [(0, 0), (1.5, 4), (2.5, 0), (3.5, 7)]:
            pivot.append(note(0, bar * 4 + beat, 0.375, degree, 86))
        for degree in [0, 4, 7]:
            pivot.append(note(1, bar * 4, 2.5, degree, 54))

    melody = []
    phrase = [0, 2, 4, 2, 1, 3, 5, 4, 2, 4, 6, 7, 5, 4, 2, 0]
    for index, degree in enumerate(phrase):
        melody.append(note(1, index, 0.625 if index % 4 != 3 else 0.875,
                           degree, 70 + (index % 4 == 0) * 10))

    patterns = [
        ("Bass · foundation", bass()),
        ("Bass · syncopated variation", bass(syncopated=True)),
        ("Bass + harmony · A", bass(changes=True) + chords()),
        ("Bass + harmony · B", bass(syncopated=True, changes=True) + chords(syncopated=True)),
        ("Pivot · tonic and fifth", pivot),
        ("Harmony · sustained", chords()),
        ("Melody · scale response", melody),
    ]
    result = {
        "name": f"Harmonic Toolkit · {key_name} · {camelot}",
        "tempo": 120,
        "ppq": PPQ,
        "velocity_max": 127,
        "tags": ["Bass", "Keys", "Clean", "Native", "Original", key_name, camelot],
        "harmonic": {
            "tonic": tonic,
            "mode": mode,
            "key": f"{NAMES[tonic]}{'m' if minor else ''}",
            "camelot": camelot,
            "root_note": 60 + tonic,
            "scale_intervals": scale,
            "progression_degrees": [degree + 1 for degree in progression],
            "composition": "Original deterministic procedural phrases",
            "generator": "scripts/generate-harmonic-presets.py",
        },
        "sounds": [
            {"name": f"Bass · {key_name}", "native_instrument": "BassSynth", "root_note": 60 + tonic},
            {"name": f"Harmony · {key_name}", "native_instrument": "PolySynth", "root_note": 60 + tonic},
        ],
        "patterns": [
            {"name": name, "length": LENGTH,
             "notes": sorted(notes, key=lambda n: (n["position"], n["sound"], n["key"]))}
            for name, notes in patterns
        ],
    }
    for pattern in result["patterns"]:
        for event in pattern["notes"]:
            assert (event["key"] - tonic) % 12 in scale
            assert 0 <= event["position"] < LENGTH
            assert 0 < event["length"] <= LENGTH - event["position"]
            assert 0 <= event["key"] <= 127
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    bank = {
        "format": 1,
        "name": "Harmonic Toolkit",
        "generated_by": "scripts/generate-harmonic-presets.py",
        "version": 1,
        "description": "24 major/minor keys, each with seven original native bass, chord, pivot and melody patterns",
        "projects": [project(tonic, minor) for minor in [False, True] for tonic in range(12)],
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(bank, ensure_ascii=False, indent=2) + "\n")
    notes = sum(len(pattern["notes"]) for p in bank["projects"] for pattern in p["patterns"])
    print(f"24 keys · 168 patterns · {notes} original notes · {args.output}")


if __name__ == "__main__":
    main()
