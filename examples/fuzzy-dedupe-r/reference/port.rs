//! Reference port of find_duplicates (with levenshtein and normalize inside it): what a good
//! agent run produces. CI uses it to test the pipeline without an agent.

use crate::r::{List, Vector};

/// R's normalize(): tolower, split on runs of " \t\n\r", drop empty words, join with " ".
/// NA goes through paste() as the text "NA".
fn normalize(name: Option<&str>) -> Vec<char> {
    let Some(name) = name else { return "NA".chars().collect() };
    let mut out = Vec::with_capacity(name.len());
    for word in name.split([' ', '\t', '\n', '\r']).filter(|w| !w.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        // R's tolower maps one character to one character
        out.extend(word.chars().map(|c| {
            let mut lower = c.to_lowercase();
            if lower.len() == 1 { lower.next().unwrap_or(c) } else { c }
        }));
    }
    out
}

/// Edit distance, one row on the shorter side.
fn distance(a: &[char], b: &[char], row: &mut Vec<usize>) -> usize {
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    row.clear();
    row.extend(0..=short.len());
    for (i, ca) in long.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in short.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1).min(row[j] + 1).min(diagonal + usize::from(ca != cb));
            diagonal = above;
        }
    }
    row[short.len()]
}

pub fn find_duplicates(names: Vector<Option<String>>, max_distance: Option<f64>) -> Result<List<Vec<(i64, i64)>>, String> {
    let clean: Vec<Vec<char>> = names.iter().map(|n| normalize(n.as_deref())).collect();
    let mut pairs = Vec::new();
    if clean.len() < 2 {
        return Ok(List(pairs));
    }
    // levenshtein(...) <= NA is NA, and if (NA) stops
    let max = max_distance.ok_or("missing value where TRUE/FALSE needed")?;
    let mut row = Vec::new();
    for i in 0..clean.len() - 1 {
        for j in i + 1..clean.len() {
            if distance(&clean[i], &clean[j], &mut row) as f64 <= max {
                pairs.push((i as i64 + 1, j as i64 + 1));
            }
        }
    }
    Ok(List(pairs))
}
