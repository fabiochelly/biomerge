//! The editor of the ignore rules, and their place in the settings.

use biomerge_core::ignore_rules::{IGNORE_PRESETS, IgnoreRule, IgnoreRules, IgnoreScope};
use egui::{Align2, Button, Popup, Rect, RichText, Sense, Stroke, Ui, WidgetInfo, pos2, vec2};

use crate::icons::{self, Icon};
use crate::theme::{UiPalette, mix};
use crate::widgets::{self, Item, dialog_frame, menu_frame, menu_width};

const WIDTH: f32 = 700.0;
const FIELD_HEIGHT: f32 = 32.0;
const SCOPE_BUTTON: [f32; 2] = [62.0, 104.0];
const LIST_HEIGHT: f32 = 330.0;

/// The rules that compile, compiled together. A rule with an error is shown with it in the
/// editor and left out here, so that one typo does not switch every other rule off.
pub fn compile(rules: &[IgnoreRule]) -> IgnoreRules {
    let valid: Vec<IgnoreRule> = rules.iter().filter(|rule| rule.check().is_ok()).cloned().collect();
    IgnoreRules::new(&valid).unwrap_or_default()
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n")
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('\\', Some('t')) => {
                out.push('\t');
                chars.next();
            }
            ('\\', Some('n')) => {
                out.push('\n');
                chars.next();
            }
            ('\\', Some('\\')) => {
                out.push('\\');
                chars.next();
            }
            (c, _) => out.push(c),
        }
    }
    out
}

/// One rule per line: three flags then the pattern, as `1t0<TAB>pattern` (enabled, text or
/// `l`ine, case-insensitive). The core keeps rules free of any storage format.
pub fn encode(rules: &[IgnoreRule]) -> String {
    rules
        .iter()
        .map(|rule| {
            format!(
                "{}{}{}\t{}\n",
                u8::from(rule.enabled),
                if rule.scope == IgnoreScope::WholeLine { 'l' } else { 't' },
                u8::from(rule.case_insensitive),
                escape(&rule.pattern)
            )
        })
        .collect()
}

/// Lines it cannot read are skipped: settings from another version must not stop the start.
pub fn decode(text: &str) -> Vec<IgnoreRule> {
    text.lines()
        .filter_map(|line| {
            let (flags, pattern) = line.split_once('\t')?;
            let flags: Vec<char> = flags.chars().collect();
            let [enabled, scope, case] = flags[..] else {
                return None;
            };
            Some(IgnoreRule {
                pattern: unescape(pattern),
                scope: if scope == 'l' { IgnoreScope::WholeLine } else { IgnoreScope::MatchedText },
                case_insensitive: case == '1',
                enabled: enabled == '1',
            })
        })
        .collect()
}

/// A lit two-way choice, as the sides of the go-to-line prompt.
fn segment(ui: &mut Ui, chrome: &UiPalette, rect: Rect, id: egui::Id, text: &str, tip: &str, lit: bool) -> bool {
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| WidgetInfo::selected(egui::Role::RadioButton, true, lit, text));
    if lit {
        let border = mix(chrome.raised, chrome.accent, 0.55);
        ui.painter().rect(rect, 6.0, mix(chrome.raised, chrome.accent, 0.2), Stroke::new(1.0, border), egui::StrokeKind::Inside);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 6.0, chrome.hover);
    }
    let color = if lit { chrome.accent } else { chrome.text_dim };
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, widgets::medium(12.0), color);
    response.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// A square icon that lights up when on, as the options of the band's search field.
