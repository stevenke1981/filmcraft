//! Versioned tools operate on the same engine session as the desktop UI.

use std::collections::{HashMap, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::server::FilmcraftMcp;

#[derive(Default)]
pub(crate) struct Ledger {
    requests: HashMap<String, (Value, Value)>,
    jobs: HashSet<u64>,
    outputs: HashMap<String, u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Guard {
    /// Unique per intended mutation; reuse exactly on retry. Kept for this MCP process's lifetime.
    pub request_id: String,
    /// Revision from project_read_v1. Checked atomically in the App command layer.
    pub revision: u64,
    /// Set true only after the user confirms discard, overwrite, clip deletion or cancellation.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectAction {
    Create,
    Open,
    Save,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectWrite {
    pub guard: Guard,
    pub action: ProjectAction,
    /// Required for create, at most 200 characters.
    pub name: Option<String>,
    /// Required for open/save: .fcproj path within --workspace. Relative paths use the workspace.
    pub path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportMedia {
    pub guard: Guard,
    /// One to 32 existing local media files; project-file import is unsupported here.
    pub paths: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum TimelineEdit {
    CreateSequence {
        name: String,
        width: u32,
        height: u32,
        fps: u32,
    },
    /// Open an existing stable sequence id in the App timeline, including after creation redo.
    ActivateSequence {
        sequence: u64,
    },
    Place {
        item: u64,
        track: String,
        audio_track: Option<String>,
        frame: i64,
        duration_ticks: Option<i64>,
        source_in_ticks: Option<i64>,
        insert: bool,
    },
    Move {
        clip: u64,
        track: String,
        time_ticks: i64,
        insert: bool,
    },
    Trim {
        clip: u64,
        edge: Edge,
        delta_frames: i64,
        ripple: bool,
    },
    Split {
        clip: u64,
        frame: i64,
    },
    Delete {
        clips: Vec<u64>,
    },
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    In,
    Out,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditTimeline {
    pub guard: Guard,
    pub edit: TimelineEdit,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum HistoryAction {
    Undo,
    Redo,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub guard: Guard,
    pub action: HistoryAction,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartExport {
    pub guard: Guard,
    /// Local MP4 output. H.264/AAC software export only in v1; other formats are explicitly unsupported.
    pub path: String,
    /// Maximum output size 1920 x 1080, at most 60 fps. Small outputs are recommended.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub job: u64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelExport {
    pub guard: Guard,
    pub job: u64,
}

pub(crate) fn error(code: &str, message: impl std::fmt::Display) -> Value {
    json!({"schema_version":1,"ok":false,"error":{"code":code,"message":message.to_string()}})
}

/// Validate nested typed arguments before the SDK router, preserving JSON-RPC invalid params.
pub(crate) fn validate_arguments(tool: &str, arguments: Value) -> Result<(), String> {
    fn parse<T: serde::de::DeserializeOwned>(arguments: Value) -> Result<(), String> {
        serde_json::from_value::<T>(arguments).map(|_| ()).map_err(|e| e.to_string())
    }
    match tool {
        "project_write_v1" => parse::<ProjectWrite>(arguments),
        "media_import_v1" => parse::<ImportMedia>(arguments),
        "timeline_edit_v1" => parse::<EditTimeline>(arguments),
        "history_v1" => parse::<History>(arguments),
        "export_start_v1" => parse::<StartExport>(arguments),
        "export_status_v1" => parse::<Job>(arguments),
        "export_cancel_v1" => parse::<CancelExport>(arguments),
        _ => Ok(()),
    }
}

fn local_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix("\\\\?\\").unwrap_or(&text).to_string()
}

fn checked_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() || name.chars().count() > 200 || name.chars().any(char::is_control) {
        Err("name must contain 1 to 200 printable characters".into())
    } else {
        Ok(())
    }
}

impl TimelineEdit {
    fn command(&self) -> Result<(&'static str, Value, bool), String> {
        let positive = |id: u64| if id > 0 { Ok(()) } else { Err("ids must be positive".to_string()) };
        let time = |value: i64| {
            if (0..=filmcraft_time::Tick::from_seconds_f64(86_400.0).0).contains(&value) {
                Ok(())
            } else {
                Err("time is outside the v1 range (0 to 24 hours)".to_string())
            }
        };
        let frame = |value: i64| if (0..=5_184_000).contains(&value) { Ok(()) } else { Err("frame is outside the v1 range".to_string()) };
        let track = |text: &str| {
            if !text.is_empty() && text.len() <= 200 && !text.chars().any(char::is_control) { Ok(()) } else { Err("invalid track id/name".to_string()) }
        };
        Ok(match self {
            Self::CreateSequence { name, width, height, fps } => {
                checked_name(name)?;
                if !(16..=3840).contains(width) || !(16..=2160).contains(height) || !(1..=60).contains(fps) {
                    return Err("invalid sequence dimensions or fps".into());
                }
                ("file.newSequence", json!({"name":name,"width":width,"height":height,"fps":fps,"video":2,"audio":2}), false)
            }
            Self::ActivateSequence { sequence } => {
                positive(*sequence)?;
                ("sequence.open", json!({"item":sequence}), false)
            }
            Self::Place { item, track: tr, audio_track, frame: at, duration_ticks, source_in_ticks, insert } => {
                positive(*item)?;
                track(tr)?;
                frame(*at)?;
                if let Some(audio) = audio_track {
                    track(audio)?;
                }
                if let Some(duration) = duration_ticks {
                    time(*duration)?;
                    if *duration == 0 {
                        return Err("duration must be positive".into());
                    }
                }
                if let Some(source) = source_in_ticks {
                    time(*source)?;
                }
                let mut params = json!({"item":item,"track":tr,"frame":at,"insert":insert});
                if let Some(audio) = audio_track {
                    params["audioTrack"] = json!(audio);
                }
                if let Some(duration) = duration_ticks {
                    params["duration"] = json!(duration);
                }
                if let Some(source) = source_in_ticks {
                    params["sourceIn"] = json!(source);
                }
                // Overwrite placement can remove existing timeline content.
                ("timeline.place", params, !insert)
            }
            Self::Move { clip, track: tr, time_ticks, insert } => {
                positive(*clip)?;
                track(tr)?;
                time(*time_ticks)?;
                ("timeline.move", json!({"moves":[{"clip":clip,"track":tr,"time":time_ticks}],"insert":insert}), !insert)
            }
            Self::Trim { clip, edge, delta_frames, ripple } => {
                positive(*clip)?;
                if !(-5_184_000..=5_184_000).contains(delta_frames) || *delta_frames == 0 {
                    return Err("invalid trim delta".into());
                }
                ("timeline.trim", json!({"clip":clip,"edge":edge,"deltaFrames":delta_frames,"mode":if *ripple {"ripple"} else {"regular"}}), false)
            }
            Self::Split { clip, frame: at } => {
                positive(*clip)?;
                frame(*at)?;
                ("timeline.razor", json!({"clip":clip,"frame":at}), false)
            }
            Self::Delete { clips } => {
                if clips.is_empty() || clips.len() > 256 {
                    return Err("delete requires 1 to 256 clip ids".into());
                }
                for id in clips {
                    positive(*id)?;
                }
                ("edit.clear", json!({"clips":clips}), true)
            }
        })
    }
}

impl FilmcraftMcp {
    pub(crate) fn agent_capabilities(&self) -> Value {
        json!({"schema_version":1,"ok":true,"mode":if self.bridge_client().is_some(){"live_app"}else{"headless_engine"},
            "workspace":self.workspace.as_ref().map(|w|local_path(&w.root)),"ticks_per_second":254_016_000_000u64,"ui_available":self.bridge_client().is_some(),
            "supported":["project_read","project_create","project_open","project_save","media_import","sequence_create","sequence_activate","clip_place","clip_move","clip_trim","clip_split","clip_delete","undo","redo","export_h264_aac_mp4","export_status","export_cancel","ui_inspect","ui_elements","ui_screenshot"],
            "unsupported":["network_publish","external_workspace_files","raw_ui_mutations_in_workspace_mode","agent_configuration","other_v1_export_formats"],
            "confirmation":"guard.confirm=true only after explicit user confirmation for discard, overwrite, deletion and cancel (partial output is deleted)",
            "idempotency":"request_id is process-local, serialized and bounded to 1024 unique mutations; no automatic retry after uncertain delivery"})
    }

    pub(crate) async fn agent_read(&self, timeline: bool) -> Value {
        let Some(w) = &self.workspace else {
            return error("WORKSPACE_REQUIRED", "start MCP with --workspace <absolute directory>");
        };
        let snapshot = match self.run_unchecked("agent.inspect", json!({})).await {
            Ok(v) => v,
            Err(e) => return error("APP_ERROR", e),
        };
        if let Err(e) = w.document(&snapshot["document"]) {
            return error("WORKSPACE_VIOLATION", e);
        }
        let project = &snapshot["project"];
        if timeline {
            if snapshot["sequence"].is_null() {
                return error("NO_SEQUENCE", "no active sequence");
            }
            json!({"schema_version":1,"ok":true,"revision":project["revision"],"sequence":snapshot["sequence"]})
        } else {
            json!({"schema_version":1,"ok":true,"project":project})
        }
    }

    pub(crate) async fn agent_project(&self, p: ProjectWrite) -> Value {
        let request = serde_json::to_value(&p).unwrap_or_default();
        let (id, params) = match p.action {
            ProjectAction::Create => ("file.newProject", json!({"name":p.name})),
            ProjectAction::Open => ("file.open", json!({"path":p.path})),
            ProjectAction::Save => ("file.saveAs", json!({"path":p.path})),
        };
        self.agent_mutate("project_write_v1", request, p.guard, id, params, false).await
    }

    pub(crate) async fn agent_import(&self, p: ImportMedia) -> Value {
        let request = serde_json::to_value(&p).unwrap_or_default();
        self.agent_mutate("media_import_v1", request, p.guard, "file.import", json!({"paths":p.paths,"imageSequence":false}), false).await
    }

    pub(crate) async fn agent_edit(&self, p: EditTimeline) -> Value {
        let request = serde_json::to_value(&p).unwrap_or_default();
        let (id, params, destructive) = match p.edit.command() {
            Ok(v) => v,
            Err(e) => return error("INVALID_PARAMS", e),
        };
        self.agent_mutate("timeline_edit_v1", request, p.guard, id, params, destructive).await
    }

    pub(crate) async fn agent_history(&self, p: History) -> Value {
        let request = serde_json::to_value(&p).unwrap_or_default();
        let id = match p.action {
            HistoryAction::Undo => "edit.undo",
            HistoryAction::Redo => "edit.redo",
        };
        self.agent_mutate("history_v1", request, p.guard, id, json!({}), false).await
    }

    pub(crate) async fn agent_export(&self, p: StartExport) -> Value {
        if !(16..=1920).contains(&p.width)
            || !(16..=1080).contains(&p.height)
            || !(1..=60).contains(&p.fps)
            || !p.width.is_multiple_of(2)
            || !p.height.is_multiple_of(2)
        {
            return error("INVALID_PARAMS", "export needs even dimensions within 16..1920 x 16..1080 and 1..60 fps");
        }
        let request = serde_json::to_value(&p).unwrap_or_default();
        self.agent_mutate(
            "export_start_v1",
            request,
            p.guard,
            "file.exportMedia",
            json!({"path":p.path,"format":"h264","width":p.width,"height":p.height,"fps":p.fps,"wait":false,"hardwareEncoding":"off","removeCancelledOutput":true}),
            false,
        )
        .await
    }

    pub(crate) async fn agent_cancel(&self, p: CancelExport) -> Value {
        let request = serde_json::to_value(&p).unwrap_or_default();
        self.agent_mutate("export_cancel_v1", request, p.guard, "jobs.cancel", json!({"job":p.job}), true).await
    }

    pub(crate) async fn agent_job(&self, job: u64) -> Value {
        if !self.ledger.lock().await.jobs.contains(&job) {
            return error("UNKNOWN_JOB", "only jobs started by this MCP process can be inspected or cancelled");
        }
        match self.run_unchecked("jobs.list", json!({})).await {
            Ok(v) => match v.as_array().and_then(|jobs| jobs.iter().find(|j| j["id"].as_u64() == Some(job))) {
                Some(state) => json!({"schema_version":1,"ok":true,"job":state}),
                None => error("UNKNOWN_JOB", "job was removed from the App"),
            },
            Err(e) => error("APP_ERROR", e),
        }
    }

    async fn agent_mutate(&self, tool: &str, request: Value, guard: Guard, id: &str, mut params: Value, destructive: bool) -> Value {
        let Some(w) = &self.workspace else {
            return error("WORKSPACE_REQUIRED", "start MCP with --workspace <absolute directory>");
        };
        if guard.request_id.is_empty() || guard.request_id.len() > 128 || !guard.request_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)) {
            return error("INVALID_REQUEST_ID", "request_id must contain 1 to 128 letters, digits, underscores, dots or hyphens");
        }
        let fingerprint = json!({"tool":tool,"request":request});
        let mut ledger = self.ledger.lock().await;
        if let Some((before, result)) = ledger.requests.get(&guard.request_id) {
            if before != &fingerprint {
                return error("REQUEST_ID_CONFLICT", "request_id was already used with different arguments");
            }
            let mut result = result.clone();
            result["replayed"] = json!(true);
            return result;
        }
        if ledger.requests.len() >= 1024 {
            return error("REQUEST_LIMIT", "start a new explicitly configured MCP session after inspecting the App state");
        }
        let result: Result<Value, Value> = async {
            let project = self.run_unchecked("project.inspect", json!({})).await.map_err(|e| error("APP_ERROR", e))?;
            if project["revision"].as_u64() != Some(guard.revision) {
                return Err(error("REVISION_CONFLICT", "inspect the App project again before making a new request"));
            }
            if (destructive || (matches!(id, "file.newProject" | "file.open") && project["dirty"] == true)) && !guard.confirm {
                return Err(error(
                    "CONFIRMATION_REQUIRED",
                    "explicit user confirmation is required; submit a new request_id with guard.confirm=true afterwards",
                ));
            }
            if !matches!(id, "file.newProject" | "file.open" | "jobs.cancel") {
                let document = self.run_unchecked("project.document", json!({})).await.map_err(|e| error("APP_ERROR", e))?;
                w.document(&document).map_err(|e| error("WORKSPACE_VIOLATION", e))?;
            }
            if matches!(id, "timeline.move" | "timeline.trim" | "timeline.razor" | "edit.clear") {
                let sequence = self.run_unchecked("sequence.inspect", json!({})).await.map_err(|e| error("NO_SEQUENCE", e))?;
                let mut targets = Vec::new();
                if let Some(clip) = params["clip"].as_u64() {
                    targets.push(clip);
                }
                if let Some(clips) = params["clips"].as_array() {
                    targets.extend(clips.iter().filter_map(Value::as_u64));
                }
                if let Some(moves) = params["moves"].as_array() {
                    targets.extend(moves.iter().filter_map(|v| v["clip"].as_u64()));
                }
                for target in targets {
                    let found = ["video", "audio"].iter().any(|kind| {
                        sequence[*kind].as_array().is_some_and(|tracks| {
                            tracks
                                .iter()
                                .any(|track| track["items"].as_array().is_some_and(|items| items.iter().any(|clip| clip["clip"].as_u64() == Some(target))))
                        })
                    });
                    if !found {
                        return Err(error("UNKNOWN_CLIP", format!("clip {target} is not in the active sequence")));
                    }
                }
            }
            match id {
                "file.newProject" => checked_name(params["name"].as_str().unwrap_or_default()).map_err(|e| error("INVALID_PARAMS", e))?,
                "file.open" | "file.saveAs" | "file.exportMedia" => {
                    let path = w.path(params["path"].as_str().unwrap_or_default(), id == "file.open").map_err(|e| error("WORKSPACE_VIOLATION", e))?;
                    if path.is_dir() {
                        return Err(error("INVALID_PATH", "project/export paths must name a file, not a directory"));
                    }
                    let expected = if id == "file.exportMedia" { "mp4" } else { "fcproj" };
                    if path.extension().and_then(|s| s.to_str()).is_none_or(|s| !s.eq_ignore_ascii_case(expected)) {
                        return Err(error("UNSUPPORTED", format!("this operation requires .{expected}")));
                    }
                    if path.exists() && id != "file.open" && !guard.confirm {
                        return Err(error("CONFIRMATION_REQUIRED", "overwriting an existing file needs explicit user confirmation"));
                    }
                    let normalized = local_path(&path);
                    if let Some(job) = ledger.outputs.get(&normalized) {
                        let jobs = self.run_unchecked("jobs.list", json!({})).await.map_err(|e| error("APP_ERROR", e))?;
                        if jobs.as_array().is_some_and(|a| a.iter().any(|j| j["id"].as_u64() == Some(*job) && j["finished"] != true)) {
                            return Err(error("OUTPUT_BUSY", "an export still owns this output"));
                        }
                    }
                    if id == "file.open" {
                        if std::fs::metadata(&path).map_err(|e| error("IO_ERROR", e))?.len() > 64 * 1024 * 1024 {
                            return Err(error("INVALID_PROJECT", "project exceeds the 64 MiB agent limit"));
                        }
                        let bytes = std::fs::read(&path).map_err(|e| error("IO_ERROR", e))?;
                        let document: Value = serde_json::from_slice(&bytes).map_err(|e| error("INVALID_PROJECT", e))?;
                        w.document(&document).map_err(|e| error("WORKSPACE_VIOLATION", e))?;
                    }
                    params["path"] = json!(normalized);
                }
                "file.import" => {
                    let paths = params["paths"].as_array().ok_or_else(|| error("INVALID_PARAMS", "paths must be an array"))?;
                    if paths.is_empty() || paths.len() > 32 {
                        return Err(error("INVALID_PARAMS", "import needs 1 to 32 files"));
                    }
                    let mut resolved = Vec::new();
                    for path in paths {
                        let p = w.path(path.as_str().unwrap_or_default(), true).map_err(|e| error("WORKSPACE_VIOLATION", e))?;
                        if !p.is_file()
                            || !p.extension().and_then(|s| s.to_str()).is_some_and(|s| {
                                matches!(
                                    s.to_ascii_lowercase().as_str(),
                                    "mp4" | "mov" | "mkv" | "wav" | "mp3" | "flac" | "ogg" | "aiff" | "png" | "jpg" | "jpeg"
                                )
                            })
                        {
                            return Err(error("UNSUPPORTED", "unsupported media extension or directory"));
                        }
                        resolved.push(local_path(&p));
                    }
                    params["paths"] = json!(resolved);
                }
                "jobs.cancel" if !ledger.jobs.contains(&params["job"].as_u64().unwrap_or(0)) => {
                    return Err(error("UNKNOWN_JOB", "this MCP process does not own the job"));
                }
                "jobs.cancel" => {
                    let jobs = self.run_unchecked("jobs.list", json!({})).await.map_err(|e| error("APP_ERROR", e))?;
                    if jobs.as_array().is_some_and(|a| a.iter().any(|j| j["id"] == params["job"] && j["finished"] == true)) {
                        return Err(error("JOB_FINISHED", "the export has already finished"));
                    }
                }
                _ => {}
            }
            let result = self.run_unchecked("agent.execute", json!({"revision":guard.revision,"command":id,"params":params})).await.map_err(|e| {
                error(
                    if e.to_string().contains("revision conflict") {
                        "REVISION_CONFLICT"
                    } else if e.to_string().contains("delivery uncertain") {
                        "DELIVERY_UNCERTAIN"
                    } else {
                        "APP_ERROR"
                    },
                    e,
                )
            })?;
            if id == "file.exportMedia"
                && let Some(job) = result["job"].as_u64()
            {
                ledger.jobs.insert(job);
                if let Some(path) = result["path"].as_str() {
                    ledger.outputs.insert(path.into(), job);
                }
            }
            let project = self.run_unchecked("project.inspect", json!({})).await.map_err(|e| error("DELIVERY_UNCERTAIN", e))?;
            Ok(json!({"schema_version":1,"ok":true,"replayed":false,"result":result,"project":project}))
        }
        .await;
        let result = result.unwrap_or_else(|v| v);
        ledger.requests.insert(guard.request_id, (fingerprint, result.clone()));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_engine::Session;

    fn guard(id: &str, revision: u64, confirm: bool) -> Guard {
        Guard { request_id: id.into(), revision, confirm }
    }
    fn server(name: &str) -> (FilmcraftMcp, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("filmcraft-agent-{}-{name}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        (FilmcraftMcp::headless(Session::default()).with_workspace(&root).unwrap(), root)
    }
    async fn revision(s: &FilmcraftMcp) -> u64 {
        s.agent_read(false).await["project"]["revision"].as_u64().unwrap()
    }
    fn cleanup(root: std::path::PathBuf) {
        let resolved = root.canonicalize().unwrap();
        assert_eq!(resolved.parent(), Some(std::env::temp_dir().canonicalize().unwrap().as_path()));
        assert!(resolved.file_name().unwrap().to_string_lossy().starts_with(&format!("filmcraft-agent-{}-", std::process::id())));
        std::fs::remove_dir_all(resolved).unwrap();
    }

    #[tokio::test]
    async fn real_engine_edits_are_once_only_and_undoable() {
        let (s, root) = server("edit");
        let rev = revision(&s).await;
        let req = json!({"guard":{"request_id":"create","revision":rev},"action":"create","name":"測試 日本語 {n}"});
        let a = s.agent_project(serde_json::from_value(req.clone()).unwrap()).await;
        assert_eq!(a["ok"], true, "{a}");
        let b = s.agent_project(serde_json::from_value(req).unwrap()).await;
        assert_eq!(b["replayed"], true);
        assert_eq!(b["project"], a["project"]);
        let seq = EditTimeline {
            guard: guard("seq", revision(&s).await, false),
            edit: TimelineEdit::CreateSequence { name: "三語 timeline".into(), width: 320, height: 180, fps: 24 },
        };
        let a = s.agent_edit(seq).await;
        assert_eq!(a["ok"], true, "{a}");
        let before = s.agent_read(true).await;
        assert!(before["sequence"]["video"].is_array());
        let sequence = before["sequence"]["id"].as_u64().unwrap();
        let a = s.agent_history(History { guard: guard("undo", revision(&s).await, false), action: HistoryAction::Undo }).await;
        assert_eq!(a["ok"], true, "{a}");
        let a = s.agent_history(History { guard: guard("redo", revision(&s).await, false), action: HistoryAction::Redo }).await;
        assert_eq!(a["ok"], true, "{a}");
        let a = s.agent_edit(EditTimeline { guard: guard("activate", revision(&s).await, false), edit: TimelineEdit::ActivateSequence { sequence } }).await;
        assert_eq!(a["ok"], true, "{a}");
        assert_eq!(s.agent_read(true).await["sequence"]["id"], sequence);
        let a = s
            .agent_project(ProjectWrite {
                guard: guard("save", revision(&s).await, false),
                action: ProjectAction::Save,
                name: None,
                path: Some("test.fcproj".into()),
            })
            .await;
        assert_eq!(a["ok"], true, "{a}");
        assert!(root.join("test.fcproj").is_file());
        let a = s
            .agent_project(ProjectWrite {
                guard: guard("overwrite", revision(&s).await, false),
                action: ProjectAction::Save,
                name: None,
                path: Some("test.fcproj".into()),
            })
            .await;
        assert_eq!(a["error"]["code"], "CONFIRMATION_REQUIRED");
        let a = s
            .agent_project(ProjectWrite {
                guard: guard("open", revision(&s).await, false),
                action: ProjectAction::Open,
                name: None,
                path: Some("test.fcproj".into()),
            })
            .await;
        assert_eq!(a["ok"], true, "{a}");
        drop(s);
        cleanup(root);
    }

    #[tokio::test]
    async fn stale_conflicting_and_outside_requests_preserve_the_app() {
        let (s, root) = server("guards");
        let rev = revision(&s).await;
        let create = |name: &str| ProjectWrite { guard: guard("same-id", rev, false), action: ProjectAction::Create, name: Some(name.into()), path: None };
        assert_eq!(s.agent_project(create("First")).await["ok"], true);
        assert_eq!(s.agent_project(create("Second")).await["error"]["code"], "REQUEST_ID_CONFLICT");
        let a =
            s.agent_project(ProjectWrite { guard: guard("stale", rev, false), action: ProjectAction::Create, name: Some("Stale".into()), path: None }).await;
        assert_eq!(a["error"]["code"], "REVISION_CONFLICT");
        let a = s
            .agent_project(ProjectWrite {
                guard: guard("outside", revision(&s).await, false),
                action: ProjectAction::Save,
                name: None,
                path: Some("../outside.fcproj".into()),
            })
            .await;
        assert_eq!(a["error"]["code"], "WORKSPACE_VIOLATION");
        assert_eq!(s.agent_read(false).await["project"]["name"], "First");
        assert!(s.run("file.newProject", json!({"name":"bypass"})).await.is_err());
        assert!(s.run("agent.execute", json!({"revision":0,"command":"file.saveAs"})).await.is_err());
        assert_eq!(s.agent_job(999).await["error"]["code"], "UNKNOWN_JOB");
        drop(s);
        cleanup(root);
    }

    #[test]
    fn hostile_and_unknown_timeline_parameters_are_rejected() {
        assert!(
            serde_json::from_value::<EditTimeline>(
                json!({"guard":{"request_id":"x","revision":0},"edit":{"action":"split","clip":1,"frame":3,"path":"private.mp4"}})
            )
            .is_err()
        );
        assert!(TimelineEdit::Split { clip: 0, frame: 0 }.command().is_err());
        assert!(TimelineEdit::Split { clip: 1, frame: i64::MAX }.command().is_err());
        assert!(TimelineEdit::Trim { clip: 1, edge: Edge::Out, delta_frames: i64::MIN, ripple: true }.command().is_err());
        assert!(TimelineEdit::CreateSequence { name: "x".into(), width: u32::MAX, height: 0, fps: 0 }.command().is_err());
        assert!(TimelineEdit::Delete { clips: vec![] }.command().is_err());
    }
}
