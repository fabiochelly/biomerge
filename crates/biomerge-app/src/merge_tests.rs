//! The three-way merge tab, driven through the whole application as the user would.

use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::Duration;

use biomerge_core::merge3::{ChunkKind, Resolution, Stop, Version};
use egui::{Key, Modifiers, pos2, vec2};
use egui_kittest::Harness;

use super::visual_tests::{answered, empty_harness, launch, listening_harness, save, settle, type_text, wait_for_tabs};
use super::*;

const BASE: &str = r#"//! Facturation : calcul des montants.

pub fn total(lines: &[Line]) -> f64 {
    let mut sum = 0.0;
    for line in lines {
        sum += line.price * line.quantity as f64;
    }
    sum
}

pub fn tax(amount: f64) -> f64 {
    amount * 0.20
}

pub fn label(kind: Kind) -> &'static str {
    match kind {
        Kind::Cheque => "Chèque",
        Kind::Transfer => "Virement",
    }
}

pub fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn main() {
    println!("{}", round(tax(total(&[]))));
}
"#;

/// Four changes against the base: one alone, one conflicting, one next to a change of the
/// other side, one the other side made too.
fn local() -> String {
    BASE.replace("let mut sum = 0.0;", "let mut sum: f64 = 0.0;")
        .replace("amount * 0.20", "amount * 0.21")
        .replace("=> \"Chèque\",", "=> \"Chèque bancaire\",")
        .replace("round() / 100.0\n", "round() / 100.0 // au centime\n")
}

/// Its own changes, two of them conflicting with the local ones, and an insertion.
fn remote() -> String {
    BASE.replace("line.price * line.quantity as f64", "line.price * f64::from(line.quantity)")
        .replace("amount * 0.20", "amount * TAX_RATE")
        .replace("=> \"Virement\",", "=> \"Virement SEPA\",")
        .replace("round() / 100.0\n", "round() / 100.0 // au centime\n")
        .replace("}\n\nfn main", "}\n\nconst TAX_RATE: f64 = 0.20;\n\nfn main")
}

/// git's four files: the base, both sides and the working file with its markers.
fn write_merge(dir: &Path) -> MergeInputs {
    let [base, local_path, remote_path, merged] = ["facture_BASE_1.rs", "facture_LOCAL_1.rs", "facture_REMOTE_1.rs", "facture.rs"].map(|name| dir.join(name));
    std::fs::write(&base, BASE).unwrap();
    std::fs::write(&local_path, local()).unwrap();
    std::fs::write(&remote_path, remote()).unwrap();
    std::fs::write(&merged, "<<<<<<< HEAD\nwhat git left\n=======\nother\n>>>>>>> theirs\n").unwrap();
    let labels = [Some("facture.rs (base)".to_owned()), Some("facture.rs (local)".to_owned()), Some("facture.rs (distant)".to_owned())];
    MergeInputs::Files { base, local: local_path, remote: remote_path, merged, labels }
}

fn harness_for_merge(inputs: MergeInputs) -> Harness<'static, App> {
    let ctx = egui::Context::default();
    let app = App::new(&ctx, None, None);
    let mut pending = Some(inputs);
    Harness::builder()
        .with_size(vec2(1440.0, 760.0))
        .with_pixels_per_point(2.0)
        .with_max_steps(20_000)
        .allow_missing_glyphs()
        .wgpu()
        .build_ui_state(
            move |ui, app: &mut App| {
                if let Some(inputs) = pending.take() {
                    app.install_look(ui.ctx());
                    app.open_merge(ui.ctx(), inputs, None);
                }
                app.root(ui);
            },
            app,
        )
}

fn opened(dir: &Path) -> Harness<'static, App> {
    let mut harness = harness_for_merge(write_merge(dir));
    harness.step();
    settle(&mut harness);
    harness
}

fn conflicts(harness: &mut Harness<'_, App>) -> Vec<usize> {
    let merge = harness.state_mut().merge_tab_mut().merge();
    (0..merge.chunks().len()).filter(|&index| merge.chunks()[index].is_conflict()).collect()
}

fn unresolved(harness: &mut Harness<'_, App>) -> usize {
    harness.state_mut().merge_tab_mut().unresolved_count()
}

fn ctrl(harness: &mut Harness<'_, App>, key: Key) {
    harness.key_press_modifiers(Modifiers::COMMAND, key);
    harness.run_steps(3);
}

