//! The crash-recovery prompt and the Revert confirmation (Settings ▸ Auto Save lives in
//! `panels::settings`).
//!
//! Automation ids: `recovery.item.<n>`, `recovery.recover`, `recovery.discard`, `recovery.later`;
//! `revert.yes`, `revert.no`.

use egui::{Color32, CornerRadius, Frame, Margin, RichText, Stroke, Ui, vec2};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::theme::Tokens;
use crate::{Dialog, FilmcraftApp};

/// Dialog-local state.
#[derive(Default)]
pub struct FileDialogState {
    pub recovery_choice: usize,
    pending: Option<PendingReplacement>,
}

struct PendingReplacement {
    command: String,
    params: Value,
    project: Arc<filmcraft_project::Project>,
    revision: u64,
}

/// A cancelled picker is never a successful save, even though it is not an error.
pub(crate) enum SaveOutcome {
    Saved(Value),
    Cancelled,
    Failed(String),
}

impl SaveOutcome {
    pub(crate) fn into_result(self) -> Result<Value, String> {
        match self {
            Self::Saved(mut value) => {
                value["outcome"] = json!("Saved");
                Ok(value)
            }
            Self::Cancelled => Ok(json!({"outcome":"Cancelled"})),
            Self::Failed(error) => Err(error),
        }
    }
}

pub(crate) fn save_project(app: &mut FilmcraftApp, command: &str, params: &Value) -> SaveOutcome {
    if !params.is_object() {
        app.ui.status = "project save params must be an object".into();
        return SaveOutcome::Failed(app.ui.status.clone());
    }
    let mut params = params.clone();
    let command = if command == "file.saveAll" { "file.save" } else { command };
    let mut actual = command;
    if params.get("path").is_none() && (command != "file.save" || app.session.path.is_none()) {
        let suggested =
            if command == "file.saveCopy" { format!("{} copy.fcproj", app.session.project.name) } else { format!("{}.fcproj", app.session.project.name) };
        let Some(path) = app.hooks.pick_save.as_mut().and_then(|f| f(&suggested)) else {
            app.ui.status = tl!("Save cancelled.").into();
            return SaveOutcome::Cancelled;
        };
        params["path"] = json!(path);
        if command != "file.saveCopy" {
            actual = "file.saveAs";
        }
    }
    match app.session.execute(actual, params) {
        Ok(value) => {
            app.ui.status = if command == "file.saveCopy" { tl!("Project copy saved.") } else { tl!("Project saved.") }.into();
            SaveOutcome::Saved(value)
        }
        Err(error) => {
            let error = error.to_string();
            app.ui.status = error.clone();
            SaveOutcome::Failed(error)
        }
    }
}

/// Shared menu/control entry for project saves and replacement consent.
pub(crate) fn route(app: &mut FilmcraftApp, command: &str, params: &Value) -> Option<Result<Value, String>> {
    if (filmcraft_engine::is_project_replacement(command) || matches!(command, "file.save" | "file.saveAs" | "file.saveCopy" | "file.saveAll"))
        && !params.is_object()
    {
        return Some(Err("project command params must be an object".into()));
    }
    if matches!(command, "file.save" | "file.saveAs" | "file.saveCopy" | "file.saveAll") {
        return Some(save_project(app, command, params).into_result());
    }
    if !filmcraft_engine::is_project_replacement(command) {
        return None;
    }
    if command == "file.recover" && params.as_object().is_none_or(|p| p.is_empty()) {
        return None;
    }
    if command == "file.open" && params.get("path").is_none() {
        return Some(app.file_dialog(command, params));
    }
    Some(request_replacement(app, command, params.clone()))
}

pub(crate) fn request_replacement(app: &mut FilmcraftApp, command: &str, params: Value) -> Result<Value, String> {
    if !params.is_object() {
        return Err("project replacement params must be an object".into());
    }
    filmcraft_engine::find_command(command).map_or(Ok(()), |c| (c.enabled)(&app.session))?;
    if app.session.is_dirty() && params["force"] != true {
        app.file_dialogs.pending =
            Some(PendingReplacement { command: command.into(), params, project: app.session.project.clone(), revision: app.session.revision });
        app.dialog = Some(Dialog::ProjectReplacement);
        return Ok(json!({"dialog":"projectReplacement"}));
    }
    let result = app.session.execute(command, params).map_err(|e| e.to_string());
    if let Err(error) = &result {
        app.ui.status = error.clone();
    }
    result
}

