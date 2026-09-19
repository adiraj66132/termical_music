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
    /// Fresh each launch: songs already heard this session (×0.3, excluded when possible).
    #[serde(skip)]
    pub session_played: Vec<String>,
    /// song_id -> total_plays index when it becomes eligible again (dislike cooldown).
    #[serde(default)]
    pub quarantine: HashMap<String, u64>,
    #[serde(default)]
    pub total_plays: u64,
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
            session_played: Vec::new(),
            quarantine: HashMap::new(),
            total_plays: 0,
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

    /// action: love (+50/+20), skip (early -25 / mid -15 / late +10),
    /// complete (+20/+5), dislike (-40/-20), next (no score).
    /// progress: fraction of track elapsed (0.0-1.0); only skip uses it.
    pub fn apply(&mut self, action: &str, song_id: &str, artist: &str, genre: &str, progress: f32) {
        let (ds, da) = match action {
            "love" => (50.0, 20.0),
            "skip" if progress >= 0.8 => (10.0, 3.0),
            "skip" if progress < 0.3 => (-25.0, -8.0),
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

    /// Call on every track start: session-no-repeat bookkeeping + quarantine expiry.
    pub fn note_played(&mut self, song_id: &str) {
        self.total_plays += 1;
        if !self.session_played.iter().any(|x| x == song_id) {
            self.session_played.push(song_id.to_string());
            if self.session_played.len() > 20 {
                self.session_played.remove(0);
            }
        }
        let now = self.total_plays;
        self.quarantine.retain(|_, release| *release > now);
    }

    /// Disliked songs sit out the next 10 tracks (survives restarts).
    pub fn quarantine_song(&mut self, song_id: &str) {
        self.quarantine.insert(song_id.to_string(), self.total_plays + 10);
    }

    fn quarantined(&self, song_id: &str) -> bool {
        self.quarantine.get(song_id).is_some_and(|rel| self.total_plays < *rel)
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

/// Score = Base(100 + learned) x Artist_Factor x Genre_Factor x Session_Factor
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
    let sf = if st.session_played.iter().any(|x| x == &song.id) {
        0.3
    } else {
        1.0
    };
    base * af * gf * sf
}

/// Weighted-random pick. Excludes `exclude_id`, quarantined songs, and —
/// when others remain — songs already heard this session. Never dead-ends:
/// falls back to quarantine, then to anything non-empty.
pub fn pick_next(
    songs: &[Song],
    st: &AlgorithmState,
    exclude_id: Option<&str>,
) -> Option<usize> {
    if songs.is_empty() {
        return None;
    }
    let eligible = |s: &Song| Some(s.id.as_str()) != exclude_id && !st.quarantined(&s.id);
    let mut cands: Vec<(usize, f32)> = songs
        .iter()
        .enumerate()
        .filter(|(_, s)| eligible(s))
        .map(|(i, s)| (i, effective_score(s, st)))
        .collect();
    if cands.is_empty() {
        // quarantine would starve us: ignore it, keep only the current excluded
        cands = songs
            .iter()
            .enumerate()
            .filter(|(_, s)| Some(s.id.as_str()) != exclude_id)
            .map(|(i, s)| (i, effective_score(s, st)))
            .collect();
    }
    if cands.is_empty() {
        return Some(0); // single-song library (or only the current exists)
    }
    if cands.len() > 1 {
        let fresh: Vec<(usize, f32)> = cands
            .iter()
            .filter(|(i, _)| !st.session_played.iter().any(|x| x == &songs[*i].id))
            .copied()
            .collect();
        if !fresh.is_empty() {
            cands = fresh;
        }
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
        st.apply("love", "a", "A1", "G1", 1.0); // recent = A1/G1
        // same artist+genre now penalized: (100+50)*0.5*0.7 = 52.5
        assert!((effective_score(&a, &st) - 52.5).abs() < 0.01);
        st.apply("dislike", "a", "A1", "G1", 1.0); // 100+50-40=110
        assert!((effective_score(&a, &st) - 110.0 * 0.5 * 0.7).abs() < 0.01);
        // pick excludes current
        let songs = vec![a, s("b", "A2", "G2")];
        assert_eq!(pick_next(&songs, &st, Some("a")), Some(1));
    }

    #[test]
    fn skip_progress_tiers() {
        let mut st = AlgorithmState::default();
        st.apply("skip", "early", "A", "G", 0.1);
        assert_eq!(st.song_scores["early"], -25.0);
        st.apply("skip", "mid", "A", "G", 0.5);
        assert_eq!(st.song_scores["mid"], -15.0);
        st.apply("skip", "late", "A", "G", 0.9);
        assert_eq!(st.song_scores["late"], 10.0); // nearly finished = liked
    }

    #[test]
    fn session_no_repeat_and_fallback() {
        let s = |id: &str| Song {
            id: id.into(), title: id.into(), artist: format!("A{id}"),
            genre: "G".into(), path: id.into(),
        };
        let songs = vec![s("a"), s("b"), s("c")];
        let mut st = AlgorithmState::default();
        st.note_played("a");
        st.note_played("b");
        // only the unplayed song is picked (deterministic: single fresh candidate)
        assert_eq!(pick_next(&songs, &st, Some("a")), Some(2));
        // played songs carry the 0.3 session factor (no recency here: 100*1.0*1.0*0.3)
        assert!((effective_score(&songs[0], &st) - 30.0).abs() < 0.01);
        // single-song library never dead-ends
        let one = vec![s("a")];
        assert_eq!(pick_next(&one, &st, Some("a")), Some(0));
    }

    #[test]
    fn dislike_quarantine_and_release() {
        let s = |id: &str| Song {
            id: id.into(), title: id.into(), artist: format!("A{id}"),
            genre: "G".into(), path: id.into(),
        };
        let songs = vec![s("a"), s("b")];
        let mut st = AlgorithmState::default();
        st.note_played("a");
        st.apply("dislike", "a", "Aa", "G", 1.0);
        st.quarantine_song("a");
        // quarantined: only b can be picked, even excluding nothing
        assert_eq!(pick_next(&songs, &st, None), Some(1));
        // survives a save/load round-trip
        let json = serde_json::to_string(&st).unwrap();
        let mut re: AlgorithmState = serde_json::from_str(&json).unwrap();
        assert!(re.quarantined("a"));
        assert!(re.session_played.is_empty()); // sessions don't persist
        // released after 10 more tracks
        for i in 0..10 {
            re.note_played(&format!("x{i}"));
        }
        assert!(!re.quarantined("a"));
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
        st.apply("love", &songs[0].id, &songs[0].artist, &songs[0].genre, 1.0);
        let fp = std::env::temp_dir().join("tm_state_test.json");
        st.save(fp.to_str().unwrap());
        let re = AlgorithmState::load(fp.to_str().unwrap());
        assert_eq!(re.song_scores[&songs[0].id], 50.0);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&fp);
    }
}