#[test]
fn a_merge_opens_with_one_sided_changes_merged_and_its_conflicts_counted() {
    let dir = tempfile::tempdir().unwrap();
    let mut harness = opened(dir.path());
    let tab = harness.state_mut().merge_tab_mut();
    let counts = tab.merge().counts();
    assert_eq!((counts.conflicts, counts.unresolved, counts.combinable), (2, 2, 1));
    assert_eq!((counts.left, counts.right, counts.both), (1, 2, 1));
    let text = tab.result_text();
    assert!(text.contains("let mut sum: f64 = 0.0;"), "the local change is in");
    assert!(text.contains("f64::from(line.quantity)"), "the remote change is in");
    assert!(text.contains("const TAX_RATE"), "the remote insertion is in");
    assert!(text.contains("amount * 0.20"), "an unresolved conflict shows the base");
    assert!(tab.is_dirty(), "the result is not on disk yet");
    // Straight to the first conflict.
    let first = conflicts(&mut harness)[0];
    assert_eq!(harness.state_mut().merge_tab_mut().current_chunk_index(), Some(first));
    let status = harness.state().status_items_text();
    assert!(status.contains("2 conflits non résolus"), "{status}");
    assert!(status.contains("Fusion auto : 1 de gauche, 2 de droite, 1 des deux"), "{status}");
    assert_eq!(harness.state().tabs[0].title(), "facture.rs (fusion)");
    for (mode, name) in [(Mode::Dark, "merge-dark.png"), (Mode::Light, "merge-light.png")] {
        let ctx = harness.ctx.clone();
        harness.state_mut().set_mode(&ctx, mode);
        settle(&mut harness);
        save(&mut harness, name);
    }
}

#[test]
fn the_arrow_keys_take_a_side_and_undo_brings_the_conflict_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut harness = opened(dir.path());
    let conflict = conflicts(&mut harness)[0];
    // Ctrl+→: what flows rightwards, the left side, into the result.
    ctrl(&mut harness, Key::ArrowRight);
    let tab = harness.state_mut().merge_tab_mut();
    assert!(tab.result_text().contains("amount * 0.21"));
    assert_eq!(tab.merge().chunks()[conflict].resolution, Resolution::Left);
    assert_eq!(unresolved(&mut harness), 1);
    save(&mut harness, "merge-took-left.png");
    // Ctrl+←: the right side instead.
    ctrl(&mut harness, Key::ArrowLeft);
    assert!(harness.state_mut().merge_tab_mut().result_text().contains("amount * TAX_RATE"));
    ctrl(&mut harness, Key::Z);
    ctrl(&mut harness, Key::Z);
    let tab = harness.state_mut().merge_tab_mut();
    assert_eq!(tab.merge().chunks()[conflict].resolution, Resolution::Unresolved);
    assert!(tab.result_text().contains("amount * 0.20"));
    assert_eq!(unresolved(&mut harness), 2);
    ctrl(&mut harness, Key::Y);
    assert!(harness.state_mut().merge_tab_mut().result_text().contains("amount * 0.21"));
    // The menu's choices go through the same steps.
    let ctx = harness.ctx.clone();
    harness.state_mut().run(&ctx, Command::Resolve(Resolution::LeftThenRight));
    harness.run_steps(2);
    let text = harness.state_mut().merge_tab_mut().result_text();
    assert!(text.contains("amount * 0.21\n    amount * TAX_RATE"), "{text}");
}

#[test]
fn a_held_key_takes_one_side_only_and_a_chunk_out_of_view_is_shown_first() {
    let dir = tempfile::tempdir().unwrap();
    // A long unchanged tail, to scroll the conflicts away.
    let inputs = write_merge(dir.path());
    let MergeInputs::Files { base, local, remote, .. } = &inputs else { unreachable!() };
    let tail: String = (0..300).map(|i| format!("// ligne de remplissage {i}\n")).collect();
    for path in [base, local, remote] {
        let text = std::fs::read_to_string(path).unwrap();
        std::fs::write(path, text + &tail).unwrap();
    }
    let mut harness = harness_for_merge(inputs);
    harness.step();
    settle(&mut harness);
    let conflict = conflicts(&mut harness)[0];
    // Scrolled far away: the key brings the chunk into view without changing it.
    harness.state_mut().merge_tab_mut().scroll_lines(400.0);
    harness.run_steps(2);
    ctrl(&mut harness, Key::ArrowRight);
    assert_eq!(harness.state_mut().merge_tab_mut().merge().chunks()[conflict].resolution, Resolution::Unresolved);
    let [left, _, _] = harness.state_mut().merge_tab_mut().pane_positions();
    assert!(left < 100.0, "the chunk came into view: {left}");
    // Held down, the key takes the left side once; its repeats do nothing more, not even on
    // the next chunk.
    harness.key_down_modifiers(Modifiers::COMMAND, Key::ArrowRight);
    harness.run_steps(2);
    for _ in 0..3 {
        harness.key_down_modifiers(Modifiers::COMMAND, Key::ArrowRight);
        harness.run_steps(2);
    }
    harness.key_up_modifiers(Modifiers::COMMAND, Key::ArrowRight);
    harness.run_steps(2);
    let tab = harness.state_mut().merge_tab_mut();
    assert_eq!(tab.merge().chunks()[conflict].resolution, Resolution::Left);
    assert_eq!(tab.undo_depth(), 1);
}

