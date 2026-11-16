//! Prints a reviewable SQL proposal. The source database is opened read-only.
//! Run with `cargo run -p opendj-library --example review_video_metadata -- DB`.
//! Redirect stdout to a private file. Review and back up the database before
//! applying the proposal with a separate SQLite client while openDJ is closed.
use std::path::Path;

// Kept usable with rustc and the host's existing dependency cache as well.
#[path = "../../engine/src/filename.rs"]
mod filename;
use rusqlite::{Connection, OpenFlags};

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("provide the source database path")?;
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut q =
        db.prepare("SELECT id, path, artist, title, source, remix FROM tracks ORDER BY id")?;
    let mut rows = q.query([])?;
    let mut proposed = 0;
    let mut unresolved = 0;
    println!("-- Read-only proposal. Every update requires the exact reviewed old values.");
    println!("BEGIN IMMEDIATE;");
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let path: String = row.get(1)?;
        let artist: String = row.get(2)?;
        let title: String = row.get(3)?;
        let source: String = row.get(4)?;
        let remix: String = row.get(5)?;
        // Imported VDJ metadata is the diagnosed source. Independently read
        // file tags remain outside this repair proposal.
        if source != "vdj" {
            continue;
        }
        let path_ref = Path::new(&path);
        let stem = path_ref.file_stem().unwrap_or_default().to_string_lossy();
        let Some((_, video)) = filename::youtube_suffix(&stem) else {
            continue;
        };
        let (new_artist, new_title) = filename::lookup_names(path_ref, &artist, &title);
        if (new_artist.as_str(), new_title.as_str()) == (artist.as_str(), title.as_str()) {
            if title.contains(video) || title.contains(&video.replace('_', " ")) {
                unresolved += 1;
                println!("-- REVIEW UNRESOLVED id={id} path={path:?} artist={artist:?} title={title:?} basename_id={video:?}");
            }
            continue;
        }
        let new_remix = if remix == video || remix == video.replace('_', " ") {
            ""
        } else {
            remix.as_str()
        };
        proposed += 1;
        println!("-- id={id} source={source:?} basename_id={video:?} path={path:?}");
        println!("-- artist {artist:?} -> {new_artist:?}; title {title:?} -> {new_title:?}; remix {remix:?} -> {new_remix:?}");
        println!("UPDATE tracks SET artist={}, title={}, remix={} WHERE id={} AND path={} AND artist={} AND title={} AND source={} AND remix={};",
            quote(&new_artist), quote(&new_title), quote(new_remix), id, quote(&path), quote(&artist), quote(&title), quote(&source), quote(&remix));
    }
    println!("COMMIT;");
    eprintln!(
        "proposed={proposed} unresolved={unresolved}; source opened read-only; no updates executed"
    );
    Ok(())
}
