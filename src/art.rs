use image::DynamicImage;
use lofty::prelude::*;
use lofty::picture::PictureType;
use std::path::Path;

/// Album art for a track: embedded front cover first, then a sibling
/// cover/folder image, else None. Pre-scaled to keep GPU/encode cost small —
/// ratatui-image fits it to the cell area at render time.
pub fn load_art(path: &Path) -> Option<DynamicImage> {
    let bytes = embedded(path).or_else(|| sibling(path))?;
    image::load_from_memory(&bytes)
        .ok()
        .map(|img| img.thumbnail(512, 512))
}

fn embedded(path: &Path) -> Option<Vec<u8>> {
    let tagged = lofty::read_from_path(path).ok()?;
    let pics = tagged.primary_tag()?.pictures();
    pics.iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or(pics.first())
        .map(|p| p.data().to_vec())
}

fn sibling(path: &Path) -> Option<Vec<u8>> {
    let dir = path.parent()?;
    [
        "cover.jpg",
        "cover.jpeg",
        "cover.png",
        "cover.webp",
        "folder.jpg",
        "folder.png",
    ]
    .iter()
    .find_map(|n| std::fs::read(dir.join(n)).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_library_files_yield_art() {
        let dir = std::env::var("HOME").map(|h| format!("{h}/Music")).unwrap();
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| matches!(x.to_lowercase().as_str(), "mp3" | "flac" | "ogg"))
            })
            .collect();
        if entries.is_empty() {
            return; // nothing to check against on this machine
        }
        let with_art = entries.iter().filter(|e| load_art(&e.path()).is_some()).count();
        assert!(with_art > 0, "expected embedded art in at least one file");
    }

    #[test]
    fn sibling_fallback_and_thumbnail_size() {
        let dir = std::env::temp_dir().join("tm_art_test");
        let _ = std::fs::create_dir_all(&dir);
        // 800x400 red png as fake cover
        let img = DynamicImage::new_rgb8(800, 400);
        img.save(dir.join("cover.png")).unwrap();
        let art = load_art(&dir.join("track.mp3")).unwrap();
        assert!(art.width() <= 512 && art.height() <= 512);
        assert!(load_art(&dir.join("missing").join("x.mp3")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
