use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feedback {
    pub song_id: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlgorithmState {
    pub song_scores: HashMap<String, f32>,
    pub artist_scores: HashMap<String, f32>,
    pub genre_scores: HashMap<String, f32>,
    pub feedback_history: Vec<Feedback>,
    pub recent_artists: Vec<String>,
    pub recent_genres: Vec<String>,
}

impl Default for AlgorithmState {
    fn default() -> Self {
        Self {
            song_scores: HashMap::new(),
            artist_scores: HashMap::new(),
            genre_scores: HashMap::new(),
            feedback_history: Vec::new(),
            recent_artists: Vec::new(),
            recent_genres: Vec::new(),
        }
    }
}

impl AlgorithmState {
    pub fn load(path: &str) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &str) {
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, s);
        }
    }

    fn push_recent(v: &mut Vec<String>, val: &str) {
        v.retain(|x| x != val);
        v.push(val.to_string());
        if v.len() > 3 {
            v.remove(0);
        }
    }

    /// action: love (+50/+20), skip (-15/-5), complete (+20/+5), dislike (-40/-20), next (no score)
    pub fn apply(&mut self, action: &str, song_id: &str, artist: &str, genre: &str) {
        let (ds, da) = match action {
            "love" => (50.0, 20.0),
            "skip" => (-15.0, -5.0),
            "complete" => (20.0, 5.0),
            "dislike" => (-40.0, -20.0),
            _ => (0.0, 0.0),
        };
        if ds != 0.0 {
            *self.song_scores.entry(song_id.to_string()).or_insert(0.0) += ds;
            *self.artist_scores.entry(artist.to_string()).or_insert(0.0) += da;
            self.feedback_history.push(Feedback {
                song_id: song_id.to_string(),
                action: action.to_string(),
            });
        }
        Self::push_recent(&mut self.recent_artists, artist);
        Self::push_recent(&mut self.recent_genres, genre);
    }
}

#[derive(Debug, Clone)]
pub struct Song {
    pub id: String, // stable: full path string
    pub title: String,
    pub artist: String,
    pub genre: String,
    pub path: PathBuf,
}

/// Score = Base(100 + learned) x Artist_Factor x Genre_Factor
pub fn effective_score(song: &Song, st: &AlgorithmState) -> f32 {
    let learned = st.song_scores.get(&song.id).copied().unwrap_or(0.0);
    let base = (100.0 + learned).max(1.0);
    let af = if st.recent_artists.contains(&song.artist) {
        0.5
    } else {
        1.0
    };
    let gf = if st.recent_genres.contains(&song.genre) {
        0.7
    } else {
        1.0
    };
    base * af * gf
}

/// Weighted-random pick, excluding `exclude_id`. Returns index.
pub fn pick_next(
    songs: &[Song],
    st: &AlgorithmState,
    exclude_id: Option<&str>,
) -> Option<usize> {
    let mut cands: Vec<(usize, f32)> = songs
        .iter()
        .enumerate()
        .filter(|(_, s)| Some(s.id.as_str()) != exclude_id)
        .map(|(i, s)| (i, effective_score(s, st)))
        .collect();
    if cands.is_empty() {
        return songs.iter().position(|_| true).filter(|_| !songs.is_empty());
    }
    // If everything got buried at min score, fall back to uniform.
    let total: f32 = cands.iter().map(|(_, w)| w).sum();
    if total <= 0.0 {
        use rand::seq::IndexedRandom;
        let mut rng = rand::rng();
        return cands.choose(&mut rng).map(|(i, _)| *i);
    }
    let mut roll = rand::random::<f32>() * total;
    for (i, w) in cands.drain(..) {
        roll -= w;
        if roll <= 0.0 {
            return Some(i);
        }
    }
    Some(songs.len() - 1)
}

/// Scan dir for audio files. `Artist - Title.ext` -> parse, else title=stem.
/// Genre = parent folder name, else Unknown.
pub fn scan_library(dir: &str) -> Vec<Song> {
    let mut out = Vec::new();
    let entries = fs::read_dir(dir).into_iter().flatten().flatten();
    for e in entries {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let ext = p
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !matches!(ext.as_str(), "mp3" | "wav" | "flac" | "ogg") {
            continue;
        }
        let stem = p
            .file_stem()
            .and_then(|x| x.to_str())
            .unwrap_or("Unknown")
            .to_string();
        let (artist, title) = stem
            .split_once(" - ")
            .map(|(a, t)| (a.trim().to_string(), t.trim().to_string()))
            .unwrap_or(("Unknown".to_string(), stem.clone()));
        let genre = p
            .parent()
            .and_then(|par| {
                let name = par.file_name()?.to_str()?.to_string();
                if name == "." || name.is_empty() || dir.contains(&name) {
                    None
                } else {
                    Some(name)
                }
            })
            .unwrap_or("Unknown".to_string());
        out.push(Song {
            id: p.to_string_lossy().to_string(),
            title,
            artist,
            genre,
            path: p,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diversity_penalties_and_learning() {
        let s = |id: &str, ar: &str, g: &str| Song {
            id: id.into(),
            title: id.into(),
            artist: ar.into(),
            genre: g.into(),
            path: id.into(),
        };
        let mut st = AlgorithmState::default();
        let a = s("a", "A1", "G1");
        assert!((effective_score(&a, &st) - 100.0).abs() < 0.01);
        st.apply("love", "a", "A1", "G1"); // recent = A1/G1
        // same artist+genre now penalized: (100+50)*0.5*0.7 = 52.5
        assert!((effective_score(&a, &st) - 52.5).abs() < 0.01);
        st.apply("dislike", "a", "A1", "G1"); // 100+50-40=110
        assert!((effective_score(&a, &st) - 110.0 * 0.5 * 0.7).abs() < 0.01);
        // pick excludes current
        let songs = vec![a, s("b", "A2", "G2")];
        assert_eq!(pick_next(&songs, &st, Some("a")), Some(1));
    }

    #[test]
    fn scan_and_state_roundtrip() {
        let dir = std::env::temp_dir().join("tm_scan_test");
        let _ = std::fs::create_dir_all(&dir);
        for f in ["Adele - Hello.mp3", "notes.txt"] {
            let _ = std::fs::write(dir.join(f), b"x");
        }
        let songs = scan_library(dir.to_str().unwrap());
        assert_eq!(songs.len(), 1); // .txt ignored
        assert_eq!(songs[0].artist, "Adele");
        assert_eq!(songs[0].title, "Hello");
        let mut st = AlgorithmState::default();
        st.apply("love", &songs[0].id, &songs[0].artist, &songs[0].genre);
        let fp = std::env::temp_dir().join("tm_state_test.json");
        st.save(fp.to_str().unwrap());
        let re = AlgorithmState::load(fp.to_str().unwrap());
        assert_eq!(re.song_scores[&songs[0].id], 50.0);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&fp);
    }
}