fn finish_replacement(app: &mut FilmcraftApp, choice: &str) -> Result<Value, String> {
    let Some(mut pending) = app.file_dialogs.pending.take() else {
        return Err("no project replacement is pending".into());
    };
    app.dialog = None;
    if choice == "cancel" {
        return Ok(json!({"outcome":"Cancelled"}));
    }
    if pending.revision != app.session.revision || !Arc::ptr_eq(&pending.project, &app.session.project) {
        let error = tl!("The project changed. Start the action again to confirm its current state.").to_string();
        app.ui.status = error.clone();
        return Err(error);
    }
    match choice {
        "save" => match save_project(app, "file.save", &json!({})) {
            SaveOutcome::Saved(_) => {}
            outcome => return outcome.into_result(),
        },
        "discard" => pending.params["force"] = json!(true),
        _ => return Err("unknown project replacement choice".into()),
    }
    // Only Saved reaches here from the Save choice. Discard grants this captured action consent.
    request_replacement(app, &pending.command, pending.params)
}

fn show_replacement(app: &mut FilmcraftApp, ctx: &egui::Context) -> bool {
    let Some(pending) = &app.file_dialogs.pending else {
        return false;
    };
    let name = pending.project.name.clone();
    let action = filmcraft_engine::find_command(&pending.command).map_or(pending.command.as_str(), |c| c.label).to_string();
    let t = app.tokens;
    let mut choice = None;
    let modal = egui::Modal::new(egui::Id::new("project-replacement-modal")).frame(modal_frame(&t)).show(ctx, |ui| {
        ui.set_width(480.0);
        Frame::new().inner_margin(Margin { left: 28, right: 28, top: 24, bottom: 20 }).show(ui, |ui| {
            ui.label(RichText::new(tl!("Save Project")).size(18.0).strong().color(t.text));
            ui.add_space(8.0);
            ui.label(tlf!("Save changes to '{name}' before continuing?", name));
            ui.label(crate::i18n::t(&action));
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                if button(app, ui, "project.replace.discard", tl!("Discard"), false) {
                    choice = Some("discard");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if button(app, ui, "project.replace.save", tl!("Save"), true) {
                        choice = Some("save");
                    }
                    if button(app, ui, "project.replace.cancel", tl!("Cancel"), false) {
                        choice = Some("cancel");
                    }
                });
            });
        });
    });
    if let Some(choice) = choice.or_else(|| modal.should_close().then_some("cancel")) {
        if let Err(error) = finish_replacement(app, choice) {
            app.ui.status = error;
        }
        return false;
    }
    true
}

fn modal_frame(t: &Tokens) -> Frame {
    Frame::new().fill(t.panel_bg).stroke(Stroke::new(1.0, t.separator)).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(0))
}

fn button(app: &mut FilmcraftApp, ui: &mut Ui, id: &str, label: &str, primary: bool) -> bool {
    let t = app.tokens;
    let text = RichText::new(label).size(13.0).color(if primary { Color32::WHITE } else { t.text });
    let b = egui::Button::new(text)
        .min_size(vec2(88.0, 30.0))
        .corner_radius(CornerRadius::same(15))
        .fill(if primary { t.accent } else { Color32::TRANSPARENT })
        .stroke(if primary { Stroke::NONE } else { Stroke::new(1.5, t.text_faint) });
    let r = ui.add(b);
    app.auto.add(id, r.rect, label);
    r.clicked()
}

