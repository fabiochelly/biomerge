//! Settings and words for the formats compared by more than their lines: JSON compared by
//! structure, word-processing documents compared by their text.

use biomerge_core::Side;
use biomerge_core::extract::{ExtractOptions, Normalization, NotePlacement, Revision};
use biomerge_core::json::{ArrayOrder, JsonError, JsonOptions, KeyOrder};

/// How much typography is folded before documents are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Typography {
    /// The text exactly as stored.
    None,
    /// Invisible differences removed, quotes and dashes kept.
    Default,
    /// Quotes and dashes folded too: only the words count.
    Aggressive,
}

impl Typography {
    pub fn of(rules: &Normalization) -> Self {
        if *rules == Normalization::none() {
            Typography::None
        } else if *rules == Normalization::aggressive() {
            Typography::Aggressive
        } else {
            Typography::Default
        }
    }

    pub fn rules(self) -> Normalization {
        match self {
            Typography::None => Normalization::none(),
            Typography::Default => Normalization::default(),
            Typography::Aggressive => Normalization::aggressive(),
        }
    }

    fn key(self) -> &'static str {
        match self {
            Typography::None => "none",
            Typography::Default => "default",
            Typography::Aggressive => "aggressive",
        }
    }
}

/// A parse error as the interface shows it; JSONC and JSON5 are named as such, since they are
/// not broken files but another language.
pub fn describe_json_error(error: &JsonError) -> String {
    if error.kind.is_dialect() {
        format!(
            "JSONC ou JSON5 (ligne {}, colonne {}) : la comparaison structurelle ne prend que le JSON strict",
            error.line, error.column
        )
    } else {
        error.to_string()
    }
}

/// The status bar message for the sides that are not JSON; `None` when both are.
pub fn json_failure(errors: &[Option<String>; 2], names: &[String; 2]) -> Option<String> {
    let parts: Vec<String> = errors
        .iter()
        .zip(names)
        .filter_map(|(error, name)| error.as_ref().map(|error| format!("{name} : {error}")))
        .collect();
    (!parts.is_empty()).then(|| format!("JSON invalide, comparé en texte simple — {}", parts.join(" ; ")))
}

/// Reads the settings saved by [`store`]; what is missing keeps its default.
pub fn load_json(get: &dyn Fn(&str) -> Option<String>) -> (JsonOptions, bool) {
    let mut options = JsonOptions::default();
    let flag = |key: &str, default: bool| get(key).map_or(default, |value| value == "true");
    options.key_order = match get("json_key_order").as_deref() {
        Some("as_written") => KeyOrder::AsWritten,
        Some("alphabetical") => KeyOrder::Alphabetical,
        _ => KeyOrder::FollowReference,
    };
    options.array_order = if flag("json_unordered_arrays", false) { ArrayOrder::Unordered } else { ArrayOrder::Ordered };
    options.numbers_by_value = flag("json_numbers_by_value", true);
    options.strings_by_value = flag("json_strings_by_value", true);
    options.reference = if get("json_reference").as_deref() == Some("right") { Side::Right } else { Side::Left };
    (options, flag("json_structural", true))
}

pub fn load_extract(get: &dyn Fn(&str) -> Option<String>) -> ExtractOptions {
    let mut options = ExtractOptions::default();
    let flag = |key: &str, default: bool| get(key).map_or(default, |value| value == "true");
    options.revision = if get("doc_revision").as_deref() == Some("original") { Revision::Original } else { Revision::Final };
    options.notes = match get("doc_notes").as_deref() {
        Some("end") => NotePlacement::End,
        Some("omit") => NotePlacement::Omit,
        _ => NotePlacement::Inline,
    };
    options.link_targets = flag("doc_links", options.link_targets);
    options.images = flag("doc_images", options.images);
    options.comments = flag("doc_comments", options.comments);
    options.headers_footers = flag("doc_headers_footers", options.headers_footers);
    options.normalization = match get("doc_typography").as_deref() {
        Some("none") => Typography::None,
        Some("aggressive") => Typography::Aggressive,
        _ => Typography::Default,
    }
    .rules();
    options
}

/// The settings to keep, as `(key, value)`.
pub fn store(json: &JsonOptions, structural: bool, extract: &ExtractOptions) -> Vec<(&'static str, String)> {
    let key_order = match json.key_order {
        KeyOrder::AsWritten => "as_written",
        KeyOrder::FollowReference => "reference",
        KeyOrder::Alphabetical => "alphabetical",
    };
    vec![
        ("json_key_order", key_order.to_owned()),
        ("json_unordered_arrays", (json.array_order == ArrayOrder::Unordered).to_string()),
        ("json_numbers_by_value", json.numbers_by_value.to_string()),
        ("json_strings_by_value", json.strings_by_value.to_string()),
        ("json_reference", if json.reference == Side::Right { "right" } else { "left" }.to_owned()),
        ("json_structural", structural.to_string()),
        ("doc_revision", if extract.revision == Revision::Original { "original" } else { "final" }.to_owned()),
        (
            "doc_notes",
            match extract.notes {
                NotePlacement::Inline => "inline",
                NotePlacement::End => "end",
                NotePlacement::Omit => "omit",
            }
            .to_owned(),
        ),
        ("doc_links", extract.link_targets.to_string()),
        ("doc_images", extract.images.to_string()),
        ("doc_comments", extract.comments.to_string()),
        ("doc_headers_footers", extract.headers_footers.to_string()),
        ("doc_typography", Typography::of(&extract.normalization).key().to_owned()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn stored_settings_come_back_as_they_were() {
        let json = JsonOptions {
            key_order: KeyOrder::Alphabetical,
            array_order: ArrayOrder::Unordered,
            numbers_by_value: false,
            reference: Side::Right,
            ..JsonOptions::default()
        };
        let extract = ExtractOptions {
            revision: Revision::Original,
            notes: NotePlacement::End,
            comments: true,
            normalization: Normalization::aggressive(),
            ..ExtractOptions::default()
        };
        let stored: HashMap<&str, String> = store(&json, false, &extract).into_iter().collect();
        let get = |key: &str| stored.get(key).cloned();
        assert_eq!(load_json(&get), (json, false));
        assert_eq!(load_extract(&get), extract);
    }

    #[test]
    fn nothing_stored_means_the_defaults() {
        let get = |_: &str| None;
        assert_eq!(load_json(&get), (JsonOptions::default(), true));
        assert_eq!(load_extract(&get), ExtractOptions::default());
    }

    #[test]
    fn a_dialect_is_named_rather_than_called_broken() {
        let error = biomerge_core::json::JsonDoc::parse("{\"a\": 1, // note\n}", biomerge_core::json::Format::Json).unwrap_err();
        assert!(describe_json_error(&error).starts_with("JSONC ou JSON5"));
        let error = biomerge_core::json::JsonDoc::parse("{\"a\": }", biomerge_core::json::Format::Json).unwrap_err();
        assert!(describe_json_error(&error).starts_with("ligne 1, colonne"));
    }
}
