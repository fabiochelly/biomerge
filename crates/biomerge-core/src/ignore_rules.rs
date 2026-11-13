//! Ignore rules: regular expressions naming the text whose differences are noise (dates,
//! generated identifiers, version numbers…).
//!
//! A rule either masks what it matches inside a line, or declares the whole matching line
//! unimportant. The rules only say *what* is ignored; [`crate::diff`] decides what that means
//! for rows and blocks.
//!
//! Every rule is matched against the raw line, before the whitespace and case options, so that
//! what the user reads is what the expression sees. The ignored text of a line is the union of
//! what each rule finds on its own: rules never hide text from one another, and their order
//! does not matter.

use std::ops::Range;

use regex::{Regex, RegexBuilder, RegexSet};

/// Stands for one run of ignored text in comparison keys. It is neither whitespace nor cased,
/// so the whitespace and case options leave it alone.
pub(crate) const PLACEHOLDER: char = '\u{1A}';

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum IgnoreScope {
    /// The matched text is ignored: lines equal once their matches are masked are equal.
    #[default]
    MatchedText,
    /// A line containing a match is ignored as a whole (anchor with `^…$` to match all of it).
    WholeLine,
}

/// One rule as the user edits it: plain data, to keep in the settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct IgnoreRule {
    pub pattern: String,
    pub scope: IgnoreScope,
    pub case_insensitive: bool,
    pub enabled: bool,
}

impl IgnoreRule {
    /// An enabled, case-sensitive rule ignoring the matched text.
    pub fn text(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            scope: IgnoreScope::MatchedText,
            case_insensitive: false,
            enabled: true,
        }
    }

    /// An enabled, case-sensitive rule ignoring the lines that contain a match.
    pub fn whole_line(pattern: impl Into<String>) -> Self {
        Self {
            scope: IgnoreScope::WholeLine,
            ..Self::text(pattern)
        }
    }

    /// The error to show next to this rule, whether it is enabled or not.
    pub fn check(&self) -> Result<(), String> {
        self.compile().map(drop)
    }

    /// An empty pattern would match everywhere: it is a rule still being typed, not a rule.
    fn is_active(&self) -> bool {
        self.enabled && !self.pattern.is_empty()
    }

    fn compile(&self) -> Result<Regex, String> {
        RegexBuilder::new(&self.pattern)
            .case_insensitive(self.case_insensitive)
            .build()
            .map_err(|error| error.to_string())
    }

    /// The pattern as a member of a set, where flags cannot be given per member.
    fn source(&self) -> String {
        if self.case_insensitive {
            format!("(?i){}", self.pattern)
        } else {
            self.pattern.clone()
        }
    }
}

/// The rules of one scope. With several of them, the set tells in one pass which ones are worth
/// running on a line: most lines match none.
#[derive(Clone, Debug, Default)]
struct Matcher {
    patterns: Vec<Regex>,
    set: Option<RegexSet>,
}

impl Matcher {
    fn new(rules: &[(&IgnoreRule, Regex)], scope: IgnoreScope) -> Self {
        let of_scope = || rules.iter().filter(move |(rule, _)| rule.scope == scope);
        let patterns: Vec<Regex> = of_scope().map(|(_, regex)| regex.clone()).collect();
        // A set too big to compile is only a lost shortcut: every pattern is then tried.
        let set = (patterns.len() > 1)
            .then(|| RegexSet::new(of_scope().map(|(rule, _)| rule.source())).ok())
            .flatten();
        Self { patterns, set }
    }

    fn is_match(&self, line: &str) -> bool {
        match (&self.set, self.patterns.as_slice()) {
            (_, []) => false,
            (_, [only]) => only.is_match(line),
            (Some(set), _) => set.is_match(line),
            (None, all) => all.iter().any(|pattern| pattern.is_match(line)),
        }
    }

