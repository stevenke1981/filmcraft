//! `.fcproj` save/open through the command interface: schema envelope, legacy upgrade, Save a Copy,
//! Revert.

use super::*;
use serde_json::json;

fn demo() -> Session {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn saved_file_is_versioned_and_save_copy_keeps_state() {
    let d = temp_dir("schema");
    let path = d.join("p.fcproj").to_string_lossy().to_string();
    let mut s = demo();
    s.execute("file.saveAs", json!({"path": path})).unwrap();
    let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(v["schema_version"], filmcraft_format::SCHEMA_VERSION);
    assert_eq!(v["format"], "filmcraft.project");
    s.execute("sequence.addEdit", json!({"seconds": 1.0})).unwrap();
    assert!(s.is_dirty());
    let copy = d.join("copy.fcproj").to_string_lossy().to_string();
    s.execute("file.saveCopy", json!({"path": copy})).unwrap();
    assert!(s.is_dirty(), "Save a Copy leaves the project unsaved");
    assert_eq!(s.path.as_deref(), Some(path.as_str()));
    let mut t = Session::default();
    t.execute("file.open", json!({"path": copy})).unwrap();
    assert_eq!(*t.project, *s.project);
    // Revert discards the edit.
    s.execute("file.revert", json!({"force":true})).unwrap();
    assert!(!s.is_dirty());
    assert_ne!(*t.project, *s.project);
    assert!(!Session::default().is_enabled("file.revert"), "nothing to revert to before a save");
    // No temp files left next to the project.
    let stray: Vec<_> = std::fs::read_dir(&d).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).collect();
    assert!(stray.is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn legacy_project_opens_and_first_save_keeps_a_backup() {
    let d = temp_dir("legacy");
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../format/tests/fixtures/v1-edit.fcproj");
    let path = d.join("old.fcproj");
    std::fs::copy(&fixture, &path).unwrap();
    let p = path.to_string_lossy().to_string();
    let mut s = Session::default();
    let r = s.execute("file.open", json!({"path": p})).unwrap();
    assert_eq!(r["schemaVersion"], 1);
    assert_eq!(r["migrated"], true);
    assert_eq!(s.project.name, "Legacy Edit");
    assert!(s.drain_events().iter().any(|e| matches!(e, Event::Toast { message, .. } if message.contains("Upgraded"))));
    let r = s.execute("file.save", json!({})).unwrap();
    let backup = r["backup"].as_str().unwrap().to_string();
    assert!(backup.ends_with("old (schema v1 backup).fcproj"), "{backup}");
    assert_eq!(std::fs::read(&backup).unwrap(), std::fs::read(&fixture).unwrap());
    let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(v["schema_version"], filmcraft_format::SCHEMA_VERSION);
    // Second save: no new backup.
    assert!(s.execute("file.save", json!({})).unwrap()["backup"].is_null());
    // A file from the future is refused clearly.
    let mut v = v;
    v["schema_version"] = json!(filmcraft_format::SCHEMA_VERSION + 1);
    std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    let e = Session::default().execute("file.open", json!({"path": p})).unwrap_err().to_string();
    assert!(e.contains("newer version of FilmCraft"), "{e}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn project_is_named_after_its_file() {
    let d = temp_dir("name");
    let mut s = demo();
    let a = d.join("Apollo 11 - Tranquility.fcproj").to_string_lossy().to_string();
    s.execute("file.saveAs", json!({"path": a})).unwrap();
    assert_eq!(s.project.name, "Apollo 11 - Tranquility");
    assert_eq!(s.project.root.name, "Apollo 11 - Tranquility");
    assert!(!s.is_dirty(), "renaming on Save As is not an edit");
    // Save a Copy keeps the current name.
    let c = d.join("Backup copy.fcproj").to_string_lossy().to_string();
    s.execute("file.saveCopy", json!({"path": c})).unwrap();
    assert_eq!(s.project.name, "Apollo 11 - Tranquility");
    s.execute("file.open", json!({"path": c})).unwrap();
    assert_eq!(s.project.name, "Apollo 11 - Tranquility");
    let _ = std::fs::remove_dir_all(&d);
}

#[derive(Default)]
struct SafetyFs {
    files: std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
    fail: std::sync::Mutex<Option<String>>,
}

impl Services for SafetyFs {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        self.files.lock().unwrap().get(path).cloned().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, path))
    }
    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        if self.fail.lock().unwrap().as_deref() == Some(path) {
            return Err(std::io::Error::other("synthetic write failure"));
        }
        self.files.lock().unwrap().insert(path.into(), data.to_vec());
        Ok(())
    }
}

