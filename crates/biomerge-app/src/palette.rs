//! The command palette: every action, found by typing a few letters.

use egui::{Align2, FontId, Key, Modifiers, Rect, Sense, Stroke, Ui, pos2, vec2};

use crate::icons::{self, Icon};
use crate::theme::UiPalette;
use crate::widgets;

const WIDTH: f32 = 620.0;
const ROW_HEIGHT: f32 = 36.0;
const LIST_HEIGHT: f32 = 400.0;

pub struct Entry<C> {
    pub name: String,
    pub group: &'static str,
    pub keys: &'static str,
    pub icon: Option<Icon>,
    pub checked: bool,
    pub command: C,
}

#[derive(Default)]
pub struct Palette {
    query: String,
    selected: usize,
    focus_done: bool,
    reveal: bool,
}

fn fold(c: char) -> char {
    let lower = c.to_lowercase().next().unwrap_or(c);
    match lower {
        'à' | 'â' | 'ä' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'î' | 'ï' => 'i',
        'ô' | 'ö' => 'o',
        'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        other => other,
    }
}

/// Score of `text` for `query`, each word's letters in order; word starts and runs count most.
pub fn fuzzy(query: &str, text: &str) -> Option<i32> {
    let text: Vec<char> = text.chars().map(fold).collect();
    let mut score = 0;
    for word in query.split_whitespace() {
        let word: Vec<char> = word.chars().map(fold).collect();
        let mut best = None;
        for start in (0..text.len()).filter(|&i| text[i] == word[0]) {
            let mut at = start;
            let mut word_score = if start == 0 { 10 } else { 0 };
            if start == 0 || !text[start - 1].is_alphanumeric() {
                word_score += 8;
            }
            let mut matched = true;
            for &letter in &word[1..] {
                let Some(next) = (at + 1..text.len()).find(|&i| text[i] == letter) else {
                    matched = false;
                    break;
                };
                word_score += if next == at + 1 { 5 } else { -(next as i32 - at as i32).min(4) };
                at = next;
            }
            if matched && best.is_none_or(|b| word_score > b) {
                best = Some(word_score);
            }
        }
        score += best?;
    }
    Some(score - text.len() as i32 / 8)
}

