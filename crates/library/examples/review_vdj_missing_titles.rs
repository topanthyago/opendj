//! Prints guarded SQL for VDJ titles lost by the old Remix-before-fallback bug.
//! Both inputs are read-only. Usage: review_vdj_missing_titles DB VDJ_XML
//! Explicit XML Titles and conflicting source evidence are retained for review.
use std::collections::HashMap;
use std::path::Path;

#[allow(dead_code)]
#[path = "../../engine/src/filename.rs"]
mod filename;
use rusqlite::{Connection, OpenFlags};

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let database = args.next().ok_or("provide DB and VDJ_XML paths")?;
    let xml_path = args.next().ok_or("provide VDJ_XML path")?;
    let xml = std::fs::read_to_string(&xml_path)?;
    let doc = roxmltree::Document::parse(&xml)?;
    let mut originals = HashMap::<&str, Vec<(&str, &str)>>::new();
    for song in doc
        .root_element()
        .children()
        .filter(|n| n.has_tag_name("Song"))
    {
        let Some(path) = song.attribute("FilePath") else {
            continue;
        };
        let tags = song.children().find(|n| n.has_tag_name("Tags"));
        let title = tags
            .and_then(|t| t.attribute("Title"))
            .unwrap_or_default()
            .trim();
        let remix = tags
            .and_then(|t| t.attribute("Remix"))
            .unwrap_or_default()
            .trim();
        originals.entry(path).or_default().push((title, remix));
    }
    let db = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut query =
        db.prepare("SELECT id,path,artist,title,remix FROM tracks WHERE source='vdj' ORDER BY id")?;
    let mut rows = query.query([])?;
    let mut proposed = 0;
    let mut review = 0;
    println!(
        "-- Read-only proposal, backed by original VDJ XML {:?}.",
        xml_path
    );
    println!("BEGIN IMMEDIATE;");
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let path: String = row.get(1)?;
        let artist: String = row.get(2)?;
        let title: String = row.get(3)?;
        let remix: String = row.get(4)?;
        let generated = !remix.is_empty() && title.trim() == format!("({remix})");
        if !title.trim().is_empty() && !generated {
            continue;
        }
        let evidence = originals.get(path.as_str());
        if !evidence.is_some_and(|entries| entries.iter().all(|(t, r)| t.is_empty() && *r == remix))
        {
            review += 1;
            println!("-- REVIEW id={id} path={path:?} artist={artist:?} title={title:?} remix={remix:?} conflicting_or_missing_xml={evidence:?}");
            continue;
        }
        let path_ref = Path::new(&path);
        let (new_artist, new_title, new_remix) = filename::vdj_names(path_ref, &artist, "", &remix);
        if new_title.trim().is_empty() || new_title.trim() == title.trim() || new_artist != artist {
            review += 1;
            println!("-- REVIEW id={id} path={path:?} artist={artist:?} title={title:?} remix={remix:?} proposed_artist={new_artist:?} proposed_title={new_title:?}; identity needs review");
            continue;
        }
        proposed += 1;
        println!(
            "-- id={id} path={path:?}; XML Title absent/empty and Remix matches, original_rows={}",
            evidence.unwrap().len()
        );
        println!("-- artist {artist:?} -> {new_artist:?}; title {title:?} -> {new_title:?}; remix {remix:?} -> {new_remix:?}");
        println!("UPDATE tracks SET artist={},title={},remix={} WHERE id={} AND path={} AND artist={} AND title={} AND remix={} AND source='vdj';",
            quote(&new_artist),quote(&new_title),quote(&new_remix),id,quote(&path),quote(&artist),quote(&title),quote(&remix));
    }
    println!("COMMIT;");
    eprintln!("proposed={proposed} review={review}; inputs read-only; no updates executed");
    Ok(())
}
