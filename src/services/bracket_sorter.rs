//! Bracket seeding (16/32 entrants) used by the operator dashboard.
//!
//! Port of `services/bracketSorter.ts`: entries are `"<number> - <name>"` lines, seeded by
//! number descending, then paired with the standard 16/32 bracket layouts.

// Consumed by the web dashboard only.
#![allow(dead_code)]

use std::cmp::Ordering;
use std::collections::HashSet;

use anyhow::{anyhow, bail};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BracketEntry {
    pub original_number: i64,
    pub name: String,
    pub original_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeededBracketEntry {
    pub original_number: i64,
    pub name: String,
    pub original_index: usize,
    pub seed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BracketMatch {
    pub match_number: usize,
    pub a: String,
    pub b: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BracketSortResult {
    /// 16 or 32.
    pub size: u32,
    pub entries: Vec<SeededBracketEntry>,
    pub matches: Vec<BracketMatch>,
}

const LAYOUT_16: [(usize, usize); 8] = [(1, 16), (8, 9), (5, 12), (4, 13), (6, 11), (3, 14), (7, 10), (2, 15)];

const LAYOUT_32: [(usize, usize); 16] = [
    (1, 32),
    (16, 17),
    (8, 25),
    (9, 24),
    (5, 28),
    (12, 21),
    (4, 29),
    (13, 20),
    (6, 27),
    (11, 22),
    (3, 30),
    (14, 19),
    (7, 26),
    (10, 23),
    (2, 31),
    (15, 18),
];

static BRACKET_LINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*(\d+)\s*-\s*(.+?)\s*$").unwrap());

fn layout(size: u32) -> &'static [(usize, usize)] {
    if size == 32 {
        &LAYOUT_32
    } else {
        &LAYOUT_16
    }
}

pub fn parse_bracket_input(raw_input: &str, size: u32) -> anyhow::Result<Vec<BracketEntry>> {
    let lines: Vec<&str> = raw_input.lines().map(str::trim).filter(|l| !l.is_empty()).collect();

    if lines.len() != size as usize {
        bail!("Expected exactly {size} entries, received {}.", lines.len());
    }

    let mut entries = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        let invalid = || anyhow!("Invalid line {}. Expected format: \"1014 - Beatrice [Re:Zero]\".", index + 1);
        let caps = BRACKET_LINE.captures(line).ok_or_else(invalid)?;
        let original_number: i64 = caps[1].parse().map_err(|_| invalid())?;
        let name = caps[2].trim().to_string();
        if name.is_empty() {
            bail!("Line {} is missing a character name.", index + 1);
        }
        entries.push(BracketEntry { original_number, name, original_index: index });
    }

    let mut seen = HashSet::new();
    for entry in &entries {
        if !seen.insert(entry.name.to_lowercase()) {
            bail!("Duplicate character name detected: \"{}\".", entry.name);
        }
    }

    Ok(entries)
}

/// Approximation of `localeCompare(..., { sensitivity: 'base' })`: case and common accents are
/// ignored, and punctuation sorts before digits, which sort before letters.
fn collation_key(name: &str) -> Vec<(u8, char)> {
    name.chars()
        .flat_map(|c| c.to_lowercase())
        .map(fold_accent)
        .map(|c| {
            let class = if c.is_alphabetic() {
                2
            } else if c.is_numeric() {
                1
            } else {
                0
            };
            (class, c)
        })
        .collect()
}

fn fold_accent(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
        'ç' | 'ć' | 'č' => 'c',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ę' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
        'ñ' | 'ń' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'ū' => 'u',
        'ý' | 'ÿ' => 'y',
        'š' | 'ś' => 's',
        'ž' | 'ź' | 'ż' => 'z',
        other => other,
    }
}

fn compare_entries(left: &BracketEntry, right: &BracketEntry) -> Ordering {
    right
        .original_number
        .cmp(&left.original_number)
        .then_with(|| collation_key(&left.name).cmp(&collation_key(&right.name)))
        .then_with(|| left.original_index.cmp(&right.original_index))
}

pub fn sort_bracket_entries(entries: &[BracketEntry]) -> Vec<SeededBracketEntry> {
    let mut sorted = entries.to_vec();
    sorted.sort_by(compare_entries);
    sorted
        .into_iter()
        .enumerate()
        .map(|(index, entry)| SeededBracketEntry {
            original_number: entry.original_number,
            name: entry.name,
            original_index: entry.original_index,
            seed: index + 1,
        })
        .collect()
}

pub fn build_bracket_matches(entries: &[SeededBracketEntry], size: u32) -> anyhow::Result<Vec<BracketMatch>> {
    layout(size)
        .iter()
        .enumerate()
        .map(|(index, (seed_a, seed_b))| {
            let a = entries.iter().find(|e| e.seed == *seed_a);
            let b = entries.iter().find(|e| e.seed == *seed_b);
            match (a, b) {
                (Some(a), Some(b)) => Ok(BracketMatch { match_number: index + 1, a: a.name.clone(), b: b.name.clone() }),
                _ => Err(anyhow!("Bracket seeding failed. Missing seeded entrant.")),
            }
        })
        .collect()
}