#[test]
fn a_combinable_conflict_takes_both_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut harness = opened(dir.path());
    let combinable = {
        let merge = harness.state_mut().merge_tab_mut().merge();
        (0..merge.chunks().len()).find(|&index| merge.chunks()[index].combinable).unwrap()
    };
    harness.key_press(Key::F8);
    harness.run_steps(3);
    assert_eq!(harness.state_mut().merge_tab_mut().current_chunk_index(), Some(combinable));
    assert!(harness.state_mut().merge_tab_mut().current_combinable());
    save(&mut harness, "merge-combinable.png");
    let ctx = harness.ctx.clone();
    harness.state_mut().run(&ctx, Command::Resolve(Resolution::Merged));
    harness.run_steps(2);
    let text = harness.state_mut().merge_tab_mut().result_text();
    assert!(text.contains("\"Chèque bancaire\",\n        Kind::Transfer => \"Virement SEPA\","), "{text}");
    assert_eq!(unresolved(&mut harness), 1);
}

#[test]
fn typing_in_a_conflict_resolves_it_and_the_burst_undoes_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut harness = opened(dir.path());
    let conflict = conflicts(&mut harness)[0];
    let line = harness.state_mut().merge_tab_mut().merge().chunks()[conflict].result.start;
    harness.state_mut().merge_tab_mut().put_caret(1, line, 4);
    harness.run_steps(2);
    type_text(&mut harness, "0.5 * ");
    let tab = harness.state_mut().merge_tab_mut();
    assert!(tab.result_text().contains("    0.5 * amount * 0.20"), "{}", tab.result_text());
    assert_eq!(tab.merge().chunks()[conflict].resolution, Resolution::Edited);
    assert_eq!(tab.caret_position(), Some((1, line, 10)));
    assert_eq!(unresolved(&mut harness), 1);
    // Typing in a side is refused: only the result changes.
    harness.state_mut().merge_tab_mut().put_caret(0, 0, 0);
    harness.run_steps(1);
    type_text(&mut harness, "x");
    assert!(harness.state_mut().merge_tab_mut().message.as_deref().is_some_and(|m| m.contains("lecture seule")));
    assert!(!harness.state_mut().merge_tab_mut().merge().left.lines[0].text.starts_with('x'));
    ctrl(&mut harness, Key::Z);
    let tab = harness.state_mut().merge_tab_mut();
    assert!(tab.result_text().contains("    amount * 0.20"));
    assert_eq!(tab.merge().chunks()[conflict].resolution, Resolution::Unresolved);
    assert!(!tab.can_undo(), "one burst, one step");
}

#[test]
fn f8_and_f7_step_through_the_unresolved_conflicts_and_around() {
    let dir = tempfile::tempdir().unwrap();
    let mut harness = opened(dir.path());
    let all = conflicts(&mut harness);
    assert_eq!(harness.state_mut().merge_tab_mut().current_chunk_index(), Some(all[0]));
    harness.key_press(Key::F8);
    harness.run_steps(3);
    assert_eq!(harness.state_mut().merge_tab_mut().current_chunk_index(), Some(all[1]));
    harness.key_press(Key::F8);
    harness.run_steps(3);
    assert_eq!(harness.state_mut().merge_tab_mut().current_chunk_index(), Some(all[0]), "around the end");
    harness.key_press(Key::F7);
    harness.run_steps(3);
    assert_eq!(harness.state_mut().merge_tab_mut().current_chunk_index(), Some(all[1]));
    // Ctrl+↓ stops at every change, conflicts or not.
    let changes: Vec<usize> = {
        let merge = harness.state_mut().merge_tab_mut().merge();
        (0..merge.chunks().len()).filter(|&i| merge.chunks()[i].kind != ChunkKind::Unchanged).collect()
    };
    ctrl(&mut harness, Key::ArrowUp);
    let position = changes.iter().position(|&c| c == all[1]).unwrap();
    assert_eq!(harness.state_mut().merge_tab_mut().current_chunk_index(), Some(changes[position - 1]));
    // Once resolved, F8 says there is nothing left.
    let ctx = harness.ctx.clone();
    for &conflict in &all {
        harness.state_mut().merge_tab_mut().show_chunk(conflict);
        harness.state_mut().run(&ctx, Command::Resolve(Resolution::Right));
        harness.run_steps(2);
    }
    harness.key_press(Key::F8);
    harness.run_steps(2);
    assert_eq!(harness.state_mut().merge_tab_mut().message.as_deref(), Some("Aucun conflit non résolu"));
}

