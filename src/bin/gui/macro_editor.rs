use super::*;

#[derive(Clone, Copy)]
struct DragStep(usize);

pub fn select(ui: &mut egui::Ui, id: &str, value: &mut Value, options: &[(&str, &str)]) {
    let mut current = value.as_str().unwrap_or("").to_owned();
    egui::ComboBox::from_id_salt(id)
        .selected_text(
            options
                .iter()
                .find(|(key, _)| *key == current)
                .map(|(_, label)| *label)
                .unwrap_or(&current),
        )
        .show_ui(ui, |ui| {
            for (key, label) in options {
                ui.selectable_value(&mut current, (*key).into(), *label);
            }
        });
    *value = json!(current);
}
pub fn number(
    ui: &mut egui::Ui,
    value: &mut Value,
    range: std::ops::RangeInclusive<i64>,
    suffix: &str,
) {
    let mut n = value.as_i64().unwrap_or(0);
    if ui
        .add(egui::DragValue::new(&mut n).range(range).suffix(suffix))
        .changed()
    {
        *value = json!(n);
    }
}

pub fn sequence(ui: &mut egui::Ui, draft: &mut Value, capture_step: &mut Option<usize>) {
    let steps = draft["steps"].as_array_mut().unwrap();
    let mut clear_all = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("ADD ACTION").small().color(MUTED));
        for (label, kind, args) in [
            ("Key", "key", json!({"combo":"a"})),
            ("Text", "text", json!({"text":""})),
            ("Wait", "delay", json!({"ms":100})),
            ("Mouse", "click", json!({"button":"left"})),
            ("Scroll", "scroll", json!({"amount":1})),
            ("Move", "move", json!({"dx":0,"dy":0})),
            ("Media", "media", json!({"key":"play-pause"})),
        ] {
            if ui.button(format!("+ {label}")).clicked() {
                steps.push(json!({"kind":kind,"args":args}));
            }
        }
        if !steps.is_empty() && ui.button("Clear all").clicked() {
            clear_all = true;
        }
    });
    if clear_all {
        steps.clear();
        *capture_step = None;
    }
    ui.add_space(12.);
    ui.label(
        RichText::new(format!(
            "{} actions · drag the handle to reorder · all changes stay in this draft",
            steps.len()
        ))
        .color(MUTED)
        .size(12.),
    );
    ui.add_space(8.);
    let len = steps.len();
    let mut reorder = None;
    let mut remove = None;
    let mut duplicate = None;
    egui::ScrollArea::vertical()
        .id_salt("editable_sequence")
        .max_height((ui.available_height() - 135.).max(180.))
        .min_scrolled_height(100.)
        .show(ui, |ui| {
            if steps.is_empty() {
                ui.add_space(24.);
                ui.label(RichText::new("Build your sequence").size(20.));
                ui.label("Add an action above, then edit its settings here.");
                ui.add_space(24.);
            }
            for (i, step) in steps.iter_mut().enumerate() {
                ui.push_id(i, |ui| {
                    let card = egui::Frame::new()
                        .fill(RAISED)
                        .corner_radius(8)
                        .inner_margin(12)
                        .show(ui, |ui| {
                            ui.set_min_width((ui.available_width() - 4.).max(100.));
                            ui.horizontal(|ui| {
                                let handle = ui.add(
                                    egui::Label::new(RichText::new("⠿").size(22.).color(MUTED))
                                        .sense(egui::Sense::drag()),
                                );
                                handle.dnd_set_drag_payload(DragStep(i));
                                handle.on_hover_text("Drag to reorder");
                                ui.label(RichText::new(format!("{:02}", i + 1)).color(ACCENT));
                                let kind = step["kind"].as_str().unwrap_or("").to_owned();
                                ui.label(
                                    RichText::new(match kind.as_str() {
                                        "delay" => "Wait",
                                        "text" => "Type text",
                                        "key" | "key-down" | "key-up" => "Keyboard",
                                        "click" | "button-down" | "button-up" => "Mouse button",
                                        "scroll" | "hscroll" => "Scroll",
                                        "move" => "Move pointer",
                                        "media" => "Media",
                                        _ => "Action",
                                    })
                                    .strong(),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button("Delete").clicked() {
                                            remove = Some(i);
                                        }
                                        if ui.small_button("Duplicate").clicked() {
                                            duplicate = Some(i);
                                        }
                                        if ui
                                            .add_enabled(
                                                i + 1 < len,
                                                egui::Button::new("↓").small(),
                                            )
                                            .clicked()
                                        {
                                            reorder = Some((i, i + 1));
                                        }
                                        if ui
                                            .add_enabled(i > 0, egui::Button::new("↑").small())
                                            .clicked()
                                        {
                                            reorder = Some((i, i - 1));
                                        }
                                    },
                                );
                            });
                            ui.add_space(6.);
                            ui.horizontal_wrapped(|ui| {
                                match step["kind"].as_str().unwrap_or("").to_owned().as_str() {
                                    "delay" => {
                                        ui.label("Duration");
                                        number(ui, &mut step["args"]["ms"], 0..=600000, " ms");
                                    }
                                    "text" => {
                                        let mut text =
                                            step["args"]["text"].as_str().unwrap_or("").to_owned();
                                        if ui
                                            .add(
                                                egui::TextEdit::multiline(&mut text)
                                                    .desired_rows(2)
                                                    .desired_width(f32::INFINITY)
                                                    .hint_text("Text to type…"),
                                            )
                                            .changed()
                                        {
                                            step["args"]["text"] = json!(text);
                                        }
                                    }
                                    "key" | "key-down" | "key-up" => {
                                        select(
                                            ui,
                                            "key_mode",
                                            &mut step["kind"],
                                            &[
                                                ("key", "Press & release"),
                                                ("key-down", "Press"),
                                                ("key-up", "Release"),
                                            ],
                                        );
                                        let capturing = *capture_step == Some(i);
                                        if ui
                                            .button(if capturing {
                                                "Press key… (Esc)"
                                            } else {
                                                "Capture"
                                            })
                                            .clicked()
                                        {
                                            *capture_step = if capturing { None } else { Some(i) };
                                        }
                                        let mut combo_str =
                                            step["args"]["combo"].as_str().unwrap_or("").to_owned();
                                        if ui
                                            .add(
                                                egui::TextEdit::singleline(&mut combo_str)
                                                    .desired_width(160.)
                                                    .hint_text("e.g. ctrl+c"),
                                            )
                                            .changed()
                                        {
                                            step["args"]["combo"] = json!(combo_str);
                                        }
                                        if let Err(err) = input::combo(&combo_str) {
                                            ui.label(
                                                RichText::new(err.to_string())
                                                    .small()
                                                    .color(Color32::from_rgb(255, 90, 95)),
                                            );
                                        }
                                    }
                                    "click" | "button-down" | "button-up" => {
                                        select(
                                            ui,
                                            "mouse_mode",
                                            &mut step["kind"],
                                            &[
                                                ("click", "Click"),
                                                ("button-down", "Press"),
                                                ("button-up", "Release"),
                                            ],
                                        );
                                        select(
                                            ui,
                                            "button",
                                            &mut step["args"]["button"],
                                            &[
                                                ("left", "Left"),
                                                ("right", "Right"),
                                                ("middle", "Middle"),
                                                ("back", "Back"),
                                                ("forward", "Forward"),
                                            ],
                                        );
                                    }
                                    "scroll" | "hscroll" => {
                                        select(
                                            ui,
                                            "axis",
                                            &mut step["kind"],
                                            &[("scroll", "Vertical"), ("hscroll", "Horizontal")],
                                        );
                                        number(
                                            ui,
                                            &mut step["args"]["amount"],
                                            -1000..=1000,
                                            " steps",
                                        );
                                    }
                                    "move" => {
                                        ui.label("X");
                                        number(ui, &mut step["args"]["dx"], -32768..=32767, " px");
                                        ui.label("Y");
                                        number(ui, &mut step["args"]["dy"], -32768..=32767, " px");
                                    }
                                    "media" => {
                                        select(ui, "media", &mut step["args"]["key"], parity::MEDIA)
                                    }
                                    _ => {
                                        ui.label("Unsupported action — delete or replace it.");
                                    }
                                }
                            });
                        });
                    if let Some(from) = card.response.dnd_release_payload::<DragStep>() {
                        reorder = Some((from.0, i));
                    }
                    if card.response.dnd_hover_payload::<DragStep>().is_some() {
                        ui.painter().rect_stroke(
                            card.response.rect,
                            8.,
                            egui::Stroke::new(1.0f32, ACCENT),
                            egui::StrokeKind::Inside,
                        );
                    }
                    ui.add_space(8.);
                });
            }
        });
    if let Some(i) = remove {
        steps.remove(i);
        *capture_step = None;
    } else if let Some(i) = duplicate {
        steps.insert(i + 1, steps[i].clone());
        *capture_step = None;
    } else if let Some((from, to)) = reorder
        && from < len
        && to < len
        && from != to
    {
        let item = steps.remove(from);
        steps.insert(to, item);
        *capture_step = None;
    }
}