fn toggle(ui: &mut Ui, chrome: &UiPalette, rect: Rect, id: egui::Id, icon: Icon, tip: &str, on: bool) -> bool {
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| WidgetInfo::selected(egui::Role::CheckBox, true, on, tip));
    if on {
        let border = mix(chrome.raised, chrome.accent, 0.55);
        ui.painter().rect(rect, 6.0, mix(chrome.raised, chrome.accent, 0.2), Stroke::new(1.0, border), egui::StrokeKind::Inside);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 6.0, chrome.hover);
    }
    let color = if on { chrome.accent } else if response.hovered() { chrome.text } else { chrome.text_dim };
    icons::paint(ui.painter(), icon, icons::square(rect.center(), 16.0), color);
    response.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// One rule on a line: on or off, its expression, what it ignores, its case, its removal.
/// Returns whether it changed and whether it asks to be removed.
fn rule_row(ui: &mut Ui, chrome: &UiPalette, index: usize, rule: &mut IgnoreRule, focus: bool) -> (bool, bool) {
    let mut changed = false;
    let mut remove = false;
    let width = ui.available_width();
    let (line, _) = ui.allocate_exact_size(vec2(width, FIELD_HEIGHT), Sense::hover());
    let id = ui.scope_id().with(("rule", index));
    let y = line.center().y;

    // egui centres a checkbox in its interaction height, a little below the middle of the row.
    let check = Rect::from_center_size(pos2(line.left() + 12.0, y - 4.0), vec2(22.0, 22.0));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(check));
    let tip = if rule.enabled { "Désactiver la règle" } else { "Activer la règle" };
    if child.checkbox(&mut rule.enabled, "").on_hover_text(tip).changed() {
        changed = true;
    }

    let remove_rect = Rect::from_center_size(pos2(line.right() - 14.0, y), vec2(28.0, 28.0));
    if widgets::icon_button(ui, chrome, remove_rect, id.with("remove"), Icon::Trash, "Supprimer la règle", chrome.text_dim).clicked() {
        remove = true;
    }
    let case_rect = Rect::from_center_size(pos2(remove_rect.left() - 18.0, y), vec2(28.0, 28.0));
    if toggle(ui, chrome, case_rect, id.with("case"), Icon::Case, "Ignorer la casse dans l'expression", rule.case_insensitive) {
        rule.case_insensitive = !rule.case_insensitive;
        changed = true;
    }
    let scopes = [
        (IgnoreScope::MatchedText, "Texte", "Ignore seulement le texte trouvé"),
        (IgnoreScope::WholeLine, "Ligne entière", "Ignore toute ligne où l'expression est trouvée"),
    ];
    let mut right = case_rect.left() - 10.0;
    for (slot, (scope, text, tip)) in scopes.into_iter().enumerate().rev() {
        let rect = Rect::from_min_max(pos2(right - SCOPE_BUTTON[slot], y - 13.0), pos2(right, y + 13.0));
        right = rect.left() - 2.0;
        if segment(ui, chrome, rect, id.with(("scope", slot)), text, tip, rule.scope == scope) && rule.scope != scope {
            rule.scope = scope;
            changed = true;
        }
    }

    let field = Rect::from_min_max(pos2(check.right() + 10.0, line.top()), pos2(right - 10.0, line.bottom()));
    let error = rule.check().err().filter(|_| !rule.pattern.is_empty());
    let color = if rule.enabled { chrome.text } else { chrome.text_dim };
    let response = widgets::code_field(ui, chrome, id.with("pattern"), field, &mut rule.pattern, "Expression régulière, ex. \\d{4}-\\d{2}-\\d{2}", color, "Expression de la règle", error.is_some());
    if focus {
        response.request_focus();
    }
    changed |= response.changed();
    if let Some(error) = error {
        // The regex crate explains on several lines with a caret under the fault: the last line
        // says what is wrong.
        let reason = error.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or(&error).trim().trim_start_matches("error: ");
        let (rect, _) = ui.allocate_exact_size(vec2(width, 18.0), Sense::hover());
        ui.painter().text(
            pos2(field.left() + 2.0, rect.center().y),
            Align2::LEFT_CENTER,
            format!("Expression invalide : {reason}"),
            egui::FontId::proportional(11.5),
            chrome.error,
        );
    }
    ui.add_space(6.0);
    (changed, remove)
}