pub fn show_recovery(app: &mut FilmcraftApp, ctx: &egui::Context) -> bool {
    let t = app.tokens;
    let list = app.session.execute("file.recoveryList", json!({})).unwrap_or_default();
    let items = list.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return false;
    }
    let mut choice = app.file_dialogs.recovery_choice.min(items.len() - 1);
    let mut open = true;
    egui::Modal::new(egui::Id::new("recovery-modal")).frame(modal_frame(&t)).show(ctx, |ui| {
        ui.set_width(520.0);
        Frame::new().inner_margin(Margin { left: 28, right: 28, top: 24, bottom: 20 }).show(ui, |ui| {
            ui.label(RichText::new(tl!("Recover Unsaved Changes")).size(18.0).strong().color(t.text));
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);
            let c = &items[choice];
            let name = c["projectName"].as_str().unwrap_or(tl!("Untitled"));
            let when = c["savedAt"].as_str().unwrap_or("");
            let why = if c["cleanExit"].as_bool() == Some(true) {
                tlf!("FilmCraft was closed while “{name}” had unsaved changes.", name)
            } else {
                tlf!("FilmCraft quit unexpectedly while “{name}” had unsaved changes.", name)
            };
            ui.label(RichText::new(why).size(13.5).color(t.text));
            ui.add_space(4.0);
            ui.label(RichText::new(tlf!("Recover unsaved changes from {when}?", when)).size(13.5).color(t.text));
            if let Some(p) = c["projectPath"].as_str() {
                ui.label(RichText::new(p).size(11.5).color(t.text_dim));
            } else {
                ui.label(RichText::new(tl!("The project had not been saved yet.")).size(11.5).color(t.text_dim));
            }
            if items.len() > 1 {
                ui.add_space(8.0);
                ui.label(RichText::new(tlf!("{n} sessions have unsaved changes:", n = items.len())).size(12.0).color(t.text_dim));
                for (i, it) in items.iter().enumerate() {
                    let label = format!("{} — {}", it["projectName"].as_str().unwrap_or(tl!("Untitled")), it["savedAt"].as_str().unwrap_or(""));
                    let r = ui.radio(choice == i, RichText::new(&label).size(12.5));
                    app.auto.add(&format!("recovery.item.{i}"), r.rect, &label);
                    if r.clicked() {
                        choice = i;
                    }
                }
            }
            ui.add_space(18.0);
            let id = items[choice]["id"].as_str().unwrap_or("").to_string();
            ui.horizontal(|ui| {
                if button(app, ui, "recovery.discard", tl!("Discard"), false) {
                    if let Err(e) = app.session.execute("file.discardRecovery", json!({ "id": id })) {
                        app.ui.status = e.to_string();
                    }
                    choice = 0;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if button(app, ui, "recovery.recover", tl!("Recover"), true) {
                        match request_replacement(app, "file.recover", json!({ "id": id })) {
                            Ok(_) => open = false,
                            Err(e) => app.ui.status = e.to_string(),
                        }
                    }
                    if button(app, ui, "recovery.later", tl!("Not Now"), false) {
                        open = false;
                    }
                });
            });
        });
    });
    app.file_dialogs.recovery_choice = choice;
    open && !app.session.recovery_candidates().is_empty()
}

pub fn show_revert(app: &mut FilmcraftApp, ctx: &egui::Context) -> bool {
    let t = app.tokens;
    let file = app.session.path.as_deref().and_then(|p| std::path::Path::new(p).file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut open = true;
    let r = egui::Modal::new(egui::Id::new("revert-modal")).frame(modal_frame(&t)).show(ctx, |ui| {
        ui.set_width(460.0);
        Frame::new().inner_margin(Margin { left: 28, right: 28, top: 24, bottom: 20 }).show(ui, |ui| {
            ui.label(RichText::new(tl!("Revert")).size(18.0).strong().color(t.text));
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(RichText::new(tlf!("Are you sure you want to discard your changes to '{file}'?", file)).size(13.5).color(t.text));
            ui.add_space(18.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if button(app, ui, "revert.yes", tl!("Yes"), true) {
                    if let Err(e) = request_replacement(app, "file.revert", json!({})) {
                        app.ui.status = e.to_string();
                    }
                    open = false;
                }
                if button(app, ui, "revert.no", tl!("No"), false) {
                    open = false;
                }
            });
        });
    });
    open && !r.should_close()
}

/// Draw the dialog if it is one of ours; returns Some(still open).
pub fn show(app: &mut FilmcraftApp, ctx: &egui::Context, d: Dialog) -> Option<bool> {
    match d {
        Dialog::Preferences => Some(crate::panels::settings::show(app, ctx)),
        Dialog::Recovery => Some(show_recovery(app, ctx)),
        Dialog::RevertConfirm => Some(show_revert(app, ctx)),
        Dialog::ProjectReplacement => Some(show_replacement(app, ctx)),
        _ => None,
    }
}

