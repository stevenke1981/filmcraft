//! Preferences, auto-save ring and crash recovery through the command interface.

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

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = std::time::Instant::now();
    while !f() {
        assert!(t0.elapsed() < std::time::Duration::from_secs(10), "timed out waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn start(s: &mut Session, data: &std::path::Path) {
    let mut cfg = autosave::AutosaveConfig::new(data);
    cfg.journal_debounce = std::time::Duration::from_millis(20);
    s.start_autosave(cfg).unwrap();
}

fn journaled(s: &mut Session) -> u64 {
    s.poll_persistence();
    s.persistence.as_ref().unwrap().status.journaled_revision
}

#[test]
fn preferences_get_set_and_persist() {
    let d = temp_dir("prefs");
    let mut s = Session::default();
    start(&mut s, &d);
    assert_eq!(s.execute("prefs.get", json!({"key": "autoSave.intervalMinutes"})).unwrap(), json!(5));
    assert_eq!(s.execute("prefs.get", json!({"key": "autoSave.maxVersions"})).unwrap(), json!(20));
    s.execute("prefs.set", json!({"key": "autoSave.intervalMinutes", "value": 12.0})).unwrap();
    s.execute("prefs.set", json!({"values": {"autoSave.maxVersions": 5000, "autoSave.saveCurrentProject": true}})).unwrap();
    assert_eq!(s.prefs.auto_save.interval_minutes, 12);
    assert_eq!(s.prefs.auto_save.max_versions, 1000, "clamped");
    assert!(s.execute("prefs.set", json!({"key": "autoSave.nope", "value": 1})).is_err());
    assert!(s.execute("prefs.set", json!({"key": "autoSave.enabled", "value": "yes"})).is_err());
    let keys = s.execute("prefs.get", json!({})).unwrap()["keys"].clone();
    assert!(keys.as_array().unwrap().iter().any(|k| k == "autoSave.recoveryJournal"));
    s.shutdown();
    let mut t = Session::default();
    start(&mut t, &d);
    assert_eq!(t.prefs.auto_save.interval_minutes, 12);
    assert!(t.prefs.auto_save.save_current_project);
    t.shutdown();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn crash_recovery_end_to_end() {
    let d = temp_dir("recover");
    let proj = d.join("film.fcproj").to_string_lossy().to_string();
    // Session A: saved project, then unsaved edits, then a crash.
    let mut a = demo();
    start(&mut a, &d);
    a.execute("file.saveAs", json!({"path": proj})).unwrap();
    a.execute("sequence.addEdit", json!({"seconds": 1.5})).unwrap();
    a.execute("sequence.addEdit", json!({"seconds": 2.5})).unwrap();
    let rev = a.revision;
    wait_for("journal", || journaled(&mut a) == rev);
    let st = a.execute("file.autoSaveStatus", json!({})).unwrap();
    assert!(st["status"]["lastBytes"].as_u64().unwrap() > 1000, "{st}");
    let edited = (*a.project).clone();
    // Session B starts while A is alive: A's journal is not stale.
    let mut b = Session::default();
    start(&mut b, &d);
    assert!(b.recovery_candidates().is_empty(), "a live session's journal is never offered");
    b.shutdown();
    a.persistence.take().unwrap().simulate_crash();

    // Session C: next launch.
    let mut c = Session::default();
    start(&mut c, &d);
    assert_eq!(c.recovery_candidates().len(), 1);
    let list = c.execute("file.recoveryList", json!({})).unwrap();
    assert_eq!(list[0]["projectName"], "film", "named after the file it was saved as");
    assert_eq!(list[0]["projectPath"], proj.as_str());
    assert_eq!(list[0]["cleanExit"], false);
    assert_eq!(list[0]["savedAt"].as_str().unwrap().len(), 19);
    c.execute("file.recover", json!({})).unwrap();
    assert_eq!(*c.project, edited);
    assert!(c.is_dirty(), "recovered changes are unsaved");
    assert_eq!(c.path.as_deref(), Some(proj.as_str()));
    assert!(c.recovery_candidates().is_empty());
    assert!(!c.is_enabled("file.recover"));
    // The recovered state is already in C's own journal (a crash now loses nothing).
    assert_eq!(journaled(&mut c), c.revision);
    c.execute("file.save", json!({})).unwrap();
    wait_for("journal cleared", || {
        c.poll_persistence();
        !c.persistence.as_ref().unwrap().session_dir.join("snapshot.fcproj").exists()
    });
    let session_dir = c.persistence.as_ref().unwrap().session_dir.clone();
    c.shutdown();
    assert!(!session_dir.exists(), "clean exit with nothing unsaved removes the journal");
    let mut e = Session::default();
    start(&mut e, &d);
    assert!(e.recovery_candidates().is_empty());
    e.shutdown();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn quitting_with_unsaved_changes_keeps_them_for_next_launch() {
    let d = temp_dir("quitdirty");
    let mut a = demo();
    start(&mut a, &d);
    a.execute("sequence.addEdit", json!({"seconds": 1.0})).unwrap();
    a.shutdown();
    let mut b = Session::default();
    start(&mut b, &d);
    let list = b.execute("file.recoveryList", json!({})).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["cleanExit"], true);
    b.execute("file.discardRecovery", json!({})).unwrap();
    assert!(b.recovery_candidates().is_empty());
    b.shutdown();
    let mut c = Session::default();
    start(&mut c, &d);
    assert!(c.recovery_candidates().is_empty(), "discarded for good");
    c.shutdown();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn auto_save_ring_rotates_and_can_save_the_project() {
    let d = temp_dir("ring");
    let proj = d.join("ring.fcproj").to_string_lossy().to_string();
    let mut s = demo();
    start(&mut s, &d);
    s.execute("prefs.set", json!({"values": {"autoSave.maxVersions": 2}})).unwrap();
    s.execute("file.saveAs", json!({"path": proj})).unwrap();
    // Nothing unsaved: no auto-save.
    assert!(s.execute("file.autoSaveNow", json!({})).unwrap()["path"].is_null());
    let mut written = Vec::new();
    for i in 0..3 {
        s.execute("sequence.addEdit", json!({"seconds": 1.0 + i as f64})).unwrap();
        let r = s.execute("file.autoSaveNow", json!({})).unwrap();
        written.push(r["path"].as_str().unwrap().to_string());
        // File names have one-second resolution.
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    let sep = std::path::MAIN_SEPARATOR;
    assert!(written[0].contains(&format!("{sep}Auto-Save{sep}ring-")), "{}", written[0]);
    let list = s.execute("file.listAutoSaves", json!({})).unwrap();
    let files: Vec<String> = list["files"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert_eq!(files, vec![written[2].clone(), written[1].clone()], "newest first, oldest pruned");
    let mut t = Session::default();
    t.execute("file.open", json!({"path": written[2]})).unwrap();
    assert_eq!(*t.project, *s.project);
    assert!(s.is_dirty(), "auto-save does not save the project itself by default");
    s.execute("prefs.set", json!({"key": "autoSave.saveCurrentProject", "value": true})).unwrap();
    s.execute("sequence.addEdit", json!({"seconds": 5.0})).unwrap();
    s.execute("file.autoSaveNow", json!({})).unwrap();
    wait_for("project saved", || {
        s.poll_persistence();
        !s.is_dirty()
    });
    let mut t = Session::default();
    t.execute("file.open", json!({"path": proj})).unwrap();
    assert_eq!(*t.project, *s.project);
    s.shutdown();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn portable_marker_keeps_data_next_to_the_executable() {
    // #139: a portable install must not write to %APPDATA%.
    let d = temp_dir("portable");
    assert_eq!(autosave::portable_data_dir(&d), None);
    std::fs::create_dir_all(d.join(autosave::PORTABLE_MARKER)).unwrap();
    assert_eq!(autosave::portable_data_dir(&d), None, "a directory is not the marker");
    std::fs::remove_dir_all(d.join(autosave::PORTABLE_MARKER)).unwrap();
    std::fs::write(d.join(autosave::PORTABLE_MARKER), "").unwrap();
    assert_eq!(autosave::portable_data_dir(&d), Some(d.join("data")));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn journal_costs_the_ui_thread_only_a_handoff() {
    // Per change the UI thread pays an Arc clone and a channel send; encoding happens on the worker.
    let d = temp_dir("cost");
    let mut s = demo();
    start(&mut s, &d);
    let t0 = std::time::Instant::now();
    for i in 0..50 {
        s.execute("playhead.set", json!({"seconds": i as f64 * 0.1})).unwrap();
        let _ = s.execute("sequence.addEdit", json!({}));
    }
    let per_cmd = t0.elapsed() / 100;
    let rev = s.revision;
    wait_for("journal", || journaled(&mut s) == rev);
    assert!(per_cmd < std::time::Duration::from_millis(50), "{per_cmd:?}");
    s.shutdown();
    let _ = std::fs::remove_dir_all(&d);
}

fn safety_state(s: &Session) -> Value {
    json!({"project":*s.project,"state":s.state,"path":s.path,"revision":s.revision,"savedRevision":s.saved_revision,"schema":s.loaded_schema,
        "undo":s.history.undo.iter().map(|(label,p)|json!([label,**p])).collect::<Vec<_>>(),
        "redo":s.history.redo.iter().map(|(label,p)|json!([label,**p])).collect::<Vec<_>>()})
}

fn safety_candidate(data: &std::path::Path) -> Project {
    let mut a = demo();
    start(&mut a, data);
    a.execute("sequence.addEdit", json!({"seconds": 3.0})).unwrap();
    let project = (*a.project).clone();
    a.persistence.take().unwrap().simulate_crash();
    project
}

#[test]
fn safety_recovery_handoff_write_failures_keep_candidate_and_current_state() {
    for blocked in ["snapshot.fcproj", "session.json"] {
        let d = temp_dir(&format!("safety-recover-{}", blocked.replace('.', "-")));
        let recovered = safety_candidate(&d);
        let mut s = demo();
        start(&mut s, &d);
        s.execute("sequence.addEdit", json!({"seconds": 1.0})).unwrap();
        s.persistence.as_ref().unwrap().flush();
        let candidate = s.recovery_candidates()[0].clone();
        let old_bytes = std::fs::read(candidate.dir.join("snapshot.fcproj")).unwrap();
        let old_meta = std::fs::read(candidate.dir.join("session.json")).unwrap();
        let own_dir = s.persistence.as_ref().unwrap().session_dir.clone();
        let obstruction = own_dir.join(blocked);
        std::fs::remove_file(&obstruction).unwrap();
        std::fs::create_dir(&obstruction).unwrap();
        let before = safety_state(&s);
        let arc = s.project.clone();
        let sent = s.persistence.as_ref().unwrap().last_sent.clone();
        let r = s.execute("file.recover", json!({"id":candidate.id,"force":true}));
        assert!(r.is_err(), "{blocked} handoff was acknowledged despite failed journal: {r:?}");
        println!("{blocked} failed handoff: {}", r.unwrap_err());
        assert_eq!(safety_state(&s), before);
        assert!(Arc::ptr_eq(&arc, &s.project));
        assert_eq!(s.persistence.as_ref().unwrap().last_sent, sent, "failed handoff must not adopt candidate in dedup state");
        assert_eq!(s.recovery_candidates().len(), 1);
        assert_eq!(std::fs::read(candidate.dir.join("snapshot.fcproj")).unwrap(), old_bytes);
        assert_eq!(std::fs::read(candidate.dir.join("session.json")).unwrap(), old_meta);
        // Removing only our synthetic obstruction lets the worker persist its PRIOR latest,
        // including after a successful candidate snapshot followed by a failed metadata write.
        std::fs::remove_dir(&obstruction).unwrap();
        s.persistence.as_ref().unwrap().flush();
        let own = filmcraft_format::decode(&std::fs::read(own_dir.join("snapshot.fcproj")).unwrap()).unwrap();
        assert_eq!(own.project, *s.project);
        assert_ne!(own.project, recovered);
        let meta: Value = serde_json::from_slice(&std::fs::read(own_dir.join("session.json")).unwrap()).unwrap();
        assert_eq!(meta["revision"], s.revision);
        s.persistence.take().unwrap().simulate_crash();
        let mut restart = Session::default();
        start(&mut restart, &d);
        assert_eq!(restart.recovery_candidates().len(), 2, "both the untouched candidate and current dirty state remain recoverable");
        assert!(restart.recovery_candidates().iter().any(|c| c.id == candidate.id));
        restart.shutdown();
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[test]
fn safety_recovery_disabled_journal_refuses_destructive_handoff() {
    let d = temp_dir("safety-recovery-disabled");
    safety_candidate(&d);
    let mut s = Session::default();
    start(&mut s, &d);
    s.execute("prefs.set", json!({"key":"autoSave.recoveryJournal","value":false})).unwrap();
    let candidate = s.recovery_candidates()[0].clone();
    let before = safety_state(&s);
    let arc = s.project.clone();
    let r = s.execute("file.recover", json!({"id":candidate.id}));
    assert!(r.is_err(), "recovery cannot discard its only journal when handoff is disabled: {r:?}");
    assert_eq!(safety_state(&s), before);
    assert!(Arc::ptr_eq(&s.project, &arc));
    assert_eq!(s.recovery_candidates().len(), 1);
    assert!(candidate.dir.join("snapshot.fcproj").is_file());
    s.shutdown();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn safety_rejected_replacements_keep_dirty_journal_before_and_after_debounce() {
    for settled in [false, true] {
        for command in ["file.newProject", "file.openDemoProject", "file.open", "file.revert", "file.recover"] {
            let d = temp_dir(&format!("safety-refused-{settled}-{}", command.replace('.', "-")));
            safety_candidate(&d);
            let path = d.join("active.fcproj").to_string_lossy().into_owned();
            let mut s = demo();
            if settled {
                start(&mut s, &d);
            } else {
                let mut cfg = autosave::AutosaveConfig::new(&d);
                cfg.journal_debounce = std::time::Duration::from_secs(600);
                s.start_autosave(cfg).unwrap();
            }
            s.execute("file.saveAs", json!({"path":path})).unwrap();
            s.execute("sequence.addEdit", json!({"seconds":1.0})).unwrap();
            if settled {
                s.persistence.as_ref().unwrap().flush();
            }
            let before = safety_state(&s);
            let arc = s.project.clone();
            if !settled {
                assert!(!s.persistence.as_ref().unwrap().session_dir.join("snapshot.fcproj").exists(), "before-debounce phase requires no own snapshot yet");
            }
            let r = s.execute(command, json!({"path":path}));
            assert!(r.is_err(), "{command} unexpectedly discarded dirty state: {r:?}");
            assert_eq!(safety_state(&s), before);
            assert!(Arc::ptr_eq(&arc, &s.project));
            s.persistence.as_ref().unwrap().flush();
            let own = s.persistence.as_ref().unwrap().session_dir.clone();
            assert_eq!(filmcraft_format::decode(&std::fs::read(own.join("snapshot.fcproj")).unwrap()).unwrap().project, *s.project);
            let edited = (*s.project).clone();
            s.shutdown();
            let mut restart = Session::default();
            start(&mut restart, &d);
            assert!(restart.recovery_candidates().iter().any(|c| crate::autosave::load_candidate(c).unwrap().project == edited));
            restart.shutdown();
            let _ = std::fs::remove_dir_all(&d);
        }
    }
}

#[cfg(windows)]
#[test]
fn review_recovery_cleanup_failure_drops_previous_live_trim() {
    use std::os::windows::fs::OpenOptionsExt;

    for moved in [false, true] {
        for followup in ["tick", "cancel", "save"] {
            let d = temp_dir(&format!("review-trim-recovery-{moved}-{followup}"));
            let path_b = d.join("Recovered-B.fcproj").to_string_lossy().into_owned();
            let mut b = demo();
            start(&mut b, &d);
            b.execute("file.saveAs", json!({"path":path_b})).unwrap();
            b.execute("sequence.addEdit", json!({"seconds":3.0})).unwrap();
            let recovered = (*b.project).clone();
            b.persistence.take().unwrap().simulate_crash();

            let path_a = d.join("Active-A.fcproj").to_string_lossy().into_owned();
            let mut s = demo();
            start(&mut s, &d);
            s.execute("file.saveAs", json!({"path":path_a})).unwrap();
            s.execute("sequence.addEdit", json!({"seconds":1.0})).unwrap();
            let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id;
            s.execute("trim.selectEditPoint", json!({"clip":clip.0,"edge":"out","kind":"roll"})).unwrap();
            s.execute("trim.shuttle", json!({"direction":"forward","clock":10.0})).unwrap();
            if moved {
                s.execute("trim.tick", json!({"clock":10.25})).unwrap();
                assert!(s.trim_play.dynamic.as_ref().unwrap().offset > Tick::ZERO);
            }
            s.persistence.as_ref().unwrap().flush();
            let candidate = s.recovery_candidates()[0].clone();
            assert_eq!(candidate.meta.project_path.as_deref(), Some(path_b.as_str()));
            let own_dir = s.persistence.as_ref().unwrap().session_dir.clone();

            // Before-install failure must retain A, its live trim, history and prior journal.
            let before = safety_state(&s);
            let before_arc = s.project.clone();
            let trim_before = s.execute("trim.monitor", json!({})).unwrap();
            let obstruction = own_dir.join("snapshot.fcproj");
            std::fs::remove_file(&obstruction).unwrap();
            std::fs::create_dir(&obstruction).unwrap();
            assert!(s.execute("file.recover", json!({"id":candidate.id,"force":true})).is_err());
            assert_eq!(safety_state(&s), before);
            assert!(Arc::ptr_eq(&s.project, &before_arc));
            assert_eq!(s.execute("trim.monitor", json!({})).unwrap(), trim_before);
            std::fs::remove_dir(&obstruction).unwrap();
            s.persistence.as_ref().unwrap().flush();
            assert_eq!(filmcraft_format::decode(&std::fs::read(own_dir.join("snapshot.fcproj")).unwrap()).unwrap().project, *s.project);

            // A task-owned metadata handle denies deletion, after loading B and confirming its
            // new journal. It does not block reading B or writing this session's journal.
            let locked_metadata = std::fs::OpenOptions::new().read(true).share_mode(3).open(candidate.dir.join("session.json")).unwrap();
            let revision = s.revision;
            let result = s.execute("file.recover", json!({"id":candidate.id,"force":true}));
            assert!(result.is_err(), "cleanup fixture must fail after install: {result:?}");
            println!("{moved}/{followup}: cleanup error after install: {}", result.unwrap_err());
            assert_eq!(*s.project, recovered, "B is installed before the cleanup error");
            assert_eq!(s.path.as_deref(), Some(path_b.as_str()));
            assert_eq!(s.revision, revision + 1);
            assert_eq!(s.saved_revision, 0);
            assert!(s.is_dirty());
            assert!(s.history.undo.is_empty() && s.history.redo.is_empty());
            assert_eq!(filmcraft_format::decode(&std::fs::read(own_dir.join("snapshot.fcproj")).unwrap()).unwrap().project, recovered);
            let installed_arc = s.project.clone();
            let installed_revision = s.revision;
            match followup {
                "tick" => {
                    s.execute("trim.tick", json!({"clock":11.0})).unwrap();
                }
                "cancel" => {
                    s.execute("trim.cancelDynamic", json!({})).unwrap();
                }
                "save" => {
                    s.execute("file.save", json!({})).unwrap();
                    assert_eq!(filmcraft_format::decode(&std::fs::read(&path_b).unwrap()).unwrap().project, recovered);
                    assert!(!s.is_dirty());
                }
                _ => unreachable!(),
            }
            assert_eq!(*s.project, recovered, "{moved}/{followup} resurrected discarded A after B was installed");
            assert_eq!(s.path.as_deref(), Some(path_b.as_str()));
            assert_eq!(s.revision, installed_revision, "no old trim edit belongs to B");
            assert!(s.history.undo.is_empty() && s.history.redo.is_empty(), "no old A snapshot may enter B's history");
            assert!(!s.trim_play.active(), "installation commits the old transient-state discard even when cleanup returns Err");
            if followup != "save" {
                assert!(Arc::ptr_eq(&s.project, &installed_arc));
                assert!(s.is_dirty());
            }
            drop(locked_metadata);
            s.shutdown();
            let _ = std::fs::remove_dir_all(&d);
        }
    }
}
