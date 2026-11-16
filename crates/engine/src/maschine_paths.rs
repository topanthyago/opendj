//! Preserve sample locations before a loaded project can be saved elsewhere.

use deckard_doc::{Document, Op};
use deckard_engine::library::Resolver;
use deckard_model::SampleRef;
use mxprj::decode::{Obj, Value};

pub(super) fn normalize(mut document: Document) -> Result<Document, String> {
    if document.path().is_none() {
        return Ok(document);
    }
    let mut original = Resolver::new(document.path());
    let mut independent = Resolver::new(None);
    let mut ops = Vec::new();
    document.root().walk_paths(&mut |object, path| {
        if object.type_name != "NI::MASCOMMON::FileLocation" {
            return;
        }
        let Some(Value::Str(file)) = object.get("path") else {
            return;
        };
        let alias = match object.get("alias") {
            Some(Value::Obj(alias)) => Some(alias),
            _ => None,
        };
        let alias_text = |field| match alias.and_then(|a| a.get(field)) {
            Some(Value::Str(value)) => value.clone(),
            _ => String::new(),
        };
        let sample = SampleRef {
            kind: object.get("kind").and_then(Value::as_int).unwrap_or(0),
            path: file.clone(),
            library_root: alias_text("root"),
            library: alias_text("library"),
        };
        let Some(resolved) = original.resolve(&sample) else {
            return;
        };
        if independent.resolve(&sample).as_ref() == Some(&resolved) {
            return;
        }
        // The resolver also searches the project's parent (library projects).
        // Canonicalize only resolved references that depend on that context.
        let Ok(absolute) = resolved.canonicalize() else {
            return;
        };
        let Some(absolute) = absolute.to_str() else {
            return;
        };
        ops.push(Op::Set {
            path: path.to_vec(),
            value: Value::Obj(Obj {
                fields: vec![
                    ("kind".into(), Value::Int(0)),
                    ("path".into(), Value::Str(absolute.into())),
                ],
                ..object.clone()
            }),
        });
    });
    if ops.is_empty() {
        return Ok(document);
    }
    document.apply("Preserve sample locations", ops)?;
    // Opening establishes the undo baseline. Keep the origin used by autosave
    // metadata, so this never turns an external project into an untitled one.
    let mut clean =
        Document::from_bytes(document.to_bytes()?, document.path().map(ToOwned::to_owned))?;
    clean.mark_unsaved();
    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use deckard_doc::{NewModule, TakeSpec, Target, ZoneFrom, ZoneSpec};
    use std::path::PathBuf;

    fn samples(document: &Document) -> Vec<SampleRef> {
        let sound = &document.project().groups[0].sounds;
        let mut refs: Vec<_> = sound[0]
            .sampler
            .as_ref()
            .unwrap()
            .zones
            .iter()
            .map(|zone| zone.sample.clone())
            .collect();
        refs.push(sound[1].audio.as_ref().unwrap().takes[0].sample.clone());
        refs
    }

    #[test]
    fn relative_samples_survive_save_as_restore_and_undo() -> Result<(), String> {
        let base = std::env::temp_dir().join(format!("opendj-sample-paths-{}", std::process::id()));
        let old = base.join("original/Projects");
        let new = base.join("destination");
        let library = base.join("library");
        for dir in [&old, &new, &library] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // A real mono PCM WAV, with one audible frame.
        let wav: &[u8] = b"RIFF\x26\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80\xbb\0\0\0\x77\x01\0\x02\0\x10\0data\x02\0\0\0\0\x40";
        let local = old.join("kick.wav");
        let parent = old.parent().unwrap().join("parent.wav");
        let absolute = base.join("absolute.wav");
        for file in [&local, &parent, &absolute, &library.join("library.wav")] {
            std::fs::write(file, wav).unwrap();
        }
        let origin = old.join("external.mxprj");
        let mut document = Document::new_project()?;
        document.add_slot(0, 0, 0, &NewModule::Sampler)?;
        document.set_zones(
            0,
            0,
            &[
                PathBuf::from("kick.wav"),
                PathBuf::from("parent.wav"),
                absolute.clone(),
                PathBuf::from("missing.wav"),
                PathBuf::from("library.wav"),
            ]
            .into_iter()
            .map(|file| ZoneSpec {
                from: ZoneFrom::Template,
                file: Some(file),
                set: vec![],
            })
            .collect::<Vec<_>>(),
            "Fixture",
        )?;
        document.audio_only(0, 1)?;
        document.add_take(
            0,
            1,
            &TakeSpec {
                file: PathBuf::from("kick.wav"),
                frames: 1,
                rate: 48000,
                beats: 1.0,
            },
            "Fixture audio",
        )?;
        let mut ops = Vec::new();
        document.root().walk_paths(&mut |object, path| {
            if object.type_name == "NI::MASCOMMON::FileLocation"
                && matches!(object.get("path"), Some(Value::Str(file)) if file == "library.wav" || file == "kick.wav")
            {
                let is_library = object.get("path") == Some(&Value::Str("library.wav".into()));
                let file = if is_library { "library.wav" } else { "/kick.wav" };
                let root = if is_library { library.clone() } else { base.join("missing-library") };
                ops.push(Op::Set {
                    path: path.to_vec(),
                    value: Value::Obj(Obj {
                        fields: vec![
                            ("kind".into(), Value::Int(2)),
                            ("path".into(), Value::Str(file.into())),
                            (
                                "alias".into(),
                                Value::Obj(Obj {
                                    type_name: "NI::MASCOMMON::AliasPath".into(),
                                    fields: vec![
                                        ("root".into(), Value::Str(root.display().to_string())),
                                        ("library".into(), Value::Str("Test Library".into())),
                                    ],
                                    ..Obj::default()
                                }),
                            ),
                        ],
                        ..object.clone()
                    }),
                });
            }
        });
        document.apply("Fixture library", ops)?;
        document.save_as(&origin)?;
        let original_bytes = std::fs::read(&origin).unwrap();
        let original_refs = samples(&Document::open(&origin)?);

        // Demonstrate the SDK regression through the unnormalized path.
        let mut broken = Document::open(&origin)?;
        broken.save_as(&new.join("broken.mxprj"))?;
        assert!(Resolver::new(broken.path())
            .resolve(&samples(&broken)[0])
            .is_none());

        let mut opened = normalize(Document::open(&origin)?)?;
        assert_eq!(opened.path(), Some(origin.as_path()));
        assert!(opened.is_dirty());
        assert!(opened.undo_label().is_none());
        let normalized_refs = samples(&opened);
        assert_eq!(
            normalized_refs[0].path,
            local.canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(
            normalized_refs[1].path,
            parent.canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(normalized_refs[2..5], original_refs[2..5]);
        assert_eq!(normalized_refs[5], normalized_refs[0]);

        // Both failed write and failed rename retain origin and allow retry.
        assert!(opened.save_as(&base.join("absent/fail.mxprj")).is_err());
        let blocked = new.join("blocked.mxprj");
        std::fs::create_dir_all(&blocked).unwrap();
        assert!(opened.save_as(&blocked).is_err());
        assert_eq!(opened.path(), Some(origin.as_path()));
        assert_eq!(samples(&opened), normalized_refs);
        opened.rename(Target::Sound(0, 0), "Edited")?;
        assert!(opened.undo());
        assert_eq!(samples(&opened), normalized_refs);
        assert!(opened.redo());
        assert_eq!(samples(&opened), normalized_refs);
        let saved = new.join("saved.mxprj");
        opened.save_as(&saved)?;
        let reopened = Document::open(&saved)?;
        let mut resolver = Resolver::new(reopened.path());
        for index in [0, 1, 2, 4, 5] {
            let file = resolver.resolve(&samples(&reopened)[index]).unwrap();
            let audio = deckard_engine::sample::Sample::load(&file)?;
            assert!(audio.frames[0][0] > 0.1);
        }
        // Autosave restoration keeps origin while establishing the same baseline.
        let restored = normalize(Document::from_bytes(
            original_bytes.clone(),
            Some(origin.clone()),
        )?)?;
        assert_eq!(samples(&restored), normalized_refs);
        assert_eq!(restored.path(), Some(origin.as_path()));
        let untitled = Document::from_bytes(reopened.to_bytes()?, None)?;
        assert_eq!(samples(&normalize(untitled)?), normalized_refs);
        assert_eq!(std::fs::read(&origin).unwrap(), original_bytes);
        assert_eq!(std::fs::read(local).unwrap(), wav);
        std::fs::remove_dir_all(base).unwrap();
        Ok(())
    }
}