#[test]
fn saving_with_unresolved_conflicts_asks_and_writes_markers_or_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let inputs = write_merge(dir.path());
    let merged = inputs.output().to_owned();
    let before = std::fs::read_to_string(&merged).unwrap();
    let mut harness = harness_for_merge(inputs);
    harness.step();
    settle(&mut harness);
    ctrl(&mut harness, Key::S);
    assert!(harness.state_mut().merge_tab_mut().wants_dialog(), "the question is asked");
    save(&mut harness, "merge-save-question.png");
    harness.key_press(Key::Escape);
    harness.run_steps(3);
    assert!(!harness.state_mut().merge_tab_mut().wants_dialog());
    assert_eq!(std::fs::read_to_string(&merged).unwrap(), before, "cancelled: nothing written");

    ctrl(&mut harness, Key::S);
    {
        use egui_kittest::kittest::Queryable;
        harness.get_by_label("Enregistrer avec les marqueurs de conflit").click();
    }
    harness.run_steps(3);
    let written = std::fs::read_to_string(&merged).unwrap();
    assert!(written.contains("<<<<<<< ours\n    amount * 0.21\n=======\n    amount * TAX_RATE\n>>>>>>> theirs\n"), "{written}");
    assert!(written.contains("let mut sum: f64 = 0.0;"));
    let tab = harness.state_mut().merge_tab_mut();
    assert!(!tab.is_dirty());
    assert_eq!(tab.exit_code(), 1, "still in conflict for git");

    // Resolved: saved at once, and git hears success.
    let all = conflicts(&mut harness);
    let ctx = harness.ctx.clone();
    for conflict in all {
        harness.state_mut().merge_tab_mut().show_chunk(conflict);
        harness.state_mut().run(&ctx, Command::Resolve(Resolution::Left));
    }
    harness.run_steps(2);
    ctrl(&mut harness, Key::S);
    assert!(!harness.state_mut().merge_tab_mut().wants_dialog());
    let written = std::fs::read_to_string(&merged).unwrap();
    assert!(!written.contains("<<<<<<<") && written.contains("amount * 0.21"), "{written}");
    assert_eq!(harness.state_mut().merge_tab_mut().exit_code(), 0);
}

#[test]
fn closing_a_merge_with_unsaved_work_asks_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut harness = opened(dir.path());
    harness.state_mut().request_close(0);
    harness.run_steps(2);
    assert_eq!(harness.state().confirm, Some(AfterConfirm::CloseTab(0)));
}

fn conflicted_file(dir: &Path) -> PathBuf {
    let path = dir.join("conflit.py");
    std::fs::write(
        &path,
        "def prix(x):\n<<<<<<< HEAD\n    return x * 1.2\n=======\n    return round(x * 1.2, 2)\n>>>>>>> feature\n\nprint(prix(3))\n",
    )
    .unwrap();
    path
}

