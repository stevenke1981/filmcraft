//! Native exports own their staging files, never the user's existing destination.
//! Reservations cover all clients in this process. Publication is atomic per file, not per group.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::{ExportError, Result};

static RESERVED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
static NEXT: AtomicU64 = AtomicU64::new(1);
/// Includes the image-sequence pattern reservation and caption sidecars; bounds retained paths.
const MAX_OUTPUT_FILES: usize = 100_000;

fn reservations() -> &'static Mutex<HashSet<PathBuf>> {
    RESERVED.get_or_init(Mutex::default)
}

fn destination(path: &str) -> Result<(PathBuf, PathBuf)> {
    let path = Path::new(path);
    let name = path.file_name().ok_or_else(|| ExportError::Io("export destination must name a file".into()))?;
    // Windows treats these spellings as aliases (or alternate streams). Refuse them rather than
    // attempting to guess their destination, including when the path uses a verbatim prefix.
    if cfg!(windows) && (name.to_string_lossy().ends_with(['.', ' ']) || name.to_string_lossy().contains(':')) {
        return Err(ExportError::Io("export destination has an unsupported Windows file name".into()));
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|e| ExportError::Io(format!("{}: {e}", parent.display())))?;
    let final_path = parent.join(name);
    // Existing files can have multiple Windows spellings (including short names). Resolve their
    // actual file name first. For fresh paths, Unicode uppercase conservatively groups common
    // Windows aliases; this is not an implementation of a filesystem's complete upcase table.
    let identity = match fs::canonicalize(&final_path) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => final_path.clone(),
        Err(e) => return Err(ExportError::Io(format!("{}: {e}", final_path.display()))),
    };
    let key = if cfg!(windows) { PathBuf::from(identity.to_string_lossy().to_uppercase()) } else { identity };
    Ok((final_path, key))
}

// NativeOutput remains the sole cleanup owner, including when publication fails.
#[cfg(windows)]
fn publish_new(staged: &Path, final_path: &Path) -> std::io::Result<()> {
    let mut path = tempfile::TempPath::try_from_path(staged.to_path_buf())?;
    path.disable_cleanup(true);
    path.persist_noclobber(final_path).map_err(|error| error.error)
}

#[cfg(not(windows))]
fn publish_new(staged: &Path, final_path: &Path) -> std::io::Result<()> {
    fs::hard_link(staged, final_path)
}

#[derive(Debug)]
struct Entry {
    final_path: PathBuf,
    key: PathBuf,
    staged: PathBuf,
    replace: bool,
    ready: bool,
    required: bool,
}

/// One native job's output files. An `Arc` is held by each writer until its file has closed.
#[derive(Debug)]
pub struct NativeOutput {
    directory: PathBuf,
    overwrite: bool,
    entries: Mutex<BTreeMap<PathBuf, Entry>>,
    published: AtomicBool,
    max_files: usize,
}

