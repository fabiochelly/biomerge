//! The block filter: keeps the blocks whose lines contain what was typed.
//!
//! As in csvfab, case and accents are ignored unless asked for. Without regular expressions the
//! query is a list of words that a block must all contain, in any order and on any line, on
//! either side. With them, the whole query is one expression.

use std::borrow::Cow;
use std::ops::Range;

use regex::Regex;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FilterOptions {
    pub match_case: bool,
    pub regex: bool,
    pub whole_words: bool,
}

#[derive(Debug)]
pub struct Filter {
    patterns: Vec<Regex>,
    folded: bool,
}

/// One letter for one letter, so that positions can be mapped back.
fn fold_char(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => 'a',
        'ç' | 'Ç' => 'c',
        'è' | 'é' | 'ê' | 'ë' | 'È' | 'É' | 'Ê' | 'Ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'Ì' | 'Í' | 'Î' | 'Ï' => 'i',
        'ñ' | 'Ñ' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'Ù' | 'Ú' | 'Û' | 'Ü' => 'u',
        'ý' | 'ÿ' | 'Ý' => 'y',
        other => other,
    }
}

fn fold(text: &str) -> String {
    text.chars().map(fold_char).collect()
}

/// A folded text, and for each of its bytes the byte of the original it comes from.
fn fold_with_map(text: &str) -> (String, Vec<u32>) {
    let mut folded = String::with_capacity(text.len());
    let mut map = Vec::with_capacity(text.len() + 1);
    for (index, c) in text.char_indices() {
        let start = folded.len();
        folded.push(fold_char(c));
        map.extend(std::iter::repeat_n(index as u32, folded.len() - start));
    }
    map.push(text.len() as u32);
    (folded, map)
}

fn word_pattern(word: &str, whole: bool) -> String {
    let body = regex::escape(word);
    if !whole {
        return body;
    }
    let is_word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let before = if is_word(word.chars().next()) { r"\b" } else { "" };
    let after = if is_word(word.chars().next_back()) { r"\b" } else { "" };
    format!("{before}{body}{after}")
}

impl Filter {
    /// `None` for an empty query. An invalid regular expression is an error to show.
    pub fn new(query: &str, options: FilterOptions) -> Result<Option<Self>, String> {
        Self::build(query.trim(), options, false)
    }

    /// The same options, for finding in a text: without regular expressions the query is one
    /// phrase, spaces included, instead of words to find in any order.
    pub fn phrase(query: &str, options: FilterOptions) -> Result<Option<Self>, String> {
        Self::build(query, options, true)
    }