#[test]
fn a_file_left_in_conflict_is_offered_as_a_merge_from_the_open_dialog_and_its_tab() {
    use egui_kittest::kittest::Queryable;
    let dir = tempfile::tempdir().unwrap();
    let path = conflicted_file(dir.path());
    let mut harness = empty_harness();
    let ctx = harness.ctx.clone();
    harness.state().install_look(&ctx);
    harness.state_mut().open_dialog = Some(OpenDialog { paths: [path.display().to_string(), String::new()], ..OpenDialog::default() });
    harness.run_steps(2);
    harness.get_by_label("Comparer").click();
    harness.run_steps(3);
    assert!(harness.state().open_dialog.as_ref().is_some_and(|dialog| dialog.conflicted.is_some()));
    save(&mut harness, "merge-offer.png");
    harness.get_by_label("Résoudre").click();
    wait_for_tabs(&mut harness, 1);
    settle(&mut harness);
    let tab = harness.state_mut().merge_tab_mut();
    assert_eq!(tab.unresolved_count(), 1);
    assert!(!tab.base_known(), "no ||||||| section");
    assert!(!tab.is_dirty(), "the result is the file");
    assert_eq!(tab.merge().document(Version::Left).lines[1].text, "    return x * 1.2");
    save(&mut harness, "merge-conflicted.png");

    // A comparison holding that file offers the same.
    let other = dir.path().join("autre.py");
    std::fs::write(&other, "def prix(x):\n    return x\n").unwrap();
    harness.state_mut().open(&ctx, [path.clone(), other]).unwrap();
    wait_for_tabs(&mut harness, 2);
    settle(&mut harness);
    assert!(harness.state_mut().text_tab_mut().conflicted_path().is_some());
    harness.get_by_label("Ce fichier contient des conflits : résoudre en fusion à 3 voies").click();
    harness.run_steps(2);
    wait_for_tabs(&mut harness, 3);
    settle(&mut harness);
    assert_eq!(harness.state_mut().merge_tab_mut().unresolved_count(), 1);
}

#[test]
fn the_base_takes_the_centre_on_request_read_only() {
    use egui_kittest::kittest::Queryable;
    let dir = tempfile::tempdir().unwrap();
    let mut harness = opened(dir.path());
    harness.key_press_modifiers(Modifiers::ALT, Key::B);
    harness.run_steps(3);
    assert!(harness.state_mut().merge_tab_mut().shows_base());
    save(&mut harness, "merge-base.png");
    let conflict = conflicts(&mut harness)[0];
    let line = harness.state_mut().merge_tab_mut().merge().chunks()[conflict].base.start;
    harness.state_mut().merge_tab_mut().put_caret(1, line, 0);
    harness.run_steps(1);
    type_text(&mut harness, "x");
    assert!(harness.state_mut().merge_tab_mut().message.as_deref().is_some_and(|m| m.contains("ancêtre commun")));
    assert!(!harness.state_mut().merge_tab_mut().merge().base.lines[line as usize].text.starts_with('x'));
    // The lit icon of the status bar turns it off.
    harness.get_by_label("Afficher l'ancêtre commun (Alt+B)").click();
    harness.run_steps(3);
    assert!(!harness.state_mut().merge_tab_mut().shows_base());
}

#[test]
fn find_and_go_to_line_reach_every_pane() {
    let dir = tempfile::tempdir().unwrap();
    let mut harness = opened(dir.path());
    ctrl(&mut harness, Key::F);
    type_text(&mut harness, "TAX_RATE");
    harness.run_steps(2);
    // In the remote version's conflict, its insertion, and the result's copy of the insertion.
    let counts = harness.state_mut().merge_tab_mut().find_counts();
    assert_eq!(counts.map(|(_, total)| total), Some(3), "{counts:?}");
    harness.key_press(Key::Escape);
    harness.run_steps(2);
    ctrl(&mut harness, Key::G);
    type_text(&mut harness, "d:12");
    harness.key_press(Key::Enter);
    harness.run_steps(4);
    let tab = harness.state_mut().merge_tab_mut();
    assert_eq!(tab.caret_position(), Some((2, 11, 0)));
    assert!(tab.caret_in_view());
    assert_eq!(tab.selected_text(), None);
}

#[test]
fn both_display_modes_and_wrapping_keep_the_panes_together() {
    let dir = tempfile::tempdir().unwrap();
    let mut harness = opened(dir.path());
    let ctx = harness.ctx.clone();
    harness.state_mut().run(&ctx, Command::ToggleAligned);
    settle(&mut harness);
    let last = *conflicts(&mut harness).last().unwrap();
    harness.state_mut().merge_tab_mut().show_chunk(last);
    harness.run_steps(3);
    save(&mut harness, "merge-aligned.png");
    harness.state_mut().run(&ctx, Command::ToggleWrap);
    harness.run_steps(3);
    harness.state_mut().run(&ctx, Command::ToggleAligned);
    harness.run_steps(3);
    assert!(harness.state_mut().merge_tab_mut().merge().next(None, Stop::Conflict).is_some());
}