impl NativeOutput {
    /// Reserve the primary destination synchronously, before a background job is started.
    pub(crate) fn new(path: &str, overwrite: bool, image_sequence: bool) -> Result<Self> {
        let (final_path, _) = destination(path)?;
        let parent = final_path.parent().ok_or_else(|| ExportError::Io("export destination has no parent".into()))?;
        let mut directory = None;
        for _ in 0..128 {
            let candidate = parent.join(format!(".filmcraft-export-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match fs::create_dir(&candidate) {
                Ok(()) => {
                    directory = Some(candidate);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(ExportError::Io(format!("{}: {e}", candidate.display()))),
            }
        }
        let output = Self {
            directory: directory.ok_or_else(|| ExportError::Io("could not allocate an export staging directory".into()))?,
            overwrite,
            entries: Mutex::new(BTreeMap::new()),
            published: AtomicBool::new(false),
            max_files: MAX_OUTPUT_FILES,
        };
        output.stage_path(path)?;
        if image_sequence && let Some(mut entry) = output.entries.lock().unwrap_or_else(|e| e.into_inner()).first_entry() {
            entry.get_mut().required = false;
        }
        Ok(output)
    }

    pub(crate) fn stage_path(&self, path: &str) -> Result<PathBuf> {
        if self.published.load(Ordering::Acquire) {
            return Err(ExportError::Io("export output was already published".into()));
        }
        let (final_path, key) = destination(path)?;
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = entries.get(&key) {
            return Ok(entry.staged.clone());
        }
        if entries.len() >= self.max_files {
            return Err(ExportError::Unsupported(format!("export exceeds the {} output-file reservation limit", self.max_files)));
        }
        if final_path.parent() != self.directory.parent() {
            return Err(ExportError::Unsupported("export output files must share one directory".into()));
        }
        let replace = match fs::symlink_metadata(&final_path) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
                return Err(ExportError::Io(format!("{}: destination is not a regular file", final_path.display())));
            }
            Ok(_) if !self.overwrite => {
                return Err(ExportError::Io(format!("{}: overwrite confirmation required", final_path.display())));
            }
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(ExportError::Io(format!("{}: {e}", final_path.display()))),
        };
        // A replacement can only be the sole entry, so checking the first entry is sufficient.
        if !entries.is_empty() && (replace || entries.first_key_value().is_some_and(|(_, entry)| entry.replace)) {
            return Err(ExportError::Unsupported("replacing existing files in a multi-file export is not supported safely; choose new output names".into()));
        }
        let mut reserved = reservations().lock().unwrap_or_else(|e| e.into_inner());
        if !reserved.insert(key.clone()) {
            return Err(ExportError::Io(format!("{}: output is busy in another export", final_path.display())));
        }
        let staged = self.directory.join(format!("output-{}", entries.len()));
        entries.insert(key.clone(), Entry { final_path, key, staged: staged.clone(), replace, ready: false, required: true });
        Ok(staged)
    }

    pub(crate) fn create(&self, path: &str) -> Result<File> {
        let staged = self.stage_path(path)?;
        OpenOptions::new().write(true).read(true).create_new(true).open(&staged).map_err(|e| ExportError::Io(format!("{}: {e}", staged.display())))
    }

    pub(crate) fn ready(&self, path: &str) -> Result<()> {
        let (_, key) = destination(path)?;
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get_mut(&key).ok_or_else(|| ExportError::Io("export writer does not own this output".into()))?;
        entry.ready = true;
        Ok(())
    }

    /// Stage a caption sidecar as part of the same job; existing sidecars remain untouched.
    pub fn write(&self, path: &str, bytes: &[u8]) -> Result<()> {
        let mut file = self.create(path)?;
        file.write_all(bytes).and_then(|()| file.sync_all()).map_err(|e| ExportError::Io(format!("{path}: {e}")))?;
        drop(file);
        self.ready(path)
    }

    pub(crate) fn publish(&self, cancel: &AtomicBool) -> Result<()> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if cancel.load(Ordering::Relaxed) {
            return Err(ExportError::Cancelled);
        }
        if self.published.load(Ordering::Acquire) {
            return Err(ExportError::Io("export output was already published".into()));
        }
        if entries.values().any(|entry| entry.required && !entry.ready) {
            return Err(ExportError::Io("export output has not been finalized; nothing was published".into()));
        }
        // Only the image-sequence base path can remain unwritten; actual files must have been
        // flushed, synced and closed before they are eligible for publication.
        for (published, entry) in entries.values().filter(|entry| entry.ready).enumerate() {
            let result = if entry.replace {
                // std::fs::rename replaces a file on supported native platforms. Never pre-delete
                // the old destination to make a failed replacement succeed.
                fs::rename(&entry.staged, &entry.final_path)
            } else {
                // Publishing a fresh path must refuse a target that appeared after reservation.
                // Same-parent staging keeps both names on the same filesystem.
                publish_new(&entry.staged, &entry.final_path)
            };
            if let Err(e) = result {
                return Err(ExportError::Io(format!(
                    "{}: could not publish export: {e}; {published} complete new file(s) already published (output groups are not atomic)",
                    entry.final_path.display()
                )));
            }
        }
        self.published.store(true, Ordering::Release);
        Ok(())
    }

    /// Remove only this job's staging. Reservations stay held until the job's guard is dropped.
    pub fn cleanup_staging(&self) -> Result<()> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        for entry in entries.values() {
            if let Err(e) = fs::remove_file(&entry.staged)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(ExportError::Io(format!("could not remove export staging file {}: {e}", entry.staged.display())));
            }
        }
        if let Err(e) = fs::remove_dir(&self.directory)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(ExportError::Io(format!("could not remove export staging directory {}: {e}", self.directory.display())));
        }
        Ok(())
    }
}