    fn build(query: &str, options: FilterOptions, phrase: bool) -> Result<Option<Self>, String> {
        if query.trim().is_empty() {
            return Ok(None);
        }
        let folded = !options.match_case;
        let query: Cow<'_, str> = if folded { Cow::Owned(fold(query)) } else { Cow::Borrowed(query) };
        let sources: Vec<String> = if options.regex {
            vec![if options.whole_words {
                format!(r"\b(?:{query})\b")
            } else {
                query.into_owned()
            }]
        } else if phrase {
            vec![word_pattern(&query, options.whole_words)]
        } else {
            query.split_whitespace().map(|word| word_pattern(word, options.whole_words)).collect()
        };
        let patterns = sources
            .iter()
            .map(|source| {
                let flagged = if folded { format!("(?i){source}") } else { source.clone() };
                Regex::new(&flagged).map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(Self { patterns, folded }))
    }

    /// True when every word is found on at least one of the lines.
    pub fn matches_lines<'a>(&self, lines: impl Iterator<Item = &'a str>) -> bool {
        let mut missing: Vec<&Regex> = self.patterns.iter().collect();
        for line in lines {
            let text: Cow<'_, str> = if self.folded && !line.is_ascii() {
                Cow::Owned(fold(line))
            } else {
                Cow::Borrowed(line)
            };
            missing.retain(|pattern| !pattern.is_match(&text));
            if missing.is_empty() {
                return true;
            }
        }
        false
    }

    /// The byte ranges of `text` to highlight, sorted and without overlap.
    pub fn find_ranges(&self, text: &str) -> Vec<Range<usize>> {
        let mapped = (self.folded && !text.is_ascii()).then(|| fold_with_map(text));
        let haystack = mapped.as_ref().map_or(text, |(folded, _)| folded.as_str());
        let mut ranges: Vec<Range<usize>> = self
            .patterns
            .iter()
            .flat_map(|pattern| pattern.find_iter(haystack))
            .filter(|found| !found.is_empty())
            .map(|found| match &mapped {
                Some((_, map)) => map[found.start()] as usize..map[found.end()] as usize,
                None => found.range(),
            })
            .collect();
        ranges.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
        for range in ranges {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
        }
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(query: &str, options: FilterOptions) -> Filter {
        Filter::new(query, options).unwrap().unwrap()
    }

    fn plain(query: &str) -> Filter {
        filter(query, FilterOptions::default())
    }

    #[test]
    fn an_empty_query_filters_nothing() {
        assert!(Filter::new("   ", FilterOptions::default()).unwrap().is_none());
    }

    #[test]
    fn every_word_must_be_in_the_block_on_any_line_in_any_order() {
        let f = plain("gamma alpha");
        assert!(f.matches_lines(["alpha beta", "x gamma"].into_iter()));
        assert!(!f.matches_lines(["alpha beta"].into_iter()));
    }

    #[test]
    fn case_and_accents_are_ignored_by_default() {
        assert!(plain("FACTURE").matches_lines(["la facture"].into_iter()));
        assert!(plain("créée").matches_lines(["CREEE"].into_iter()));
        assert!(plain("creee").matches_lines(["a été créée"].into_iter()));
    }

    #[test]
    fn matching_case_and_accents_is_an_option() {
        let exact = FilterOptions { match_case: true, ..FilterOptions::default() };
        assert!(!filter("Facture", exact).matches_lines(["facture"].into_iter()));
        assert!(!filter("creee", exact).matches_lines(["créée"].into_iter()));
        assert!(filter("créée", exact).matches_lines(["créée"].into_iter()));
    }

    #[test]
    fn whole_words_do_not_match_inside_a_word() {
        let whole = FilterOptions { whole_words: true, ..FilterOptions::default() };
        assert!(filter("line", whole).matches_lines(["the line is"].into_iter()));
        assert!(!filter("line", whole).matches_lines(["getLineData"].into_iter()));
        assert!(!filter("line", whole).matches_lines(["inline"].into_iter()));
        assert!(plain("line").matches_lines(["inline"].into_iter()));
    }

    #[test]
    fn whole_words_work_with_symbols() {
        let whole = FilterOptions { whole_words: true, ..FilterOptions::default() };
        assert!(filter("$total", whole).matches_lines(["echo $total;"].into_iter()));
    }

    #[test]
    fn symbols_are_literal_without_regex() {
        assert!(plain("a.b").matches_lines(["a.b"].into_iter()));
        assert!(!plain("a.b").matches_lines(["axb"].into_iter()));
    }

    #[test]
    fn a_regular_expression_is_one_pattern() {
        let regex = FilterOptions { regex: true, ..FilterOptions::default() };
        assert!(filter(r"foo\d+ bar", regex).matches_lines(["foo12 bar"].into_iter()));
        assert!(!filter(r"foo\d+ bar", regex).matches_lines(["foo12", "bar"].into_iter()));
    }

    #[test]
    fn an_invalid_regular_expression_is_reported() {
        let regex = FilterOptions { regex: true, ..FilterOptions::default() };
        assert!(Filter::new("(", regex).is_err());
        assert!(Filter::new("(", FilterOptions::default()).is_ok());
    }

    #[test]
    fn found_text_maps_back_to_the_original_bytes() {
        let f = plain("creee");
        let text = "a été créée ici";
        let ranges = f.find_ranges(text);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&text[ranges[0].clone()], "créée");
    }

    #[test]
    fn a_phrase_is_found_as_typed_and_not_word_by_word() {
        let phrase = Filter::phrase("int side", FilterOptions::default()).unwrap().unwrap();
        assert_eq!(phrase.find_ranges("side, INT side, int  side"), vec![6..14]);
        assert_eq!(plain("int side").find_ranges("side, int").len(), 2);
        assert!(Filter::phrase("  ", FilterOptions::default()).unwrap().is_none());
        let whole = FilterOptions { whole_words: true, ..FilterOptions::default() };
        let phrase = Filter::phrase("int side", whole).unwrap().unwrap();
        assert!(phrase.find_ranges("print sides").is_empty());
    }

    #[test]
    fn found_ranges_are_sorted_and_merged() {
        let f = plain("bar foo foobar");
        assert_eq!(f.find_ranges("xx foobar yy foo"), vec![3..9, 13..16]);
    }
}