/// Random keys and clicks in a merge tab, in every display mode: nothing may panic.
fn torture(seed: u64, steps: usize, aligned: bool, wrap: bool) {
    let dir = tempfile::tempdir().unwrap();
    let mut harness = opened(dir.path());
    harness.set_size(vec2(1000.0, 520.0));
    let ctx = harness.ctx.clone();
    if aligned {
        harness.state_mut().run(&ctx, Command::ToggleAligned);
    }
    if !wrap {
        harness.state_mut().run(&ctx, Command::ToggleWrap);
    }
    harness.state_mut().merge_tab_mut().put_caret(1, 3, 0);
    harness.step();
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut random = move |limit: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % limit
    };
    let keys = [
        Key::ArrowLeft, Key::ArrowRight, Key::ArrowUp, Key::ArrowDown, Key::Home, Key::End,
        Key::Backspace, Key::Delete, Key::Enter, Key::Tab, Key::PageUp, Key::PageDown,
    ];
    let resolutions = [Resolution::Left, Resolution::Right, Resolution::LeftThenRight, Resolution::RightThenLeft, Resolution::Base, Resolution::Merged, Resolution::Unresolved];
    for _ in 0..steps {
        if !matches!(harness.state().tabs.get(harness.state().active), Some(Tab::Merge(_))) || harness.state().confirm.is_some() {
            return;
        }
        match random(14) {
            0..=3 => {
                let text = ["é", "a", "日", " ", "<<<<<<< x\n", "x"][random(6) as usize];
                harness.event(egui::Event::Text(text.to_owned()));
            }
            4..=7 => harness.key_press(keys[random(keys.len() as u64) as usize]),
            8 => harness.key_press_modifiers(Modifiers::SHIFT, keys[random(6) as usize]),
            9 => match random(9) {
                0 => harness.key_press_modifiers(Modifiers::COMMAND, Key::Z),
                1 => harness.key_press_modifiers(Modifiers::COMMAND, Key::Y),
                2 => harness.key_press_modifiers(Modifiers::COMMAND, Key::ArrowDown),
                3 => harness.key_press_modifiers(Modifiers::COMMAND, Key::ArrowLeft),
                4 => harness.key_press_modifiers(Modifiers::COMMAND, Key::ArrowRight),
                5 => harness.event(egui::Event::Paste("deux\nlignes é\n".to_owned())),
                6 => harness.key_press(Key::F8),
                7 => harness.key_press_modifiers(Modifiers::ALT, Key::B),
                _ => harness.event(egui::Event::Cut),
            },
            10 => {
                let resolution = resolutions[random(resolutions.len() as u64) as usize];
                harness.state_mut().run(&ctx, Command::Resolve(resolution));
            }
            11 => {
                let ctx = harness.ctx.clone();
                harness.state_mut().run(&ctx, Command::EditWithMarkers);
            }
            _ => {
                if random(2) == 0 {
                    harness.state_mut().merge_tab_mut().put_caret(random(3) as usize, random(40) as u32, 0);
                } else {
                    let at = pos2(random(1000) as f32, 112.0 + random(370) as f32);
                    harness.hover_at(at);
                    harness.step();
                    harness.drag_at(at);
                    harness.step();
                    harness.drop_at(at);
                }
            }
        }
        harness.step();
    }
    // Whatever happened, undoing everything gives the merge as it opened.
    while harness.state_mut().merge_tab_mut().can_undo() {
        let ctx = harness.ctx.clone();
        harness.state_mut().merge_tab_mut().undo(&ctx);
    }
    harness.step();
    assert_eq!(harness.state_mut().merge_tab_mut().unresolved_count(), 2);
}

fn seeds() -> u64 {
    std::env::var("BIOMERGE_TORTURE_SEEDS").ok().and_then(|n| n.parse().ok()).unwrap_or(1)
}

#[test]
fn random_merging_never_panics() {
    (0..seeds()).for_each(|seed| torture(11 + seed * 10, 900, false, true));
}

#[test]
fn random_merging_never_panics_aligned_or_unwrapped() {
    (0..seeds()).for_each(|seed| torture(12 + seed * 10, 700, true, true));
    (0..seeds()).for_each(|seed| torture(13 + seed * 10, 700, false, false));
}