/// Shows the editor. Returns whether the rules changed (to compile and apply them at once)
/// and whether the editor closes.
pub fn show(ctx: &egui::Context, chrome: &UiPalette, rules: &mut Vec<IgnoreRule>) -> (bool, bool) {
    let mut changed = false;
    let mut close = false;
    let focus_id = egui::Id::unique("ignore-rules-focus");
    let focus = ctx.data_mut(|d| d.remove_temp::<usize>(focus_id));
    let modal = egui::Modal::new(egui::Id::unique("ignore-rules")).frame(dialog_frame(chrome)).show(ctx, |ui| {
        ui.set_width(WIDTH);
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.label(RichText::new("Règles d'ignorance").font(widgets::semibold(16.0)));
        ui.label(
            RichText::new(
                "Ce que ces expressions régulières trouvent ne compte pas comme une différence : dates, \
                 identifiants, numéros de version… Les lignes concernées restent visibles, en grisé.",
            )
            .color(chrome.text_dim)
            .size(12.0),
        );
        ui.add_space(8.0);
        if rules.is_empty() {
            let (rect, _) = ui.allocate_exact_size(vec2(WIDTH, 70.0), Sense::hover());
            ui.painter().rect_stroke(rect, 8.0, Stroke::new(1.0, chrome.border), egui::StrokeKind::Inside);
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Aucune règle. Ajoute une expression, ou pars d'un préréglage.",
                egui::FontId::proportional(13.0),
                chrome.text_dim,
            );
        } else {
            let mut removed = None;
            egui::ScrollArea::vertical().max_height(LIST_HEIGHT).auto_shrink([false, true]).show(ui, |ui| {
                ui.set_width(WIDTH - 4.0);
                for (index, rule) in rules.iter_mut().enumerate() {
                    let (rule_changed, remove) = rule_row(ui, chrome, index, rule, focus == Some(index));
                    changed |= rule_changed;
                    if remove {
                        removed = Some(index);
                    }
                }
            });
            if let Some(index) = removed {
                rules.remove(index);
                changed = true;
            }
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.add(Button::new("Ajouter une règle").min_size(vec2(0.0, 32.0))).clicked() {
                rules.push(IgnoreRule::text(""));
                ctx.data_mut(|d| d.insert_temp(focus_id, rules.len() - 1));
                changed = true;
            }
            let response = ui.add(Button::new("Ajouter un préréglage").min_size(vec2(0.0, 32.0)));
            Popup::menu(&response).frame(menu_frame(chrome)).show(|ui| {
                menu_width(ui, 420.0);
                for preset in IGNORE_PRESETS {
                    if Item::new(preset.name).keys(preset.example).show(ui, chrome).clicked() {
                        rules.push(preset.rule());
                        changed = true;
                        ui.close();
                    }
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(crate::folder_view::primary_button(chrome, "Fermer")).clicked() {
                    close = true;
                }
            });
        });
    });
    (changed, close || modal.should_close())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_come_back_from_the_settings_as_they_were() {
        let rules = vec![
            IgnoreRule::text(r"\d{4}\t-\\x"),
            IgnoreRule { case_insensitive: true, enabled: false, ..IgnoreRule::whole_line("^// généré") },
            IgnoreRule::text("tab\there"),
        ];
        assert_eq!(decode(&encode(&rules)), rules);
        assert!(decode("garbage\n\n1t\n").is_empty());
    }

    #[test]
    fn a_rule_with_an_error_does_not_switch_the_others_off() {
        let rules = vec![IgnoreRule::text("("), IgnoreRule::text("[0-9]+")];
        let compiled = compile(&rules);
        assert!(!compiled.is_empty());
        assert_eq!(compiled.ignored_ranges("v12"), [1..3]);
    }
}
