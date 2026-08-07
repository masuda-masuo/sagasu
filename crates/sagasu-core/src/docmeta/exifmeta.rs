//! EXIF (`kamadak-exif`): the camera and the moment, off an image file.
//!
//! Images have no text body, so this is a metadata-only path — the extensions
//! stay on the denylist in [`crate::text`] and only the tag pass opens them.
//!
//! **An image without EXIF is not an error.** Screenshots, exported PNGs and
//! anything that has been through a stripping tool have none, and they are the
//! majority on a real disk. Reporting them as failures would bury the handful
//! of genuinely truncated files under tens of thousands of ordinary ones, so
//! `NotFound` comes back as an empty [`EmbeddedMeta`] and only a malformed
//! container is an error.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{anyhow, Result};
use exif::{In, Tag, Value};

use super::{clean, iso_date, EmbeddedMeta};

pub(super) fn meta(path: &Path) -> Result<EmbeddedMeta> {
    let file = File::open(path).map_err(|e| anyhow!("failed to open: {e}"))?;
    let mut reader = BufReader::new(file);
    let exif = match exif::Reader::new().read_from_container(&mut reader) {
        Ok(exif) => exif,
        Err(exif::Error::NotFound(_)) => return Ok(EmbeddedMeta::default()),
        Err(e) => return Err(anyhow!("unreadable EXIF: {e}")),
    };

    let mut meta = EmbeddedMeta::default();

    // `Make` and `Model` overlap ("NIKON CORPORATION" + "NIKON D750"), so they
    // are joined only when the model does not already carry the make — one
    // `camera:` bucket per camera is the point of the axis.
    let make = text(&exif, Tag::Make);
    let model = text(&exif, Tag::Model);
    meta.camera = match (make, model) {
        (Some(make), Some(model)) => {
            let first = make.split_whitespace().next().unwrap_or(&make).to_string();
            if model.to_lowercase().contains(&first.to_lowercase()) {
                Some(model)
            } else {
                Some(format!("{make} {model}"))
            }
        }
        (Some(one), None) | (None, Some(one)) => Some(one),
        (None, None) => None,
    };

    // Original capture time first: `DateTime` is the last *edit*, which moves
    // when a tool rewrites the file and would put the same photo in two
    // different `date:` buckets over its life.
    meta.date = text(&exif, Tag::DateTimeOriginal)
        .as_deref()
        .and_then(iso_date)
        .or_else(|| {
            text(&exif, Tag::DateTimeDigitized)
                .as_deref()
                .and_then(iso_date)
        })
        .or_else(|| text(&exif, Tag::DateTime).as_deref().and_then(iso_date));

    // Some cameras and most editors write an author into EXIF/XMP's Artist.
    if let Some(artist) = text(&exif, Tag::Artist) {
        meta.push_author(&artist);
    }
    if let Some(title) = text(&exif, Tag::ImageDescription) {
        meta.title = clean(&title);
    }

    Ok(meta)
}

/// The value of an ASCII field, cleaned.
///
/// Read out of `Value::Ascii` rather than through `display_value()`: the
/// display form wraps strings in quotes, and `camera:"canon eos r6"` with the
/// quotes baked into the tag value is not a bucket anybody will match.
fn text(exif: &exif::Exif, tag: Tag) -> Option<String> {
    let field = exif.get_field(tag, In::PRIMARY)?;
    match &field.value {
        Value::Ascii(parts) => {
            let joined: Vec<String> = parts
                .iter()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect();
            clean(&joined.join(" "))
        }
        other => clean(&other.display_as(tag).to_string()),
    }
}