#[test]
fn a_merge_launch_hears_whether_the_result_was_saved_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("instance.sock");
    let mut harness = empty_harness();
    let ctx = harness.ctx.clone();
    let listener = crate::instance::Listener::bind(&socket).unwrap();
    harness.state_mut().listen(&ctx, listener);
    let launch = |inputs: MergeInputs| {
        let MergeInputs::Files { base, local, remote, merged, labels } = inputs else { unreachable!() };
        let request = Request { merge: Some(MergeRequest { base, local, remote, merged, labels }), wait: true, ..Request::default() };
        let client = socket.clone();
        let (done, finished) = channel();
        std::thread::spawn(move || {
            let handed = crate::instance::hand_over(&client, &request).expect("the window answers");
            let _ = done.send(handed.wait());
        });
        finished
    };

    // Closed without saving: still in conflict for git.
    let first = tempfile::tempdir().unwrap();
    let finished = launch(write_merge(first.path()));
    wait_for_tabs(&mut harness, 1);
    settle(&mut harness);
    assert_eq!(harness.state().tabs[0].title(), "facture.rs (fusion)");
    harness.state_mut().close_tab(0);
    harness.run_steps(2);
    assert_eq!(finished.recv_timeout(Duration::from_secs(5)), Ok(Some(1)));

    // Saved with every conflict resolved: success.
    let second = tempfile::tempdir().unwrap();
    let finished = launch(write_merge(second.path()));
    wait_for_tabs(&mut harness, 1);
    settle(&mut harness);
    for conflict in conflicts(&mut harness) {
        harness.state_mut().merge_tab_mut().resolve(&ctx, conflict, Resolution::Right);
    }
    harness.step();
    ctrl(&mut harness, Key::S);
    std::thread::sleep(Duration::from_millis(50));
    assert!(finished.try_recv().is_err(), "the launch waits for the tab");
    harness.state_mut().request_close(0);
    harness.run_steps(2);
    assert_eq!(finished.recv_timeout(Duration::from_secs(5)), Ok(Some(0)));
    assert!(std::fs::read_to_string(second.path().join("facture.rs")).unwrap().contains("amount * TAX_RATE"));
}

#[test]
fn a_single_file_in_conflict_opens_as_a_merge_whose_launch_hears_the_code() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("instance.sock");
    let mut harness = listening_harness(&socket);
    let conflicted = dir.path().join("facture.rs");
    std::fs::write(&conflicted, "fn tax() -> f64 {\n<<<<<<< HEAD\n    0.21\n=======\n    TAX_RATE\n>>>>>>> autre\n}\n").unwrap();

    // Closed without saving: still in conflict.
    let finished = launch(&socket, Request { single: Some(conflicted.clone()), wait: true, ..Request::default() });
    answered(&mut harness, &finished);
    wait_for_tabs(&mut harness, 1);
    assert!(harness.state().open_dialog.is_none(), "a merge, not the comparison dialog");
    assert_eq!(harness.state().tabs[0].title(), "facture.rs (fusion)");
    assert_eq!(unresolved(&mut harness), 1);
    save(&mut harness, "single-conflicted.png");
    std::thread::sleep(Duration::from_millis(100));
    assert!(finished.try_recv().is_err(), "the launch waits for the tab");
    harness.state_mut().close_tab(0);
    harness.run_steps(2);
    assert_eq!(finished.recv_timeout(Duration::from_secs(5)), Ok(Some(1)));

    // Resolved and saved: success, and the markers are gone from the file.
    let finished = launch(&socket, Request { single: Some(conflicted.clone()), wait: true, ..Request::default() });
    answered(&mut harness, &finished);
    wait_for_tabs(&mut harness, 1);
    let ctx = harness.ctx.clone();
    for conflict in conflicts(&mut harness) {
        harness.state_mut().merge_tab_mut().resolve(&ctx, conflict, Resolution::Right);
    }
    harness.step();
    ctrl(&mut harness, Key::S);
    harness.state_mut().request_close(0);
    harness.run_steps(2);
    assert_eq!(finished.recv_timeout(Duration::from_secs(5)), Ok(Some(0)));
    assert_eq!(std::fs::read_to_string(&conflicted).unwrap(), "fn tax() -> f64 {\n    TAX_RATE\n}\n");
}

fn click_at(harness: &mut Harness<'_, App>, pos: egui::Pos2, button: egui::PointerButton) {
    harness.hover_at(pos);
    harness.step();
    for pressed in [true, false] {
        harness.event(egui::Event::PointerButton { pos, button, pressed, modifiers: Modifiers::NONE });
        harness.step();
    }
    harness.run_steps(2);
}

