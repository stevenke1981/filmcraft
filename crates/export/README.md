# Native export output safety

Native blocking and stepped media exports share `NativeOutput`. This batch covers
`file.exportMedia`, `export.quick` and `export.queue.*` through UI, CLI, control and MCP entry points;
project saving, frame exports and preset/settings writes need separate verification.
The engine reserves the primary destination before starting
a worker. Each job creates an exclusive staging directory beside the destination; writers retain
the ownership guard until their files close. Cancellation, encoding/setup failure and panic
unwinding remove only that job's staging files. Existing final paths are never deleted by cleanup.

The reservation is **process-wide**, with canonical parent directories and existing final files,
plus conservative Unicode-uppercase Windows keys for fresh names. Native tests establish common
ASCII, Greek sigma and accented-letter aliases on this machine; some uppercase-equivalent pairs
(final sigma/sigma and long-s/S) are conservatively grouped even though this filesystem treats them
as distinct. Rust uppercase is not exact parity with the
[filesystem's upcase table](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/nf-ntifs-_fsrtl_advanced_fcb_header-fsrtlarenamesequal);
other aliases/filesystems need separate coverage. Windows names ending in dots/spaces or containing
alternate-stream syntax are refused.
It is not a cross-process lock or a filesystem security boundary. Reservations also cover dynamic
image-sequence and OP-Atom files, with at most 100,000 retained output paths per job (including the
image-sequence pattern and any sidecar). Paths are stored in a bounded ordered map, not allocated
from an input frame count.

After every required output is flushed, synced and closed, publication uses:

- A [hard link](https://doc.rust-lang.org/std/fs/fn.hard_link.html) for a destination absent when
  it was reserved. This cannot overwrite a target that appears during rendering, even if the
  invocation supplied generic confirmation. Filesystems without hard-link support return an I/O
  error; there is no fallback to truncation or `exists()` followed by replacement.
- [Rename](https://doc.rust-lang.org/std/fs/fn.rename.html) for an existing single-file destination
  with explicit overwrite consent. There is no pre-delete fallback. Failed replacement leaves
  the existing file in place. Same-parent staging keeps source and destination on one filesystem.

`file.exportMedia`, `export.quick` and `export.queue.add` accept flat `overwrite: bool = false`.
Stored presets and settings JSON cannot import consent. MCP v1 forwards `guard.confirm` to the
same engine parameter. Export/Quick Export/Send to Queue buttons capture the invocation and ask
before replacement; an intervening project edit or replacement invalidates that captured consent.
Confirmation also re-resolves and compares the complete settings, effective range, derived output
and final destination. Preset save/import changes require a fresh export request and confirmation,
even when they leave the project revision and identity unchanged. A missing/deleted preset aborts
the pending request with an error; the old overwrite consent is cleared.
The UI reports completed, cancelled and failed export jobs, including short jobs and out-of-order
completion. Safety prompts/outcomes have English source and Traditional Chinese, Japanese and
Spanish translations; Portuguese retains the existing menu-only catalog/fallback contract.
The footer classifies exports using the stable internal `Export `, `Quick Export ` and `Queue:` job
labels; helper regressions cover all three routes. On normal return, native engine workers store
their terminal result, drop output ownership and set `finished` with
Release ordering while holding the result mutex. Result/queue observers therefore see completion
only after the destination reservation has been released, and `finished` implies a terminal result.

Caption sidecars are staged with their native export rather than written before encoding. Replacing
an existing file in a multi-file group (video plus captions, image sequences, OP-Atom) is explicitly
unsupported: choose fresh names. Fresh groups publish complete files individually. A later
publication error may leave some complete new files; the error reports the count and cleanup does
not remove final paths. This is **not group-atomic publication**. Cancellation is checked before
publication starts; once publication begins, a late cancellation does not interrupt the group or
roll back published files. No power-loss durability guarantee is made.

The legacy H.264-only `removeCancelledOutput` parameter is retained for compatibility. It no longer
controls final-file deletion: job-owned staging cleanup applies to every native export format.
In-memory/web sinks commit each file when their callback accepts the complete bytes. Cancellation
raised by that callback does not turn an already delivered single-file export into a cancelled
result. Blocking WAV and stepped MJPEG regressions parse the delivered container and check its
samples/frames plus a successful report with the cancellation flag still set. This is not a new
atomic sink contract for multi-file exports; later work or a later callback can still fail.

Focused regressions live in `output::tests`, engine `export_tests`, UI `export_mode::tests` and
automation `agent::tests`. They cover untouched complete MP4 bytes across early/mid cancellation,
constructor/error/drop cleanup, real Windows replacement failure, fresh no-clobber races, existing
sidecar preservation, group publication failures, shared client reservations and native Windows
case/Unicode aliases, consent propagation/invalidated UI consent, terminal UI outcomes and translation
placeholder parity. Run Cargo with one build job and one test thread on a shared machine.

UI tests are engineering regressions, not native manual acceptance. Cross-process writers, other
filesystems/platforms, process termination/power loss, and native desktop interaction require
separate acceptance coverage. Full repository gates remain required before committing.