impl Palette {
    /// Returns the index of the entry to run, and whether the palette must close.
    pub fn show<C>(
        &mut self,
        ui: &mut Ui,
        chrome: &UiPalette,
        entries: &[Entry<C>],
    ) -> (Option<usize>, bool) {
        let query = self.query.trim().to_owned();
        let mut shown: Vec<(usize, i32)> = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if query.is_empty() {
                    return Some((index, 0));
                }
                fuzzy(&query, &format!("{} {}", entry.name, entry.group)).map(|score| (index, score))
            })
            .collect();
        shown.sort_by_key(|&(index, score)| (-score, index));
        self.selected = self.selected.min(shown.len().saturating_sub(1));

        let (up, down, enter, escape, toggle) = ui.input_mut(|input| {
            (
                input.consume_key(Modifiers::NONE, Key::ArrowUp),
                input.consume_key(Modifiers::NONE, Key::ArrowDown),
                input.consume_key(Modifiers::NONE, Key::Enter),
                input.consume_key(Modifiers::NONE, Key::Escape),
                input.consume_key(Modifiers::COMMAND, Key::P),
            )
        });
        if down && !shown.is_empty() {
            self.selected = (self.selected + 1) % shown.len();
            self.reveal = true;
        }
        if up && !shown.is_empty() {
            self.selected = (self.selected + shown.len() - 1) % shown.len();
            self.reveal = true;
        }
        let mut pick = enter.then(|| shown.get(self.selected).map(|s| s.0)).flatten();
        let close = escape || toggle;

        ui.set_width(WIDTH);
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        let (field, _) = ui.allocate_exact_size(vec2(WIDTH, 46.0), Sense::hover());
        let center_y = field.center().y;
        icons::paint(ui.painter(), Icon::Magnifier, icons::square(pos2(field.min.x + 22.0, center_y), 16.0), chrome.text_dim);
        let inner = Rect::from_min_max(pos2(field.min.x + 42.0, field.min.y), pos2(field.max.x - 12.0, field.max.y));
        let mut child = ui.new_child(
            egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        child.spacing_mut().interact_size.y = 0.0;
        let id = ui.scope_id().with("palette-query");
        let response = child.add(
            egui::TextEdit::singleline(&mut self.query)
                .id(id)
                .font(FontId::proportional(14.0))
                .hint_text("Rechercher une commande…")
                .frame(egui::Frame::NONE)
                .margin(egui::Margin::ZERO)
                .desired_width(inner.width()),
        );
        ui.ctx().accesskit_node_builder(response.id, |builder| {
            builder.set_label("Commande");
        });
        if !self.focus_done || !response.has_focus() {
            self.focus_done = true;
            response.request_focus();
        }
        if response.changed() {
            self.selected = 0;
        }
        ui.painter().hline(field.x_range(), ui.painter().round_to_pixel_center(field.max.y), Stroke::new(1.0, chrome.border));

        egui::ScrollArea::vertical()
            .max_height(LIST_HEIGHT)
            .auto_shrink([false, true])
            .id_salt("palette-list")
            .show(ui, |ui| {
                egui::Frame::new().inner_margin(6).show(ui, |ui| {
                    ui.set_width(WIDTH - 12.0);
                    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                    if shown.is_empty() {
                        let (rect, _) = ui.allocate_exact_size(vec2(WIDTH - 12.0, 72.0), Sense::hover());
                        ui.painter().text(rect.center(), Align2::CENTER_CENTER, "Aucune commande", FontId::proportional(13.0), chrome.text_dim);
                    }
                    for (rank, &(index, _)) in shown.iter().enumerate() {
                        let entry = &entries[index];
                        let (rect, row) = ui.allocate_exact_size(vec2(WIDTH - 12.0, ROW_HEIGHT), Sense::click());
                        row.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &entry.name));
                        let selected = rank == self.selected;
                        if selected && self.reveal {
                            ui.scroll_to_rect(rect, None);
                        }
                        if row.hovered() && ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO) {
                            self.selected = rank;
                        }
                        if !ui.is_rect_visible(rect) {
                            continue;
                        }
                        let painter = ui.painter();
                        if selected {
                            painter.rect_filled(rect, 7.0, chrome.hover);
                        }
                        let y = rect.center().y;
                        let mut x = rect.min.x + 12.0;
                        if let Some(icon) = entry.icon {
                            let color = if selected { chrome.accent } else { chrome.text_dim };
                            icons::paint(painter, icon, icons::square(pos2(x + 8.0, y), 16.0), color);
                        }
                        x += 28.0;
                        painter.text(pos2(x, y), Align2::LEFT_CENTER, &entry.name, FontId::proportional(13.5), chrome.text);
                        let right = rect.max.x - 12.0;
                        if entry.checked {
                            icons::paint(painter, Icon::MenuCheck, icons::square(pos2(right - 7.0, y), 14.0), chrome.accent);
                        } else if !entry.keys.is_empty() {
                            painter.text(pos2(right, y), Align2::RIGHT_CENTER, entry.keys, FontId::proportional(12.0), chrome.text_dim);
                        } else {
                            painter.text(pos2(right, y), Align2::RIGHT_CENTER, entry.group, widgets::medium(11.5), chrome.text_dim);
                        }
                        if row.clicked() {
                            pick = Some(index);
                        }
                    }
                });
            });
        self.reveal = false;
        (pick, close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_starts_rank_first_and_accents_do_not_matter() {
        let near = fuzzy("diff", "Différence suivante").unwrap();
        let far = fuzzy("diff", "Modifier les fins de ligne").unwrap_or(i32::MIN);
        assert!(near > far);
        assert!(fuzzy("pol tex", "Police du texte…").is_some());
        assert!(fuzzy("xyz", "Police du texte…").is_none());
    }
}