#[test]
fn the_action_menu_opens_on_its_button_or_a_right_click_and_resolves() {
    use egui_kittest::kittest::Queryable;
    let dir = tempfile::tempdir().unwrap();
    let mut harness = opened(dir.path());
    let all = conflicts(&mut harness);
    // Hovering the button peeks at the base.
    let button = harness.state_mut().merge_tab_mut().menu_button_center(all[0]).unwrap();
    harness.hover_at(button);
    harness.run_steps(2);
    std::thread::sleep(Duration::from_millis(600));
    harness.run_steps(4);
    save(&mut harness, "merge-peek.png");
    click_at(&mut harness, button, egui::PointerButton::Primary);
    save(&mut harness, "merge-menu.png");
    harness.get_by_label("Prendre la base");
    harness.get_by_label("Prendre droite puis gauche").click();
    harness.run_steps(3);
    assert_eq!(harness.state_mut().merge_tab_mut().merge().chunks()[all[0]].resolution, Resolution::RightThenLeft);

    // A right click on the other conflict's lines, in any pane.
    let line = harness.state_mut().merge_tab_mut().merge().chunks()[all[1]].right.start;
    harness.state_mut().merge_tab_mut().show_chunk(all[1]);
    harness.run_steps(2);
    let at = harness.state_mut().merge_tab_mut().line_center(2, line).unwrap();
    click_at(&mut harness, at, egui::PointerButton::Secondary);
    harness.get_by_label("Combiner").click();
    harness.run_steps(3);
    assert_eq!(harness.state_mut().merge_tab_mut().merge().chunks()[all[1]].resolution, Resolution::Merged);
    // Markers by hand: the conflict counts as unresolved until they are gone.
    let ctx = harness.ctx.clone();
    harness.state_mut().run(&ctx, Command::EditWithMarkers);
    harness.run_steps(2);
    let tab = harness.state_mut().merge_tab_mut();
    assert!(tab.merge().chunks()[all[1]].has_markers);
    assert!(tab.result_text().contains("<<<<<<< ours\n        Kind::Cheque"), "{}", tab.result_text());
    assert_eq!(tab.unresolved_count(), 1);
}

#[test]
fn the_new_merge_dialog_takes_four_files() {
    use egui_kittest::kittest::Queryable;
    let dir = tempfile::tempdir().unwrap();
    let MergeInputs::Files { base, local, remote, merged, .. } = write_merge(dir.path()) else { unreachable!() };
    let mut harness = empty_harness();
    let ctx = harness.ctx.clone();
    harness.state().install_look(&ctx);
    harness.state_mut().run(&ctx, Command::NewMerge);
    harness.run_steps(2);
    harness.get_by_label("Fusionner").click();
    harness.run_steps(2);
    assert!(harness.state().merge_dialog.as_ref().is_some_and(|dialog| dialog.error.is_some()), "nothing given");
    let paths = [base, local, remote, merged].map(|path| path.display().to_string());
    harness.state_mut().merge_dialog.as_mut().unwrap().paths = paths;
    harness.run_steps(2);
    save(&mut harness, "merge-dialog.png");
    harness.get_by_label("Fusionner").click();
    wait_for_tabs(&mut harness, 1);
    settle(&mut harness);
    assert_eq!(harness.state_mut().merge_tab_mut().unresolved_count(), 2);
    harness.get_by_label("Menu").click();
    harness.run_steps(3);
    harness.get_by_label("Fusion").hover();
    harness.run_steps(6);
    save(&mut harness, "merge-hamburger.png");
    harness.get_by_label("Conflit non résolu suivant");
}

#[test]
fn a_file_deleted_on_one_side_says_so_and_saving_it_empty_is_asked() {
    use egui_kittest::kittest::Queryable;
    let dir = tempfile::tempdir().unwrap();
    let MergeInputs::Files { base, remote, merged, labels, .. } = write_merge(dir.path()) else { unreachable!() };
    // Deleted locally: git passes a file that does not exist.
    let inputs = MergeInputs::Files { base, local: dir.path().join("absent_LOCAL.rs"), remote, merged: merged.clone(), labels };
    let mut harness = harness_for_merge(inputs);
    harness.step();
    settle(&mut harness);
    harness.get_by_label("absent");
    save(&mut harness, "merge-deleted.png");
    assert_eq!(unresolved(&mut harness), 1, "deleted here, changed there");
    let ctx = harness.ctx.clone();
    harness.state_mut().run(&ctx, Command::Resolve(Resolution::Left));
    harness.run_steps(2);
    assert!(harness.state_mut().merge_tab_mut().result_text().is_empty());
    ctrl(&mut harness, Key::S);
    assert!(harness.state_mut().merge_tab_mut().wants_dialog());
    harness.get_by_label("Enregistrer le fichier vide").click();
    harness.run_steps(2);
    assert_eq!(std::fs::read_to_string(&merged).unwrap(), "");
    assert_eq!(harness.state_mut().merge_tab_mut().exit_code(), 0);
}