#[cfg(test)]
mod safety_tests {
    use super::*;
    use filmcraft_engine::{Services, Session};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Files {
        files: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
        fail: Mutex<bool>,
    }
    impl Services for Files {
        fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
            self.files.lock().unwrap().get(path).cloned().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, path))
        }
        fn write_file(&self, path: &str, bytes: &[u8]) -> std::io::Result<()> {
            if *self.fail.lock().unwrap() {
                return Err(std::io::Error::other("synthetic save failure"));
            }
            self.files.lock().unwrap().insert(path.into(), bytes.to_vec());
            Ok(())
        }
    }
    fn app(saved: bool) -> (FilmcraftApp, Arc<Files>) {
        crate::i18n::set_current(crate::i18n::Language::En);
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        let files = Arc::new(Files::default());
        s.services = files.clone();
        if saved {
            s.execute("file.saveAs", json!({"path":"Original.fcproj"})).unwrap();
        }
        s.execute("sequence.addEdit", json!({"seconds":1.0})).unwrap();
        (FilmcraftApp::new(s), files)
    }
    fn snapshot(app: &FilmcraftApp) -> Value {
        json!({"project":*app.session.project,"state":app.session.state,"revision":app.session.revision,"saved":app.session.saved_revision,"path":app.session.path,
            "undo":app.session.history.undo.iter().map(|(label,p)|json!([label,**p])).collect::<Vec<_>>(),
            "redo":app.session.history.redo.iter().map(|(label,p)|json!([label,**p])).collect::<Vec<_>>()})
    }
    #[test]
    fn safety_replacement_cancel_covers_shared_menu_routes() {
        for command in ["file.newProject", "file.openDemoProject", "file.open", "file.revert", "file.closeProject", "file.closeAllProjects"] {
            let (mut app, _) = app(true);
            let before = snapshot(&app);
            let arc = app.session.project.clone();
            let r = crate::menus::invoke(&mut app, &egui::Context::default(), command, json!({"path":"Original.fcproj"})).unwrap();
            assert_eq!(r["dialog"], "projectReplacement", "{command}");
            assert_eq!(finish_replacement(&mut app, "cancel").unwrap()["outcome"], "Cancelled");
            assert!(snapshot(&app) == before, "{command} cancel changed state");
            assert!(Arc::ptr_eq(&arc, &app.session.project));
            assert!(app.file_dialogs.pending.is_none());
        }
    }
    #[test]
    fn safety_cancelled_or_failed_save_never_replaces_unsaved_project() {
        for command in ["file.newProject", "file.closeProject"] {
            for fail in [false, true] {
                let (mut app, files) = app(false);
                let before = snapshot(&app);
                let arc = app.session.project.clone();
                app.hooks.pick_save = Some(Box::new(move |_| fail.then(|| "Chosen.fcproj".into())));
                *files.fail.lock().unwrap() = fail;
                request_replacement(&mut app, command, json!({})).unwrap();
                let result = finish_replacement(&mut app, "save");
                if fail {
                    assert!(result.unwrap_err().contains("synthetic save failure"));
                    assert!(app.ui.status.contains("synthetic save failure"));
                } else {
                    assert_eq!(result.unwrap()["outcome"], "Cancelled");
                    assert_eq!(app.ui.status, "Save cancelled.");
                }
                assert!(snapshot(&app) == before);
                assert!(Arc::ptr_eq(&arc, &app.session.project));
                assert!(files.files.lock().unwrap().is_empty());
            }
        }
    }
    #[test]
    fn safety_unsaved_close_save_uses_picker_and_only_saved_continues() {
        let (mut app, files) = app(false);
        let old = (*app.session.project).clone();
        let picks = Arc::new(Mutex::new(Vec::new()));
        let capture = picks.clone();
        app.hooks.pick_save = Some(Box::new(move |suggested| {
            capture.lock().unwrap().push(suggested.to_string());
            Some("Chosen.fcproj".into())
        }));
        crate::menus::invoke(&mut app, &egui::Context::default(), "file.closeProject", json!({})).unwrap();
        assert_eq!(finish_replacement(&mut app, "save").unwrap()["closed"], true);
        assert_eq!(picks.lock().unwrap().len(), 1);
        let mut reopened = Session::default();
        reopened.services = files.clone();
        reopened.execute("file.open", json!({"path":"Chosen.fcproj"})).unwrap();
        let saved = (*reopened.project).clone();
        let mut expected = old;
        expected.name = "Chosen".into();
        expected.root.name = "Chosen".into();
        assert_eq!(saved, expected);
        assert_eq!(app.session.project.name, "Untitled");
        assert!(app.session.path.is_none());
        assert!(!app.session.is_dirty());
    }
    #[test]
    fn safety_discard_invalid_candidate_preserves_project_and_demo_resets_path() {
        let (mut app, _) = app(true);
        let before = snapshot(&app);
        let arc = app.session.project.clone();
        request_replacement(&mut app, "file.open", json!({"path":"missing.fcproj"})).unwrap();
        assert!(finish_replacement(&mut app, "discard").is_err());
        assert!(snapshot(&app) == before);
        assert!(Arc::ptr_eq(&arc, &app.session.project));
        request_replacement(&mut app, "file.openDemoProject", json!({})).unwrap();
        finish_replacement(&mut app, "discard").unwrap();
        assert!(app.session.path.is_none());
        assert!(!app.session.is_dirty());
    }
    #[test]
    fn safety_stale_replacement_consent_checks_revision_and_arc() {
        for same_revision in [false, true] {
            let (mut app, _) = app(true);
            request_replacement(&mut app, "file.newProject", json!({})).unwrap();
            let old_revision = app.session.revision;
            if same_revision {
                app.session.execute("file.saveAs", json!({"path":"Changed.fcproj"})).unwrap();
                assert_eq!(app.session.revision, old_revision);
            } else {
                app.session.execute("sequence.addEdit", json!({"seconds":2.0})).unwrap();
            }
            let before = snapshot(&app);
            let arc = app.session.project.clone();
            assert!(finish_replacement(&mut app, "discard").unwrap_err().contains("The project changed"));
            assert!(snapshot(&app) == before);
            assert!(Arc::ptr_eq(&arc, &app.session.project));
            assert!(app.file_dialogs.pending.is_none());
        }
    }
    #[test]
    fn safety_save_results_and_open_picker_cancel_are_explicit() {
        let (mut app, files) = app(false);
        let before = snapshot(&app);
        let ctx = egui::Context::default();
        assert_eq!(crate::menus::invoke(&mut app, &ctx, "file.save", json!({})).unwrap()["outcome"], "Cancelled");
        assert_eq!(crate::menus::invoke(&mut app, &ctx, "file.open", json!({})).unwrap()["outcome"], "Cancelled");
        assert!(snapshot(&app) == before);
        assert!(crate::menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path":"Saved.fcproj"})).unwrap()["outcome"] == "Saved");
        assert!(!app.session.is_dirty());
        *files.fail.lock().unwrap() = true;
        assert!(crate::menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path":"Failed.fcproj"})).unwrap_err().contains("synthetic save failure"));
        assert!(app.ui.status.contains("synthetic save failure"));
    }
    #[test]
    fn safety_project_prompts_have_retained_translations() {
        for language in [crate::i18n::Language::ZhTw, crate::i18n::Language::Ja, crate::i18n::Language::Es] {
            crate::i18n::set_current(language);
            for text in [
                "Save cancelled.",
                "Project saved.",
                "Project copy saved.",
                "The project changed. Start the action again to confirm its current state.",
                "Save changes to '{name}' before continuing?",
            ] {
                assert_ne!(crate::i18n::t(text), text, "{language:?}: {text}");
            }
        }
        crate::i18n::set_current(crate::i18n::Language::En);
    }
    #[test]
    fn safety_hostile_project_params_never_invoke_picker_or_change_pending() {
        let (mut app, _) = app(false);
        request_replacement(&mut app, "file.newProject", json!({})).unwrap();
        let before = snapshot(&app);
        let arc = app.session.project.clone();
        let picks = Arc::new(Mutex::new(0));
        let capture = picks.clone();
        app.hooks.pick_open_project = Some(Box::new(move || {
            *capture.lock().unwrap() += 1;
            Some("hostile.fcproj".into())
        }));
        for params in [Value::Null, json!("string"), json!([]), json!(3)] {
            for command in ["file.open", "file.recover", "file.saveAs"] {
                assert!(crate::menus::invoke(&mut app, &egui::Context::default(), command, params.clone()).is_err());
            }
            assert!(app.file_dialog("file.open", &params).is_err());
            assert_eq!(app.file_dialogs.pending.as_ref().unwrap().command, "file.newProject");
            assert_eq!(app.dialog, Some(Dialog::ProjectReplacement));
            assert!(snapshot(&app) == before);
            assert!(Arc::ptr_eq(&arc, &app.session.project));
        }
        assert_eq!(*picks.lock().unwrap(), 0);
    }
}