struct SafetyState {
    project: Arc<Project>,
    state: Value,
    history: History,
    path: Option<String>,
    revision: u64,
    saved_revision: u64,
    schema: u32,
    prefs: Value,
    journal: Vec<(String, Value)>,
}

impl SafetyState {
    fn capture(s: &Session) -> Self {
        Self {
            project: s.project.clone(),
            state: serde_json::to_value(&s.state).unwrap(),
            history: s.history.clone(),
            path: s.path.clone(),
            revision: s.revision,
            saved_revision: s.saved_revision,
            schema: s.loaded_schema,
            prefs: serde_json::to_value(&s.prefs).unwrap(),
            journal: s.journal.clone(),
        }
    }
    fn unchanged(&self, s: &Session) {
        assert!(*s.project == *self.project, "full project must survive failure/refusal");
        assert!(Arc::ptr_eq(&s.project, &self.project), "active Arc identity must survive failure/refusal");
        assert_eq!(serde_json::to_value(&s.state).unwrap(), self.state);
        for (now, before) in [(&s.history.undo, &self.history.undo), (&s.history.redo, &self.history.redo)] {
            assert_eq!(now.len(), before.len());
            for ((label, project), (old_label, old_project)) in now.iter().zip(before) {
                assert_eq!(label, old_label);
                assert!(Arc::ptr_eq(project, old_project));
            }
        }
        assert_eq!(s.history.limit, self.history.limit);
        assert_eq!(s.history.merge_key, self.history.merge_key);
        assert_eq!(s.path, self.path);
        assert_eq!((s.revision, s.saved_revision, s.loaded_schema), (self.revision, self.saved_revision, self.schema));
        assert_eq!(serde_json::to_value(&s.prefs).unwrap(), self.prefs);
        assert_eq!(s.journal, self.journal);
    }
}

