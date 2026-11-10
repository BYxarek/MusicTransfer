use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

use crate::{chrome::Candidate, report::Track};

static VERSIONS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        "remix|ремикс|bootleg|mashup",
        "cover|кавер",
        "live|лайв|концерт",
        "acoustic|акустика",
        "instrumental|инструментал|karaoke|караоке",
        "slowed|slow version|замедленная версия",
        "sped up|speed up|nightcore|ускоренная версия",
        "remaster(?:ed)?|ремастер",
        "radio edit|extended mix",
    ]
    .map(|pattern| Regex::new(&format!(r"\b(?:{pattern})\b")).unwrap())
    .into()
});
static TITLE_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(\.(?:mp3|wav|flac)|\s*[\[(]\s*(?:feat\.?|ft\.?|featuring)\s+[^)\]]+[)\]])\s*$",
    )
    .unwrap()
});
static CREDITS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[,;]|\s+(?:&|and|x|и)\s+").unwrap());
static PAIR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+[-–—−]\s+|\s*[:|–—−]\s*").unwrap());

pub fn version_tags(title: &str) -> Vec<&'static str> {
    let normalized = normalize(title);
    VERSIONS
        .iter()
        .zip([
            "Ремикс",
            "Кавер",
            "Концертная запись",
            "Акустика",
            "Инструментал / караоке",
            "Замедленная версия",
            "Ускоренная версия",
            "Ремастер",
            "Альтернативная версия",
        ])
        .filter_map(|(marker, label)| marker.is_match(&normalized).then_some(label))
        .collect()
}

fn normalize(text: &str) -> String {
    let text: String = text
        .nfkc()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character == 'ё' {
                'е'
            } else if character.is_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn title(text: &str) -> String {
    let clean = TITLE_SUFFIX.replace(text, "");
    normalize(&TITLE_SUFFIX.replace(&clean, ""))
}

fn exact(text: &str) -> String {
    text.nfc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn exact_match(track: &Track, candidates: &[Candidate]) -> Option<usize> {
    let [artist] = track.artists.as_slice() else {
        return None;
    };
    let artist = exact(artist);
    let title = exact(&track.title);
    let mut hits = candidates.iter().enumerate().filter(|(_, candidate)| {
        exact(&candidate.artist) == artist && exact(&candidate.title) == title
    });
    let first = hits.next()?.0;
    hits.next().is_none().then_some(first)
}

fn similarity(left: &str, right: &str) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    if left == right {
        return 1.0;
    }
    let left: Vec<_> = left.chars().collect();
    let right: Vec<_> = right.chars().collect();
    let mut ranges = vec![(left.as_slice(), right.as_slice())];
    let mut matched = 0;
    while let Some((left, right)) = ranges.pop() {
        let mut lengths = vec![0; right.len() + 1];
        let mut best = (0, 0, 0);
        for (i, &a) in left.iter().enumerate() {
            let mut diagonal = 0;
            for (j, &b) in right.iter().enumerate() {
                let length = if a == b { diagonal + 1 } else { 0 };
                diagonal = lengths[j + 1];
                lengths[j + 1] = length;
                if length > best.2 {
                    best = (i + 1 - length, j + 1 - length, length);
                }
            }
        }
        let (i, j, length) = best;
        if length == 0 {
            continue;
        }
        matched += length;
        if i > 0 && j > 0 {
            ranges.push((&left[..i], &right[..j]));
        }
        if i + length < left.len() && j + length < right.len() {
            ranges.push((&left[i + length..], &right[j + length..]));
        }
    }
    (2 * matched) as f64 / (left.len() + right.len()) as f64
}

pub fn closest(track: &Track, candidates: &[Candidate], relaxed: bool) -> Option<usize> {
    let source_title = title(&track.title);
    let source_versions = normalize(&track.title);
    let artists = track
        .artists
        .iter()
        .map(|artist| normalize(artist))
        .collect::<Vec<_>>();
    let title_score = |value: &str| {
        let candidate = title(value);
        if !source_title.is_empty() && candidate == source_title {
            1.0
        } else if source_title.chars().count() >= 3
            && format!(" {candidate} ").contains(&format!(" {source_title} "))
        {
            0.96
        } else {
            similarity(&source_title, &candidate)
        }
    };
    let artist_score = |value: &str| {
        std::iter::once(value)
            .chain(CREDITS.split(value))
            .map(normalize)
            .flat_map(|credit| {
                artists
                    .iter()
                    .map(move |artist| similarity(&credit, artist))
            })
            .fold(0.0_f64, f64::max)
    };
    let mut best = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let mut title = title_score(&candidate.title);
        let mut artist = artist_score(&candidate.artist);
        if let Some(split) = PAIR.find(&candidate.title) {
            let (left, right) = (
                &candidate.title[..split.start()],
                &candidate.title[split.end()..],
            );
            for (credit, name) in [(left, right), (right, left)] {
                let score = artist_score(credit);
                if score >= 0.85 {
                    artist = artist.max(score);
                    title = title.max(title_score(name));
                }
            }
        }
        let candidate_versions = normalize(&candidate.title);
        let same_version = VERSIONS.iter().all(|marker| {
            marker.is_match(&source_versions) == marker.is_match(&candidate_versions)
        });
        let accepted = if relaxed {
            (source_title
                .chars()
                .filter(|character| *character != ' ')
                .count()
                >= 4
                && title >= 0.96)
                || (title >= 0.70 && artist >= 0.85)
        } else {
            title >= 0.86 && artist >= 0.85 && same_version
        };
        let score = 0.76 * title + 0.24 * artist - if same_version { 0.0 } else { 0.2 };
        if accepted && best.is_none_or(|(_, previous)| score > previous) {
            best = Some((index, score));
        }
    }
    best.map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reuploads_and_version_markers() {
        let track =
            serde_json::from_value(json!({"title": "Масть крестовая", "artists": ["Михаил Круг"]}))
                .unwrap();
        let candidates: Vec<Candidate> = serde_json::from_value(json!([
            {"index": 1, "artist": "Михаил Круг", "title": "Масть крестовая (Remix)"},
            {"index": 2, "artist": "Uploader", "title": "Михаил Круг - Масть крестовая (Official Audio)"},
        ])).unwrap();
        assert_eq!(closest(&track, &candidates, false), Some(1));
        assert_eq!(closest(&track, &candidates[..1], false), None);
        assert_eq!(closest(&track, &candidates[..1], true), Some(0));
        assert_eq!(exact_match(&track, &candidates), None);
    }
}