    /// Sorted, non-empty, and merged when they overlap or touch.
    fn ranges(&self, line: &str) -> Vec<Range<usize>> {
        let mut ranges: Vec<Range<usize>> = Vec::new();
        let mut find = |pattern: &Regex| {
            ranges.extend(pattern.find_iter(line).filter(|found| !found.is_empty()).map(|found| found.range()));
        };
        match (&self.set, self.patterns.as_slice()) {
            (_, []) => return ranges,
            (_, [only]) => find(only),
            (Some(set), all) => set.matches(line).iter().for_each(|index| find(&all[index])),
            (None, all) => all.iter().for_each(find),
        }
        if ranges.len() < 2 {
            return ranges;
        }
        ranges.sort_unstable_by_key(|range| range.start);
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

/// Compiled rules, ready to compare with. Compile them when the rules change, not at each
/// comparison; cloning is cheap.
#[derive(Clone, Debug, Default)]
pub struct IgnoreRules {
    text: Matcher,
    lines: Matcher,
}

impl IgnoreRules {
    /// Compiles the enabled rules; disabled ones and empty patterns are skipped. An invalid
    /// regular expression is an error to show, naming the rule by its position in `rules`.
    pub fn new(rules: &[IgnoreRule]) -> Result<Self, String> {
        let mut compiled = Vec::new();
        for (index, rule) in rules.iter().enumerate().filter(|(_, rule)| rule.is_active()) {
            let regex = rule.compile().map_err(|error| {
                format!("Règle {} : expression régulière invalide.\n{error}", index + 1)
            })?;
            compiled.push((rule, regex));
        }
        Ok(Self {
            text: Matcher::new(&compiled, IgnoreScope::MatchedText),
            lines: Matcher::new(&compiled, IgnoreScope::WholeLine),
        })
    }

    /// True when nothing is ignored: comparing then costs exactly what it costs without rules.
    pub fn is_empty(&self) -> bool {
        self.text.patterns.is_empty() && self.lines.patterns.is_empty()
    }

    /// True when a whole-line rule matches somewhere in `line`.
    pub fn ignores_line(&self, line: &str) -> bool {
        self.lines.is_match(line)
    }

    /// The byte ranges of `line` matched by the text rules: sorted, non-empty, on character
    /// boundaries, and merged when they overlap or touch. Whole-line rules are not included.
    pub fn ignored_ranges(&self, line: &str) -> Vec<Range<usize>> {
        self.text.ranges(line)
    }
}

/// A ready-made pattern to offer in the interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IgnorePreset {
    pub name: &'static str,
    pub example: &'static str,
    pub pattern: &'static str,
}

impl IgnorePreset {
    /// An enabled rule ignoring the text this preset matches.
    pub fn rule(&self) -> IgnoreRule {
        IgnoreRule::text(self.pattern)
    }
}

/// The patterns are self-contained (no flag needed) and written for speed, as they run on every
/// line: ASCII classes rather than `\d`, which also matches the digits of every other script,
/// and ASCII word boundaries `(?-u:\b)` rather than `\b`, which sends every line holding an
/// accented letter to a matcher ten times slower.
pub const IGNORE_PRESETS: &[IgnorePreset] = &[
    IgnorePreset {
        name: "Date ISO",
        example: "2024-03-15",
        pattern: r"(?-u:\b)[0-9]{4}-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12][0-9]|3[01])(?-u:\b)",
    },
    IgnorePreset {
        name: "Date et heure ISO",
        example: "2024-03-15T10:30:00Z",
        pattern: r"(?-u:\b)[0-9]{4}-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12][0-9]|3[01])[T ](?:[01][0-9]|2[0-3]):[0-5][0-9](?::[0-5][0-9](?:[.,][0-9]+)?)?(?:Z|[+-][0-9]{2}(?::?[0-9]{2})?)?(?-u:\b)",
    },
    IgnorePreset {
        name: "Date jour/mois/année",
        example: "15/03/2024",
        pattern: r"(?-u:\b)(?:0?[1-9]|[12][0-9]|3[01])[/.-](?:0?[1-9]|1[0-2])[/.-](?:[0-9]{4}|[0-9]{2})(?-u:\b)",
    },
    IgnorePreset {
        name: "Date en toutes lettres",
        example: "15 mars 2024",
        pattern: r"(?i)(?-u:\b)(?:1er|0?[1-9]|[12][0-9]|3[01])\s+(?:janvier|f[ée]vrier|mars|avril|mai|juin|juillet|ao[uû]t|septembre|octobre|novembre|d[ée]cembre|janv|f[ée]vr|avr|juil|sept|oct|nov|d[ée]c)\.?\s+[0-9]{4}(?-u:\b)",
    },
    IgnorePreset {
        name: "Heure",
        example: "10:30:59",
        pattern: r"(?-u:\b)(?:[01]?[0-9]|2[0-3])(?::[0-5][0-9](?::[0-5][0-9](?:[.,][0-9]+)?)?|h[0-5][0-9])(?-u:\b)",
    },
    IgnorePreset {
        name: "UUID",
        example: "123e4567-e89b-12d3-a456-426614174000",
        pattern: r"(?-u:\b)[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}(?-u:\b)",
    },
    IgnorePreset {
        name: "Empreinte hexadécimale (MD5, SHA…)",
        example: "d41d8cd98f00b204e9800998ecf8427e",
        pattern: r"(?-u:\b)[0-9a-fA-F]{32,}(?-u:\b)",
    },
    IgnorePreset {
        name: "Numéro de version",
        example: "v1.12.3-beta.2",
        pattern: r"(?-u:\b)v?[0-9]+(?:\.[0-9]+){2,3}(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?(?:\+[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?",
    },
    IgnorePreset {
        name: "Nombre",
        example: "1234,56",
        pattern: r"(?-u:\b)[0-9]+(?:[.,][0-9]+)?(?-u:\b)",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(name: &str) -> Regex {
        let preset = IGNORE_PRESETS.iter().find(|preset| preset.name == name).unwrap();
        Regex::new(preset.pattern).unwrap()
    }

    /// The preset must match exactly the whole of each example.
    fn assert_matches(name: &str, examples: &[&str]) {
        let regex = preset(name);
        for example in examples {
            let found = regex.find(example).map(|found| found.as_str());
            assert_eq!(found, Some(*example), "{name} on {example:?}");
        }
    }

    fn assert_rejects(name: &str, examples: &[&str]) {
        let regex = preset(name);
        for example in examples {
            let found = regex.find(example).map(|found| found.as_str());
            assert_eq!(found, None, "{name} on {example:?}");
        }
    }

    #[test]
    fn every_preset_compiles_and_matches_its_own_example() {
        let mut names: Vec<&str> = IGNORE_PRESETS.iter().map(|preset| preset.name).collect();
        for preset in IGNORE_PRESETS {
            assert_eq!(preset.rule().check(), Ok(()), "{}", preset.name);
            assert_matches(preset.name, &[preset.example]);
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), IGNORE_PRESETS.len());
        let rules: Vec<IgnoreRule> = IGNORE_PRESETS.iter().map(IgnorePreset::rule).collect();
        assert!(!IgnoreRules::new(&rules).unwrap().is_empty());
    }

    #[test]
    fn the_iso_date_preset_wants_a_real_month_and_day() {
        assert_matches("Date ISO", &["2024-03-15", "1999-12-31", "2024-01-01"]);
        assert_rejects(
            "Date ISO",
            &["2024-13-15", "2024-00-10", "2024-03-32", "2024-3-15", "12024-03-15", "2024-03-150", "2024/03/15"],
        );
    }

    #[test]
    fn the_iso_date_time_preset_takes_seconds_fractions_and_zones() {
        assert_matches(
            "Date et heure ISO",
            &[
                "2024-03-15T10:30",
                "2024-03-15 10:30:00",
                "2024-03-15T10:30:00Z",
                "2024-03-15T10:30:00.123456+02:00",
                "2024-03-15T23:59:59,5-0500",
                "2024-03-15T10:30:00+01",
            ],
        );
        assert_rejects(
            "Date et heure ISO",
            &["2024-03-15", "2024-03-15T24:00", "2024-03-15T10:60", "2024-03-15_10:30", "2024-03-15T1030"],
        );
    }

    #[test]
    fn the_day_month_year_preset_takes_the_usual_separators() {
        assert_matches(
            "Date jour/mois/année",
            &["15/03/2024", "15.03.2024", "15-03-2024", "1/3/24", "31/12/99", "01.01.2000"],
        );
        assert_rejects(
            "Date jour/mois/année",
            &["32/01/2024", "15/13/2024", "0/03/2024", "15/03/202", "15/03/20245", "115/03/2024", "2024-03-15", "1.2.3", "15 03 2024"],
        );
    }

    #[test]
    fn the_written_date_preset_reads_french_months_whatever_the_case() {
        assert_matches(
            "Date en toutes lettres",
            &["15 mars 2024", "1er janvier 2025", "3 FÉVRIER 1998", "03 fevrier 1998", "12 août 2020", "9 déc. 2021", "28 Sept 2019"],
        );
        assert_rejects(
            "Date en toutes lettres",
            &["15 marsupial 2024", "32 mars 2024", "15 mars 24", "mars 2024", "15 march 2024", "15mars2024"],
        );
    }

    #[test]
    fn the_time_preset_wants_valid_hours_and_minutes() {
        assert_matches("Heure", &["10:30", "9:05", "23:59:59", "00:00:00.000", "10:30:59,123", "9h05", "14h30"]);
        assert_rejects("Heure", &["25:00", "24:00", "10:60", "10:5", "1030", "12:345", "14h", "a10:30"]);
        // What follows valid minutes is not part of the time.
        assert_eq!(preset("Heure").find("10:30:75").unwrap().as_str(), "10:30");
    }

    #[test]
    fn the_uuid_preset_wants_the_five_groups() {
        assert_matches(
            "UUID",
            &["123e4567-e89b-12d3-a456-426614174000", "123E4567-E89B-12D3-A456-426614174000"],
        );
        assert_rejects(
            "UUID",
            &[
                "123e4567-e89b-12d3-a456-42661417400",
                "123e4567-e89b-12d3-a456-4266141740000",
                "123e4567e89b12d3a456426614174000",
                "123g4567-e89b-12d3-a456-426614174000",
            ],
        );
    }

    #[test]
    fn the_hash_preset_wants_at_least_an_md5() {
        let sha1 = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        let sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_matches("Empreinte hexadécimale (MD5, SHA…)", &["d41d8cd98f00b204e9800998ecf8427e", "D41D8CD98F00B204E9800998ECF8427E", sha1, sha256]);
        assert_rejects(
            "Empreinte hexadécimale (MD5, SHA…)",
            &["d41d8cd98f00b204e9800998ecf8427", "deadbeef", "a1b2c3d", "g41d8cd98f00b204e9800998ecf8427e", "xd41d8cd98f00b204e9800998ecf8427e"],
        );
    }

    #[test]
    fn the_version_preset_wants_three_or_four_numbers() {
        assert_matches(
            "Numéro de version",
            &["1.2.3", "v1.12.3", "10.0.19045.3803", "1.0.0-alpha", "1.0.0-rc.1", "v1.12.3-beta.2", "1.0.0+build.5", "2.1.0-rc.1+exp.sha.5114f85"],
        );
        assert_rejects("Numéro de version", &["1.2", "v1", "1.2.x", "a1.2.3", "rev1.2.3", "1..2.3", "1,2,3"]);
        // The dot ending a sentence stays out.
        assert_eq!(preset("Numéro de version").find("en 1.2.3.").unwrap().as_str(), "1.2.3");
    }

    #[test]
    fn the_number_preset_leaves_identifiers_alone() {
        assert_matches("Nombre", &["0", "42", "1234,56", "3.14", "007"]);
        assert_rejects("Nombre", &["abc", "var1", "x2y", "1e5", "0x1F", "v2", "_1"]);
        // A sign is an operator as often as a sign: it stays compared.
        assert_eq!(preset("Nombre").find("-12").unwrap().as_str(), "12");
        assert_eq!(preset("Nombre").find("1.").unwrap().as_str(), "1");
    }

    #[test]
    fn an_invalid_expression_is_an_error_naming_the_rule() {
        let rules = [IgnoreRule::text("[0-9]+"), IgnoreRule::whole_line("(open")];
        let error = IgnoreRules::new(&rules).unwrap_err();
        assert!(error.starts_with("Règle 2 : expression régulière invalide."), "{error}");
        assert!(rules[1].check().is_err());
        assert_eq!(rules[0].check(), Ok(()));
    }

    #[test]
    fn disabled_rules_and_empty_patterns_are_skipped_even_when_invalid() {
        let disabled = IgnoreRule {
            enabled: false,
            ..IgnoreRule::text("(open")
        };
        let rules = IgnoreRules::new(&[disabled.clone(), IgnoreRule::text(""), IgnoreRule::whole_line("")]).unwrap();
        assert!(rules.is_empty());
        assert!(!rules.ignores_line("anything"));
        assert!(rules.ignored_ranges("anything 123").is_empty());
        assert!(IgnoreRules::default().is_empty());
        // The interface can still show what is wrong with a rule that is switched off.
        assert!(disabled.check().is_err());
    }

    #[test]
    fn ranges_are_the_merged_union_of_what_each_rule_finds() {
        // The first rule alone would stop at the dot and hide "1.5" from the second.
        let rules = IgnoreRules::new(&[
            IgnoreRule::text("[0-9]+"),
            IgnoreRule::text(r"[0-9]+\.[0-9]+ ms"),
            IgnoreRule::text("x*"),
        ])
        .unwrap();
        assert_eq!(rules.ignored_ranges("took 1.5 ms for 20 items"), [5..11, 16..18]);
        assert!(rules.ignored_ranges("nothing here").is_empty());
        // Matches that touch are one run.
        let digit = IgnoreRules::new(&[IgnoreRule::text("[0-9]")]).unwrap();
        assert_eq!(digit.ignored_ranges("a123b4"), [1..4, 5..6]);
        // Multibyte text keeps byte offsets on character boundaries.
        assert_eq!(digit.ignored_ranges("été 7"), [6..7]);
    }

    #[test]
    fn case_insensitivity_is_per_rule() {
        let rules = IgnoreRules::new(&[
            IgnoreRule {
                case_insensitive: true,
                ..IgnoreRule::text("build")
            },
            IgnoreRule::text("Date"),
            // Extended syntax must survive being put in a set with a flag in front.
            IgnoreRule {
                case_insensitive: true,
                ..IgnoreRule::whole_line("(?x) ^ \\s* rem # comment")
            },
            IgnoreRule::whole_line("^//"),
        ])
        .unwrap();
        assert_eq!(rules.ignored_ranges("BUILD date Date"), [0..5, 11..15]);
        assert!(rules.ignores_line("  REM old"));
        assert!(rules.ignores_line("// note"));
        assert!(!rules.ignores_line("code // note"));
    }

    #[test]
    fn rules_with_the_same_group_name_can_live_together() {
        let rules = IgnoreRules::new(&[IgnoreRule::text("(?P<n>[0-9]+)"), IgnoreRule::text("(?P<n>#[a-f]+)")]).unwrap();
        assert_eq!(rules.ignored_ranges("id 12 #abc"), [3..5, 6..10]);
    }
}