fn safety_edited(s: &mut Session) {
    s.execute("sequence.addEdit", json!({"seconds": 1.0})).unwrap();
    s.execute("sequence.addEdit", json!({"seconds": 2.0})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert!(!s.history.undo.is_empty() && !s.history.redo.is_empty());
}

#[test]
fn safety_save_failure_preserves_project_identity_and_history() {
    for (saved, edited) in [(false, false), (false, true), (true, false), (true, true)] {
        let fs = Arc::new(SafetyFs::default());
        let mut s = demo();
        s.services = fs.clone();
        if saved {
            s.execute("file.saveAs", json!({"path":"old.fcproj"})).unwrap();
        }
        if edited {
            safety_edited(&mut s);
        }
        fs.files.lock().unwrap().insert("new.fcproj".into(), b"old destination bytes".to_vec());
        *fs.fail.lock().unwrap() = Some("new.fcproj".into());
        let before = SafetyState::capture(&s);
        let disk = fs.files.lock().unwrap().clone();
        let e = s.execute("file.saveAs", json!({"path":"new.fcproj"})).unwrap_err();
        println!("SaveAs saved={saved} edited={edited}: {e}");
        before.unchanged(&s);
        assert_eq!(*fs.files.lock().unwrap(), disk);
        assert_eq!(s.is_dirty(), edited);
    }
}

#[test]
fn safety_legacy_backup_and_primary_failures_preserve_active_state() {
    let legacy = include_bytes!("../../format/tests/fixtures/v1-edit.fcproj").to_vec();
    for failed in ["old (schema v1 backup).fcproj", "old.fcproj"] {
        let fs = Arc::new(SafetyFs::default());
        fs.files.lock().unwrap().insert("old.fcproj".into(), legacy.clone());
        let mut s = Session { services: fs.clone(), ..Session::default() };
        s.execute("file.open", json!({"path":"old.fcproj"})).unwrap();
        safety_edited(&mut s);
        *fs.fail.lock().unwrap() = Some(failed.into());
        let before = SafetyState::capture(&s);
        assert!(s.execute("file.save", json!({})).is_err());
        before.unchanged(&s);
        assert_eq!(fs.read_file("old.fcproj").unwrap(), legacy);
        if failed == "old.fcproj" {
            assert_eq!(fs.read_file("old (schema v1 backup).fcproj").unwrap(), legacy, "a completed schema backup remains useful after primary failure");
        } else {
            assert!(fs.read_file("old (schema v1 backup).fcproj").is_err());
        }
    }
}

#[test]
fn safety_save_as_normalizes_retained_history_without_an_edit() {
    let fs = Arc::new(SafetyFs::default());
    let mut s = demo();
    s.services = fs.clone();
    safety_edited(&mut s);
    let old = SafetyState::capture(&s);
    let content = |p: &Project| {
        let mut p = p.clone();
        p.name.clear();
        p.root.name.clear();
        p
    };
    s.execute("file.saveAs", json!({"path":"New identity.fcproj"})).unwrap();
    assert_eq!(s.revision, old.revision);
    assert_eq!(s.saved_revision, s.revision);
    assert_eq!(s.history.limit, old.history.limit);
    assert_eq!(s.history.merge_key, old.history.merge_key);
    assert_eq!(serde_json::to_value(&s.state).unwrap(), old.state);
    for (now, before) in [(&s.history.undo, &old.history.undo), (&s.history.redo, &old.history.redo)] {
        assert_eq!(now.len(), before.len());
        for ((label, p), (old_label, old_p)) in now.iter().zip(before) {
            assert_eq!(label, old_label);
            assert_eq!((p.name.as_str(), p.root.name.as_str()), ("New identity", "New identity"));
            assert_eq!(content(p), content(old_p));
        }
    }
    let saved = filmcraft_format::decode(&fs.read_file("New identity.fcproj").unwrap()).unwrap();
    assert_eq!(saved.project, *s.project);
    let edited = content(&s.project);
    s.execute("edit.undo", json!({})).unwrap();
    assert_ne!(content(&s.project), edited);
    assert_eq!(s.project.name, "New identity");
    s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(content(&s.project), edited);
    assert_eq!(s.project.name, "New identity");
    s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(s.project.root.name, "New identity");
}

#[test]
fn safety_save_copy_never_adopts_even_on_failure() {
    let fs = Arc::new(SafetyFs::default());
    let mut s = demo();
    s.services = fs.clone();
    safety_edited(&mut s);
    for fail in [true, false] {
        *fs.fail.lock().unwrap() = fail.then(|| "Copy.fcproj".into());
        let before = SafetyState::capture(&s);
        let r = s.execute("file.saveCopy", json!({"path":"Copy.fcproj"}));
        // A successful command is journaled, but it does not change the editor or file identity.
        if r.is_ok() {
            s.journal.pop();
        }
        before.unchanged(&s);
        assert_eq!(r.is_err(), fail);
        if !fail {
            assert_eq!(filmcraft_format::decode(&fs.read_file("Copy.fcproj").unwrap()).unwrap().project, *s.project);
        }
    }
}

#[test]
fn safety_dirty_replacements_require_explicit_discard() {
    for command in
        ["file.newProject", "file.openDemoProject", "file.open", "file.revert", "file.closeProject", "file.closeAllProjects", "file.newProjectFromTemplate"]
    {
        let fs = Arc::new(SafetyFs::default());
        let mut s = demo();
        s.services = fs.clone();
        s.execute("file.saveAs", json!({"path":"old.fcproj"})).unwrap();
        fs.files.lock().unwrap().insert("candidate.fcproj".into(), filmcraft_format::encode(&Project::new("Candidate"), false));
        safety_edited(&mut s);
        let before = SafetyState::capture(&s);
        let p = json!({"path":"candidate.fcproj", "template":"candidate.fcproj", "name":"New"});
        let r = s.execute(command, p.clone());
        assert!(r.is_err(), "{command} discarded dirty state without consent: {r:?}");
        before.unchanged(&s);
        let mut confirmed = p;
        confirmed["force"] = json!(true);
        s.execute(command, confirmed).unwrap();
        assert!(!s.is_dirty());
        if matches!(command, "file.newProject" | "file.openDemoProject" | "file.closeProject" | "file.closeAllProjects" | "file.newProjectFromTemplate") {
            assert!(s.path.is_none(), "{command} retained an unrelated saved path");
        }
    }
}

#[test]
fn safety_invalid_open_and_revert_preserve_dirty_state() {
    let fs = Arc::new(SafetyFs::default());
    let mut s = demo();
    s.services = fs.clone();
    s.execute("file.saveAs", json!({"path":"old.fcproj"})).unwrap();
    safety_edited(&mut s);
    fs.files.lock().unwrap().insert("old.fcproj".into(), b"invalid synthetic project".to_vec());
    for command in ["file.open", "file.revert"] {
        let before = SafetyState::capture(&s);
        assert!(s.execute(command, json!({"path":"old.fcproj", "force":true})).is_err());
        before.unchanged(&s);
    }
}

#[test]
fn safety_failed_file_commands_preserve_dynamic_trim() {
    for moved in [false, true] {
        for command in ["file.saveAs", "file.open"] {
            let fs = Arc::new(SafetyFs::default());
            let mut s = demo();
            s.services = fs.clone();
            for track in ["V2", "V3", "A2", "A3"] {
                s.execute("timeline.setTrack", json!({"track":track,"syncLock":false})).unwrap();
            }
            let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id;
            s.execute("trim.selectEditPoint", json!({"clip":clip.0,"edge":"out","kind":"roll"})).unwrap();
            s.execute("trim.shuttle", json!({"direction":"forward","clock":10.0})).unwrap();
            if moved {
                s.execute("trim.tick", json!({"clock":10.5})).unwrap();
            }
            let before = SafetyState::capture(&s);
            let trim = format!("{:?}", s.trim_play);
            *fs.fail.lock().unwrap() = Some("missing.fcproj".into());
            assert!(s.execute(command, json!({"path":"missing.fcproj","force":true})).is_err());
            assert!(format!("{:?}", s.trim_play) == trim, "{command} moved={moved} settled an edit despite failure");
            before.unchanged(&s);
            if command == "file.saveAs" {
                *fs.fail.lock().unwrap() = None;
                s.execute("file.saveCopy", json!({"path":"Trim copy.fcproj"})).unwrap();
                s.journal.pop();
                before.unchanged(&s);
                assert!(format!("{:?}", s.trim_play) == trim);
                let undo = s.history.undo.len();
                let rev = s.revision;
                s.execute("file.saveAs", json!({"path":"Trim identity.fcproj"})).unwrap();
                assert!(s.trim_play.dynamic.is_none());
                assert!(!s.is_dirty());
                assert_eq!(s.revision, rev);
                assert_eq!(s.history.undo.len(), undo + usize::from(moved));
                assert_eq!(filmcraft_format::decode(&fs.read_file("Trim identity.fcproj").unwrap()).unwrap().project, *s.project);
                if moved {
                    s.execute("edit.undo", json!({})).unwrap();
                    assert_eq!(s.project.name, "Trim identity");
                }
            } else {
                s.execute("file.newProject", json!({"force":true})).unwrap();
                assert!(!s.trim_play.active());
                assert!(s.history.undo.is_empty());
            }
        }
    }
}