impl Drop for NativeOutput {
    fn drop(&mut self) {
        if let Err(e) = self.cleanup_staging() {
            log::warn!("{e}");
        }
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let mut reserved = reservations().lock().unwrap_or_else(|e| e.into_inner());
        for entry in entries.values() {
            reserved.remove(&entry.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExportSettings, Exporter, Format, Out, Progress, Step, export};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("filmcraft-output-safety-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self, name: &str) -> String {
            self.0.join(name).to_str().unwrap().to_owned()
        }
        fn only(&self, names: &[&str]) {
            let mut actual: Vec<_> = fs::read_dir(&self.0).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            actual.sort();
            let mut expected: Vec<_> = names.iter().map(|s| s.to_string()).collect();
            expected.sort();
            assert_eq!(actual, expected, "no job staging should remain");
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn unconfirmed_overwrite_and_constructor_errors_preserve_files() {
        let dir = Scratch::new();
        let path = dir.path("old.mp4");
        fs::write(&path, b"original").unwrap();
        assert!(NativeOutput::new(&path, false, false).unwrap_err().to_string().contains("confirmation"));
        assert!(NativeOutput::new(&dir.path("missing/out.mp4"), true, false).is_err());
        fs::create_dir(dir.path("folder.mp4")).unwrap();
        assert!(NativeOutput::new(&dir.path("folder.mp4"), true, false).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original");
        dir.only(&["old.mp4", "folder.mp4"]);
    }

    #[test]
    fn fresh_publication_cannot_clobber_an_unexpected_target_even_with_consent() {
        let dir = Scratch::new();
        let path = dir.path("new.mp4");
        let output = NativeOutput::new(&path, true, false).unwrap();
        output.write(&path, b"complete new output").unwrap();
        let staged = output.stage_path(&path).unwrap();
        fs::write(&path, b"appeared while rendering").unwrap();
        assert!(output.publish(&AtomicBool::new(false)).is_err());
        assert!(!output.published.load(Ordering::Acquire));
        assert_eq!(fs::read(&path).unwrap(), b"appeared while rendering");
        assert_eq!(fs::read(&staged).unwrap(), b"complete new output");
        assert!(NativeOutput::new(&path, true, false).unwrap_err().to_string().contains("busy"));
        drop(output);
        assert!(!staged.exists());
        assert_eq!(fs::read(&path).unwrap(), b"appeared while rendering");
        dir.only(&["new.mp4"]);
    }

    #[test]
    fn successful_fresh_and_confirmed_single_file_publication() {
        let dir = Scratch::new();
        let path = dir.path("new.mp4");
        for (overwrite, bytes) in [(false, b"first".as_slice()), (true, b"replacement".as_slice())] {
            let output = NativeOutput::new(&path, overwrite, false).unwrap();
            output.write(&path, bytes).unwrap();
            let publication = output.publish(&AtomicBool::new(false));
            if let Err(error) = &publication {
                let entries = output.entries.lock().unwrap_or_else(|e| e.into_inner());
                eprintln!(
                    "publication failure: error={error:?}, overwrite={overwrite}, configured_overwrite={}, expected_bytes={}, pid={}, cwd={:?}, temp={:?}, scratch={:?}, staging_directory={:?}, published={}, entries={}",
                    output.overwrite,
                    bytes.len(),
                    std::process::id(),
                    std::env::current_dir(),
                    std::env::temp_dir(),
                    dir.0,
                    output.directory,
                    output.published.load(Ordering::Acquire),
                    entries.len()
                );
                for entry in entries.values() {
                    eprintln!(
                        "publication entry: operation={}, replace={}, ready={}, required={}, final={:?}, staged={:?}, final_exists={:?}, staged_exists={:?}, final_metadata={:?}, staged_metadata={:?}, parent_metadata={:?}",
                        if entry.replace {
                            "rename"
                        } else if cfg!(windows) {
                            "move_noclobber"
                        } else {
                            "hard_link"
                        },
                        entry.replace,
                        entry.ready,
                        entry.required,
                        entry.final_path,
                        entry.staged,
                        entry.final_path.try_exists(),
                        entry.staged.try_exists(),
                        fs::symlink_metadata(&entry.final_path).map(|m| (m.is_file(), m.file_type().is_symlink(), m.len(), m.permissions().readonly())),
                        fs::symlink_metadata(&entry.staged).map(|m| (m.is_file(), m.file_type().is_symlink(), m.len(), m.permissions().readonly())),
                        fs::symlink_metadata(&dir.0).map(|m| (m.is_dir(), m.file_type().is_symlink(), m.permissions().readonly()))
                    );
                }
            }
            publication.unwrap();
            drop(output);
            assert_eq!(fs::read(&path).unwrap(), bytes);
            dir.only(&["new.mp4"]);
        }
    }

    #[test]
    fn incomplete_required_output_cannot_report_success() {
        let dir = Scratch::new();
        let path = dir.path("old.mp4");
        fs::write(&path, b"original").unwrap();
        let output = NativeOutput::new(&path, true, false).unwrap();
        assert!(output.publish(&AtomicBool::new(false)).unwrap_err().to_string().contains("not been finalized"));
        drop(output);
        assert_eq!(fs::read(&path).unwrap(), b"original");
        dir.only(&["old.mp4"]);
    }

    #[test]
    fn output_file_cap_refuses_new_reservations_without_publishing() {
        let dir = Scratch::new();
        let path = dir.path("fresh.mp4");
        let mut output = NativeOutput::new(&path, false, false).unwrap();
        output.max_files = 2;
        output.write(&path, b"complete video").unwrap();
        output.write(&dir.path("fresh.srt"), b"complete captions").unwrap();
        assert!(output.write(&dir.path("extra.vtt"), b"extra").unwrap_err().to_string().contains("reservation limit"));
        assert_eq!(output.entries.lock().unwrap().len(), 2);
        drop(output);
        dir.only(&[]);
    }

    #[test]
    fn publication_failure_keeps_the_old_file_and_releases_reservation() {
        let dir = Scratch::new();
        let path = dir.path("old.mp4");
        fs::write(&path, b"original").unwrap();
        let output = NativeOutput::new(&path, true, false).unwrap();
        output.write(&path, b"new").unwrap();
        fs::remove_file(output.stage_path(&path).unwrap()).unwrap();
        assert!(output.publish(&AtomicBool::new(false)).is_err());
        drop(output);
        assert_eq!(fs::read(&path).unwrap(), b"original");
        drop(NativeOutput::new(&path, true, false).unwrap());
        dir.only(&["old.mp4"]);
    }

    #[test]
    fn caption_groups_refuse_existing_output_replacement() {
        let dir = Scratch::new();
        let path = dir.path("old.mp4");
        let sidecar = dir.path("old.srt");
        fs::write(&path, b"original video").unwrap();
        fs::write(&sidecar, b"original captions").unwrap();
        let output = NativeOutput::new(&path, true, false).unwrap();
        assert!(output.write(&sidecar, b"new captions").unwrap_err().to_string().contains("multi-file"));
        drop(output);
        assert_eq!(fs::read(&path).unwrap(), b"original video");
        assert_eq!(fs::read(&sidecar).unwrap(), b"original captions");
        dir.only(&["old.mp4", "old.srt"]);
    }

    #[test]
    fn cancel_and_failed_group_preparation_never_publish_sidecars() {
        let dir = Scratch::new();
        let path = dir.path("fresh.mp4");
        let sidecar = dir.path("fresh.srt");
        let output = NativeOutput::new(&path, false, false).unwrap();
        output.write(&sidecar, b"new captions").unwrap();
        output.write(&path, b"new video").unwrap();
        let video_staged = output.stage_path(&path).unwrap();
        let caption_staged = output.stage_path(&sidecar).unwrap();
        assert!(matches!(output.publish(&AtomicBool::new(true)), Err(ExportError::Cancelled)));
        assert!(!output.published.load(Ordering::Acquire));
        assert!(!Path::new(&path).exists());
        assert!(!Path::new(&sidecar).exists());
        assert_eq!(fs::read(&video_staged).unwrap(), b"new video");
        assert_eq!(fs::read(&caption_staged).unwrap(), b"new captions");
        drop(output);
        dir.only(&[]);
        let output = NativeOutput::new(&path, false, false).unwrap();
        output.write(&sidecar, b"new captions").unwrap();
        let caption_staged = output.stage_path(&sidecar).unwrap();
        assert!(output.publish(&AtomicBool::new(false)).unwrap_err().to_string().contains("not been finalized"));
        assert!(!output.published.load(Ordering::Acquire));
        assert!(!Path::new(&path).exists());
        assert!(!Path::new(&sidecar).exists());
        assert_eq!(fs::read(&caption_staged).unwrap(), b"new captions");
        drop(output);
        dir.only(&[]);
    }

    #[test]
    fn group_publication_error_reports_complete_new_files_and_preserves_unexpected_sidecar() {
        let dir = Scratch::new();
        let path = dir.path("fresh.mp4");
        let sidecar = dir.path("fresh.srt");
        let later = dir.path("fresh.vtt");
        let output = NativeOutput::new(&path, true, false).unwrap();
        output.write(&path, b"complete video").unwrap();
        output.write(&sidecar, b"new captions").unwrap();
        output.write(&later, b"later captions").unwrap();
        let sidecar_staged = output.stage_path(&sidecar).unwrap();
        let later_staged = output.stage_path(&later).unwrap();
        fs::write(&sidecar, b"external captions").unwrap();
        let error = output.publish(&AtomicBool::new(false)).unwrap_err().to_string();
        assert!(error.contains("1 complete new file(s) already published"), "{error}");
        assert!(!output.published.load(Ordering::Acquire));
        assert_eq!(fs::read(&path).unwrap(), b"complete video");
        assert_eq!(fs::read(&sidecar).unwrap(), b"external captions");
        assert!(!Path::new(&later).exists());
        assert_eq!(fs::read(&sidecar_staged).unwrap(), b"new captions");
        assert_eq!(fs::read(&later_staged).unwrap(), b"later captions");
        output.cleanup_staging().unwrap();
        assert!(!sidecar_staged.exists());
        assert!(!later_staged.exists());
        assert_eq!(fs::read(&path).unwrap(), b"complete video");
        assert_eq!(fs::read(&sidecar).unwrap(), b"external captions");
        assert!(!Path::new(&later).exists());
        assert!(NativeOutput::new(&later, false, false).unwrap_err().to_string().contains("busy"));
        drop(output);
        drop(NativeOutput::new(&later, false, false).unwrap());
        dir.only(&["fresh.mp4", "fresh.srt"]);
    }

    #[test]
    fn writer_retains_reservation_until_its_file_has_closed() {
        let dir = Scratch::new();
        let path = dir.path("fresh.mp4");
        let mut settings = ExportSettings { path: path.clone(), ..Default::default() };
        settings.prepare_output().unwrap();
        let writer = Out::create(&settings).unwrap();
        drop(settings);
        assert!(NativeOutput::new(&path, false, false).unwrap_err().to_string().contains("busy"));
        drop(writer);
        drop(NativeOutput::new(&path, false, false).unwrap());
        dir.only(&[]);
    }

    #[test]
    fn early_and_mid_cancel_preserve_a_complete_existing_mp4() {
        let dir = Scratch::new();
        let path = dir.path("old.mp4");
        let (project, seq, sources) = crate::tests::project();
        let settings = ExportSettings { path: path.clone(), frame_size: Some((64, 36)), include_audio: false, ..Default::default() };
        export(&project, seq, &settings, &sources, &Progress::default()).unwrap();
        let original = fs::read(&path).unwrap();
        let parsed = filmcraft_isobmff::open(original.as_slice()).unwrap();
        assert_eq!(parsed.tracks[0].samples.len(), 24);
        for frames in [0, 17] {
            let settings = ExportSettings { overwrite: true, ..settings.clone() };
            let progress = Progress::default();
            let mut exporter = Exporter::new(project.clone(), seq, &settings, &progress).unwrap();
            exporter.set_batch(1);
            exporter.set_overlap(false);
            for _ in 0..frames {
                assert!(matches!(exporter.step(&sources, &progress).unwrap(), Step::Progress));
            }
            assert_eq!(progress.done.load(Ordering::Relaxed), frames);
            assert_eq!(fs::read(&path).unwrap(), original);
            progress.cancel.store(true, Ordering::Relaxed);
            assert!(matches!(exporter.step(&sources, &progress), Err(ExportError::Cancelled)));
            drop(exporter);
            assert_eq!(fs::read(&path).unwrap(), original);
            dir.only(&["old.mp4"]);
        }
    }

    #[test]
    fn setup_failure_preserves_existing_mp4_and_sidecar() {
        let dir = Scratch::new();
        let path = dir.path("old.mp4");
        let sidecar = dir.path("old.srt");
        fs::write(&path, b"original video").unwrap();
        fs::write(&sidecar, b"original captions").unwrap();
        let (project, seq, _) = crate::tests::project();
        let settings = ExportSettings { format: Format::Hevc, overwrite: true, path: path.clone(), ..Default::default() };
        assert!(Exporter::new(project, seq, &settings, &Progress::default()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original video");
        assert_eq!(fs::read(&sidecar).unwrap(), b"original captions");
        dir.only(&["old.mp4", "old.srt"]);
    }

    #[test]
    fn missing_ready_staging_cannot_publish_a_fresh_destination() {
        let dir = Scratch::new();
        let path = dir.path("fresh.mp4");
        let output = NativeOutput::new(&path, false, false).unwrap();
        output.write(&path, b"complete output").unwrap();
        let staged = output.stage_path(&path).unwrap();
        fs::remove_file(&staged).unwrap();
        let error = output.publish(&AtomicBool::new(false)).unwrap_err().to_string();
        assert!(error.contains("0 complete new file(s) already published"), "{error}");
        assert!(!output.published.load(Ordering::Acquire));
        assert!(!Path::new(&path).exists());
        assert!(!staged.exists());
        drop(output);
        drop(NativeOutput::new(&path, false, false).unwrap());
        dir.only(&[]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_locked_fresh_staging_survives_failure_until_owned_cleanup() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = Scratch::new();
        let path = dir.path("fresh.mp4");
        let output = NativeOutput::new(&path, false, false).unwrap();
        output.write(&path, b"complete output").unwrap();
        let staged = output.stage_path(&path).unwrap();
        let directory = output.directory.clone();
        let locked = OpenOptions::new().read(true).share_mode(3).open(&staged).unwrap();
        assert!(output.publish(&AtomicBool::new(false)).is_err());
        assert!(!output.published.load(Ordering::Acquire));
        assert!(!Path::new(&path).exists());
        assert_eq!(fs::read(&staged).unwrap(), b"complete output");
        assert!(NativeOutput::new(&path, false, false).unwrap_err().to_string().contains("busy"));
        drop(locked);
        output.cleanup_staging().unwrap();
        assert!(!staged.exists());
        assert!(!directory.exists());
        assert!(NativeOutput::new(&path, false, false).unwrap_err().to_string().contains("busy"));
        drop(output);
        drop(NativeOutput::new(&path, false, false).unwrap());
        dir.only(&[]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_unicode_temp_fresh_then_confirmed_publication_preserves_each_phase() {
        let dir = Scratch::new();
        let parent = dir.0.join("安全-名前");
        fs::create_dir(&parent).unwrap();
        let mut expected = Vec::new();
        // Independent fixtures, never a retry of a failed publication.
        for i in 0..100 {
            let name = format!("影片-動画-{i:03}.mp4");
            let path = parent.join(&name).to_str().unwrap().to_owned();
            for (overwrite, bytes) in [(false, b"first".as_slice()), (true, b"replacement".as_slice())] {
                let output = NativeOutput::new(&path, overwrite, false).unwrap();
                let directory = output.directory.clone();
                output.write(&path, bytes).unwrap();
                output.publish(&AtomicBool::new(false)).unwrap();
                assert!(output.published.load(Ordering::Acquire));
                drop(output);
                assert_eq!(fs::read(&path).unwrap(), bytes);
                assert!(!directory.exists());
                assert!(NativeOutput::new(&path, false, false).unwrap_err().to_string().contains("confirmation"));
                assert_eq!(fs::read(&path).unwrap(), bytes);
            }
            expected.push(name);
            let mut actual: Vec<_> = fs::read_dir(&parent).unwrap().map(|e| e.unwrap().file_name().to_str().unwrap().to_owned()).collect();
            actual.sort();
            assert_eq!(actual, expected, "only complete final files remain after each fixture");
        }
        dir.only(&["安全-名前"]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_case_aliases_share_one_reservation() {
        let dir = Scratch::new();
        let output = NativeOutput::new(&dir.path("Target.MP4"), false, false).unwrap();
        for name in ["Target.MP4.", "Target.MP4 ", "Target.MP4:stream"] {
            assert!(NativeOutput::new(&dir.path(name), true, false).unwrap_err().to_string().contains("unsupported Windows file name"));
        }
        assert!(NativeOutput::new(&dir.path("target.mp4"), true, false).unwrap_err().to_string().contains("busy"));
        drop(output);
        drop(NativeOutput::new(&dir.path("target.mp4"), false, false).unwrap());
        dir.only(&[]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_unicode_aliases_share_one_reservation() {
        let dir = Scratch::new();
        for (name, alias) in [("\u{03c3}.mp4", "\u{03a3}.mp4"), ("\u{00e9}.mp4", "\u{00c9}.mp4")] {
            let path = dir.path(name);
            let alias = dir.path(alias);
            fs::write(&path, b"original").unwrap();
            // Establish the real filesystem alias relationship before testing our reservation.
            assert_eq!(fs::canonicalize(&path).unwrap(), fs::canonicalize(&alias).unwrap());
            assert_eq!(fs::read(&alias).unwrap(), b"original");
            let output = NativeOutput::new(&path, true, false).unwrap();
            assert!(NativeOutput::new(&alias, true, false).unwrap_err().to_string().contains("busy"));
            drop(output);
            assert_eq!(fs::read(&alias).unwrap(), b"original");
            fs::remove_file(&path).unwrap();
            let output = NativeOutput::new(&path, false, false).unwrap();
            assert!(NativeOutput::new(&alias, false, false).unwrap_err().to_string().contains("busy"));
            drop(output);
        }
        // The native filesystem need not alias every pair Rust uppercase groups. Conservative
        // fresh-name reservations still refuse a second writer for these potential aliases.
        for (name, alias) in [("\u{03c3}.mp4", "\u{03c2}.mp4"), ("\u{017f}.mp4", "S.mp4")] {
            let output = NativeOutput::new(&dir.path(name), false, false).unwrap();
            assert!(NativeOutput::new(&dir.path(alias), false, false).unwrap_err().to_string().contains("busy"));
            drop(output);
        }
        dir.only(&[]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_replacement_io_failure_preserves_the_original() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = Scratch::new();
        let path = dir.path("old.mp4");
        fs::write(&path, b"original").unwrap();
        let locked = OpenOptions::new().read(true).share_mode(1).open(&path).unwrap();
        let output = NativeOutput::new(&path, true, false).unwrap();
        output.write(&path, b"replacement").unwrap();
        let staged = output.stage_path(&path).unwrap();
        assert!(output.publish(&AtomicBool::new(false)).is_err());
        assert!(!output.published.load(Ordering::Acquire));
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read(&staged).unwrap(), b"replacement");
        drop(output);
        assert!(!staged.exists());
        drop(locked);
        drop(NativeOutput::new(&path, true, false).unwrap());
        dir.only(&["old.mp4"]);
    }
}