/// `size` is normalised like the web endpoint did: anything other than 32 means 16.
pub fn sort_bracket(raw_input: &str, size: u32) -> anyhow::Result<BracketSortResult> {
    let size = if size == 32 { 32 } else { 16 };
    let parsed = parse_bracket_input(raw_input, size)?;
    let entries = sort_bracket_entries(&parsed);
    let matches = build_bracket_matches(&entries, size)?;
    Ok(BracketSortResult { size, entries, matches })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(count: usize) -> String {
        (0..count).map(|i| format!("{} - Name {i}", 1000 + i)).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn rejects_wrong_entry_count() {
        let err = sort_bracket(&input(15), 16).unwrap_err().to_string();
        assert_eq!(err, "Expected exactly 16 entries, received 15.");
        let err = sort_bracket(&input(16), 32).unwrap_err().to_string();
        assert_eq!(err, "Expected exactly 32 entries, received 16.");
    }

    #[test]
    fn rejects_bad_line_and_reports_one_based_index() {
        let mut lines: Vec<String> = (0..16).map(|i| format!("{} - N{i}", i + 1)).collect();
        lines[4] = "no number here".into();
        let err = sort_bracket(&lines.join("\n"), 16).unwrap_err().to_string();
        assert_eq!(err, "Invalid line 5. Expected format: \"1014 - Beatrice [Re:Zero]\".");
    }

    #[test]
    fn rejects_duplicate_names_case_insensitively() {
        let mut lines: Vec<String> = (0..16).map(|i| format!("{} - N{i}", i + 1)).collect();
        lines[7] = "99 - n3".into();
        let err = sort_bracket(&lines.join("\r\n"), 16).unwrap_err().to_string();
        assert_eq!(err, "Duplicate character name detected: \"n3\".");
    }

    #[test]
    fn ignores_blank_lines_and_trims() {
        let mut text = String::from("\n   \n");
        text.push_str(&input(16));
        text.push_str("\n\n");
        assert!(sort_bracket(&text, 16).is_ok());
    }

    #[test]
    fn parses_names_with_dashes_and_brackets() {
        let mut lines: Vec<String> = (0..16).map(|i| format!("{} - N{i}", i + 1)).collect();
        lines[0] = "  1014 -  Beatrice [Re:Zero - S2]  ".into();
        let entries = parse_bracket_input(&lines.join("\n"), 16).unwrap();
        assert_eq!(entries[0].original_number, 1014);
        assert_eq!(entries[0].name, "Beatrice [Re:Zero - S2]");
        assert_eq!(entries[0].original_index, 0);
    }

    #[test]
    fn seeds_by_number_descending_with_stable_tiebreaks() {
        let entries = vec![
            BracketEntry { original_number: 5, name: "Zed".into(), original_index: 0 },
            BracketEntry { original_number: 9, name: "Low".into(), original_index: 1 },
            BracketEntry { original_number: 5, name: "alpha".into(), original_index: 2 },
            BracketEntry { original_number: 5, name: "Beta".into(), original_index: 3 },
        ];
        let seeded = sort_bracket_entries(&entries);
        let names: Vec<_> = seeded.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Low", "alpha", "Beta", "Zed"]);
        let seeds: Vec<_> = seeded.iter().map(|e| e.seed).collect();
        assert_eq!(seeds, vec![1, 2, 3, 4]);
    }

    #[test]
    fn identical_numbers_and_names_keep_input_order() {
        let a = BracketEntry { original_number: 1, name: "Same".into(), original_index: 0 };
        let b = BracketEntry { original_number: 1, name: "SAME".into(), original_index: 1 };
        let seeded = sort_bracket_entries(&[b.clone(), a.clone()]);
        assert_eq!(seeded[0].original_index, 0);
        assert_eq!(seeded[1].original_index, 1);
    }

    #[test]
    fn sixteen_bracket_layout() {
        // numbers 16..1 so that seed N is the entry named "sN"
        let text = (1..=16).map(|s| format!("{} - s{s}", 17 - s)).collect::<Vec<_>>().join("\n");
        let result = sort_bracket(&text, 16).unwrap();
        assert_eq!(result.size, 16);
        assert_eq!(result.entries.len(), 16);
        assert_eq!(result.matches.len(), 8);
        assert_eq!(result.matches[0], BracketMatch { match_number: 1, a: "s1".into(), b: "s16".into() });
        assert_eq!(result.matches[1], BracketMatch { match_number: 2, a: "s8".into(), b: "s9".into() });
        assert_eq!(result.matches[7], BracketMatch { match_number: 8, a: "s2".into(), b: "s15".into() });
    }

    #[test]
    fn thirty_two_bracket_layout() {
        let text = (1..=32).map(|s| format!("{} - s{s}", 33 - s)).collect::<Vec<_>>().join("\n");
        let result = sort_bracket(&text, 32).unwrap();
        assert_eq!(result.size, 32);
        assert_eq!(result.matches.len(), 16);
        assert_eq!(result.matches[0].a, "s1");
        assert_eq!(result.matches[0].b, "s32");
        assert_eq!(result.matches[1].a, "s16");
        assert_eq!(result.matches[1].b, "s17");
        assert_eq!(result.matches[15].a, "s15");
        assert_eq!(result.matches[15].b, "s18");
        // every entrant appears exactly once
        let mut all: Vec<_> = result.matches.iter().flat_map(|m| [m.a.clone(), m.b.clone()]).collect();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 32);
    }

    #[test]
    fn unknown_size_means_sixteen_and_json_shape() {
        let result = sort_bracket(&input(16), 7).unwrap();
        assert_eq!(result.size, 16);
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["size"], 16);
        assert!(json["entries"][0]["originalNumber"].is_number());
        assert!(json["entries"][0]["originalIndex"].is_number());
        assert!(json["entries"][0]["seed"].is_number());
        assert_eq!(json["matches"][0]["matchNumber"], 1);
        assert!(json["matches"][0]["a"].is_string());
    }
}
