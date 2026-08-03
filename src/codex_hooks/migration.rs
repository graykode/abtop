//! Removal of the retired shell-function based Codex integration.
//!
//! This module recognizes only abtop's exact legacy marker lines. It never
//! searches for, edits, or removes an unrelated `codex` alias or function.

use std::collections::BTreeMap;
use std::fs;
use std::fs::File;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::thread;
#[cfg(not(unix))]
use std::time::SystemTime;
#[cfg(unix)]
use std::time::{Duration, Instant};

pub const LEGACY_START_MARKER: &str = "# >>> abtop managed codex >>>";
pub const LEGACY_END_MARKER: &str = "# <<< abtop managed codex <<<";
const LEGACY_MARKER_NAMESPACE: &str = "abtop managed codex";
const MAX_PROFILE_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(unix)]
const LEGACY_HOME_LOCK_FILE: &str = ".abtop-codex-migration.lock";
#[cfg(unix)]
const ZDOTDIR_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(unix)]
const ZDOTDIR_PROBE_OUTPUT_LIMIT: usize = 4096;
#[cfg(unix)]
const ZDOTDIR_PROBE_BEGIN: &[u8] = b"\x1eABTOP_ZDOTDIR_V1_BEGIN\x1f";
#[cfg(unix)]
const ZDOTDIR_PROBE_END: &[u8] = b"\x1eABTOP_ZDOTDIR_V1_END\x1f";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MigrationReport {
    pub scanned_files: Vec<PathBuf>,
    pub changed_files: Vec<PathBuf>,
    pub powershell_guidance: Option<String>,
}

#[derive(Debug)]
struct PreparedEdit {
    requested_path: PathBuf,
    path: PathBuf,
    original: Vec<u8>,
    updated: Vec<u8>,
    permissions: fs::Permissions,
    original_identity: FileIdentity,
    installed_identity: Option<FileIdentity>,
    committed: bool,
}

/// A multi-file legacy cleanup transaction.
///
/// Call [`commit`](Self::commit) only after the replacement Codex integration
/// has installed successfully. Calling [`rollback`](Self::rollback), or
/// dropping an unfinished transaction, restores every profile that this
/// transaction changed, provided no concurrent editor changed it afterwards.
#[derive(Debug)]
pub struct LegacyCleanupTransaction {
    home: PathBuf,
    lock: LegacyHomeLock,
    report: MigrationReport,
    edits: Vec<PreparedEdit>,
    finished: bool,
}

impl LegacyCleanupTransaction {
    pub fn begin() -> io::Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "cannot determine the current user's home directory",
            )
        })?;
        Self::begin_at(&home)
    }

    pub fn begin_at(home: &Path) -> io::Result<Self> {
        let home = canonical_home(home)?;
        // One stable lock serializes the complete legacy migration, including
        // no-op scans. Keeping the inode on disk prevents an unlink/recreate
        // window in which two abtop processes could lock different files.
        let lock = LegacyHomeLock::acquire(&home, LockMode::Exclusive)?;
        let candidates = legacy_profile_candidates(&home)?;
        let mut edits = Vec::new();
        let mut report = MigrationReport {
            powershell_guidance: powershell_guidance(),
            ..MigrationReport::default()
        };

        // Resolve, validate, and prepare every candidate before changing any
        // file. Canonical targets deduplicate profile aliases and symlinks.
        let mut targets = BTreeMap::<PathBuf, PathBuf>::new();
        for requested in candidates {
            let Some(target) = validated_existing_profile(&requested, &home)? else {
                continue;
            };
            targets.entry(target).or_insert(requested);
        }

        for (target, requested) in targets {
            let metadata = fs::metadata(&target)?;
            let original = read_profile_bounded(&target, &metadata)?;
            let text = std::str::from_utf8(&original).map_err(|_| {
                invalid_data(format!(
                    "legacy shell file {} is not valid UTF-8",
                    requested.display()
                ))
            })?;
            let Some(updated) = remove_marked_block(text)? else {
                report.scanned_files.push(requested);
                continue;
            };
            report.scanned_files.push(requested.clone());
            revalidate_requested_target(&requested, &target, &home)?;
            let current = fs::symlink_metadata(&target)?;
            if !same_file_identity(&metadata, &current) {
                return Err(invalid_data(format!(
                    "legacy shell file {} changed while cleanup was prepared",
                    requested.display()
                )));
            }
            edits.push(PreparedEdit {
                requested_path: requested,
                path: target,
                original,
                updated: updated.into_bytes(),
                permissions: metadata.permissions(),
                original_identity: FileIdentity::from_metadata(&metadata),
                installed_identity: None,
                committed: false,
            });
        }
        lock.revalidate()?;

        let mut transaction = Self {
            home,
            lock,
            report,
            edits,
            finished: false,
        };
        if let Err(error) = transaction.apply() {
            let rollback = transaction.rollback_internal();
            transaction.finished = true;
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(io::Error::new(
                    error.kind(),
                    format!(
                        "{error}; additionally failed to roll back legacy shell cleanup: {rollback_error}"
                    ),
                )),
            };
        }
        Ok(transaction)
    }

    #[allow(dead_code)]
    pub fn report(&self) -> &MigrationReport {
        &self.report
    }

    pub fn commit(mut self) -> MigrationReport {
        self.finished = true;
        self.report.clone()
    }

    pub fn rollback(&mut self) -> io::Result<()> {
        let result = self.rollback_internal();
        if result.is_ok() {
            self.finished = true;
        }
        result
    }

    fn apply(&mut self) -> io::Result<()> {
        for edit in &mut self.edits {
            self.lock.revalidate()?;
            revalidate_requested_target(&edit.requested_path, &edit.path, &self.home)?;
            let metadata = fs::symlink_metadata(&edit.path)?;
            if !edit.original_identity.matches(&metadata) {
                return Err(invalid_data(format!(
                    "legacy shell file {} was replaced during cleanup",
                    edit.path.display()
                )));
            }
            let current = read_profile_bounded(&edit.path, &metadata)?;
            if current != edit.original {
                return Err(invalid_data(format!(
                    "legacy shell file {} changed during cleanup; retry",
                    edit.path.display()
                )));
            }
            revalidate_requested_target(&edit.requested_path, &edit.path, &self.home)?;
            if !edit
                .original_identity
                .matches(&fs::symlink_metadata(&edit.path)?)
            {
                return Err(invalid_data(format!(
                    "legacy shell file {} was replaced immediately before cleanup",
                    edit.path.display()
                )));
            }
            let PreparedEdit {
                requested_path,
                path,
                original,
                updated,
                permissions,
                original_identity,
                installed_identity,
                committed,
            } = edit;
            atomic_replace(
                AtomicReplacement {
                    requested_path,
                    path,
                    home: &self.home,
                    bytes: updated,
                    permissions,
                    expected_identity: original_identity,
                    expected_bytes: original,
                    lock: &self.lock,
                },
                |identity| {
                    // There must be no fallible operation between the rename
                    // and recording that rollback now owns the replacement.
                    *installed_identity = identity;
                    *committed = installed_identity.is_some();
                },
            )?;
            self.report.changed_files.push(edit.requested_path.clone());
        }
        Ok(())
    }

    fn rollback_internal(&mut self) -> io::Result<()> {
        let mut errors = Vec::new();
        for edit in self.edits.iter_mut().rev() {
            if !edit.committed {
                continue;
            }
            if let Err(error) = self.lock.revalidate() {
                errors.push(format!(
                    "legacy cleanup lock changed before rollback: {error}"
                ));
                break;
            }
            let installed_metadata = match fs::symlink_metadata(&edit.path) {
                Ok(metadata)
                    if edit
                        .installed_identity
                        .as_ref()
                        .is_some_and(|identity| identity.matches(&metadata)) =>
                {
                    metadata
                }
                _ => {
                    errors.push(format!(
                        "{} was replaced after cleanup; refusing to overwrite it",
                        edit.path.display()
                    ));
                    continue;
                }
            };
            let current = match read_profile_bounded(&edit.path, &installed_metadata) {
                Ok(current) => current,
                Err(error) => {
                    errors.push(format!("{}: {error}", edit.path.display()));
                    continue;
                }
            };
            if current != edit.updated {
                errors.push(format!(
                    "{} changed after cleanup; refusing to overwrite it",
                    edit.path.display()
                ));
                continue;
            }
            if !edit.installed_identity.as_ref().is_some_and(|identity| {
                fs::symlink_metadata(&edit.path)
                    .map(|metadata| identity.matches(&metadata))
                    .unwrap_or(false)
            }) {
                errors.push(format!(
                    "{} was replaced during rollback; refusing to overwrite it",
                    edit.path.display()
                ));
                continue;
            }
            let expected_identity = edit
                .installed_identity
                .clone()
                .expect("a committed edit has an installed identity");
            let PreparedEdit {
                requested_path,
                path,
                original,
                updated,
                permissions,
                installed_identity,
                committed,
                ..
            } = edit;
            if let Err(error) = atomic_replace(
                AtomicReplacement {
                    requested_path,
                    path,
                    home: &self.home,
                    bytes: original,
                    permissions,
                    expected_identity: &expected_identity,
                    expected_bytes: updated,
                    lock: &self.lock,
                },
                |_| {
                    // As soon as the restoring rename succeeds, this edit is
                    // no longer eligible for another rollback.
                    *committed = false;
                    *installed_identity = None;
                },
            ) {
                errors.push(format!("{}: {error}", edit.path.display()));
            }
        }
        if errors.is_empty() {
            self.report.changed_files.clear();
            Ok(())
        } else {
            Err(invalid_data(errors.join("; ")))
        }
    }
}

impl Drop for LegacyCleanupTransaction {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.rollback_internal();
        }
    }
}

/// Remove one exact legacy block from `content`.
///
/// `Ok(None)` means no legacy marker is present. Duplicate, unmatched,
/// reordered, or look-alike marker lines fail closed.
pub fn remove_marked_block(content: &str) -> io::Result<Option<String>> {
    let locations = marker_locations(content)?;
    match (locations.starts.as_slice(), locations.ends.as_slice()) {
        ([], []) => Ok(None),
        ([start], [end]) if start.body_start < end.body_start => {
            let mut output = String::with_capacity(
                content
                    .len()
                    .saturating_sub(end.full_end.saturating_sub(start.body_start)),
            );
            output.push_str(&content[..start.body_start]);
            output.push_str(&content[end.full_end..]);
            Ok(Some(output))
        }
        ([_], [_]) => Err(invalid_data(
            "legacy managed Codex profile markers are out of order",
        )),
        _ => Err(invalid_data(
            "legacy shell file contains duplicate or unmatched managed Codex markers",
        )),
    }
}

/// Inspect known legacy shell files without modifying them.
pub fn inspect_legacy_shell_integration() -> io::Result<Vec<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cannot determine the current user's home directory",
        )
    })?;
    inspect_legacy_shell_integration_at(&home)
}

pub fn inspect_legacy_shell_integration_at(home: &Path) -> io::Result<Vec<PathBuf>> {
    let home = canonical_home(home)?;
    let lock = LegacyHomeLock::acquire(&home, LockMode::Shared)?;
    let mut found = Vec::new();
    let mut seen = BTreeMap::<PathBuf, ()>::new();
    for candidate in legacy_profile_candidates(&home)? {
        let Some(target) = validated_existing_profile(&candidate, &home)? else {
            continue;
        };
        if seen.insert(target.clone(), ()).is_some() {
            continue;
        }
        let metadata = fs::metadata(&target)?;
        let bytes = read_profile_bounded(&target, &metadata)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            invalid_data(format!(
                "legacy shell file {} is not valid UTF-8",
                candidate.display()
            ))
        })?;
        if remove_marked_block(text)?.is_some() {
            found.push(candidate);
        }
    }
    lock.revalidate()?;
    Ok(found)
}

fn legacy_profile_candidates(home: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = vec![
        home.join(".zshrc"),
        home.join(".config/zsh/.zshrc"),
        home.join(".bashrc"),
        home.join(".bash_profile"),
        home.join(".bash_login"),
        home.join(".profile"),
        home.join(".config/abtop/codex-shell.bash"),
        home.join(".config/fish/config.fish"),
    ];

    let is_current_home = dirs::home_dir()
        .and_then(|current| fs::canonicalize(current).ok())
        .as_deref()
        == Some(home);
    if is_current_home {
        if let Some(zdotdir) = std::env::var_os("ZDOTDIR") {
            paths.push(checked_root(&PathBuf::from(zdotdir), home, "ZDOTDIR")?.join(".zshrc"));
        }
        #[cfg(unix)]
        if let Some(zdotdir) = probe_current_zdotdir(home)? {
            paths.push(zdotdir.join(".zshrc"));
        }
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            paths.push(
                checked_root(&PathBuf::from(xdg), home, "XDG_CONFIG_HOME")?
                    .join("fish/config.fish"),
            );
        }
    }

    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(unix)]
fn probe_current_zdotdir(home: &Path) -> io::Result<Option<PathBuf>> {
    let Some(shell) = std::env::var_os("SHELL").map(PathBuf::from) else {
        return Ok(None);
    };
    probe_zdotdir_with_shell(&shell, home)
}

#[cfg(unix)]
fn probe_zdotdir_with_shell(shell: &Path, home: &Path) -> io::Result<Option<PathBuf>> {
    probe_zdotdir_with_shell_timeout(shell, home, ZDOTDIR_PROBE_TIMEOUT)
}

#[cfg(unix)]
fn probe_zdotdir_with_shell_timeout(
    shell: &Path,
    home: &Path,
    timeout: Duration,
) -> io::Result<Option<PathBuf>> {
    if !shell.is_absolute()
        || !shell
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "zsh" || name.starts_with("zsh-"))
    {
        return Ok(None);
    }
    let shell = normalize_absolute(shell)?;
    let metadata = fs::metadata(&shell)?;
    if !metadata.is_file() {
        return Err(invalid_data("SHELL does not name a regular zsh executable"));
    }
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(invalid_data("SHELL does not name an executable zsh"));
    }

    let login = run_zdotdir_probe_with_timeout(&shell, true, timeout)?;
    let non_login = run_zdotdir_probe_with_timeout(&shell, false, timeout)?;
    if login != non_login {
        return Err(invalid_data(
            "login and non-login zsh resolved different ZDOTDIR values; refusing incomplete legacy cleanup",
        ));
    }
    let Some(root) = login else {
        return Ok(None);
    };
    checked_root(&root, home, "probed ZDOTDIR").map(Some)
}

#[cfg(unix)]
fn run_zdotdir_probe_with_timeout(
    shell: &Path,
    login: bool,
    timeout: Duration,
) -> io::Result<Option<PathBuf>> {
    use std::os::unix::process::CommandExt;

    let command =
        "printf '\\036ABTOP_ZDOTDIR_V1_BEGIN\\037%s\\036ABTOP_ZDOTDIR_V1_END\\037' \"${ZDOTDIR-}\"";
    let mut process = Command::new(shell);
    if login {
        process.arg("-l");
    }
    let mut child = process
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing zsh probe stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing zsh probe stderr"))?;
    if let Err(error) =
        set_probe_pipe_nonblocking(&stdout).and_then(|()| set_probe_pipe_nonblocking(&stderr))
    {
        terminate_probe_group(&mut child);
        return Err(error);
    }
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_overflow = false;
    let mut stderr_overflow = false;
    let deadline = Instant::now() + timeout;
    let status = loop {
        let poll = drain_probe_pipe(&mut stdout, &mut stdout_bytes, &mut stdout_overflow)
            .and_then(|()| drain_probe_pipe(&mut stderr, &mut stderr_bytes, &mut stderr_overflow))
            .and_then(|()| child.try_wait());
        let status = match poll {
            Ok(status) => status,
            Err(error) => {
                terminate_probe_group(&mut child);
                return Err(error);
            }
        };
        if let Some(status) = status {
            // Drain bytes already present in the kernel pipes, then close our
            // descriptors. A background process inherited from shell startup
            // must not keep this bounded probe waiting after zsh has exited.
            drain_probe_pipe(&mut stdout, &mut stdout_bytes, &mut stdout_overflow)?;
            drain_probe_pipe(&mut stderr, &mut stderr_bytes, &mut stderr_overflow)?;
            break status;
        }
        if Instant::now() >= deadline {
            terminate_probe_group(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "zsh ZDOTDIR probe timed out",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    if !status.success() || stdout_overflow || stderr_overflow || !stderr_bytes.is_empty() {
        return Err(invalid_data(
            "zsh ZDOTDIR probe did not produce one bounded silent result",
        ));
    }
    parse_zdotdir_probe(&stdout_bytes)
}

#[cfg(unix)]
fn set_probe_pipe_nonblocking(pipe: &impl std::os::fd::AsRawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn drain_probe_pipe(
    pipe: &mut impl Read,
    output: &mut Vec<u8>,
    overflow: &mut bool,
) -> io::Result<()> {
    let mut buffer = [0_u8; 1024];
    let mut drained = 0_usize;
    while drained < 64 * 1024 {
        let read = match pipe.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if read == 0 {
            return Ok(());
        }
        drained += read;
        let remaining = ZDOTDIR_PROBE_OUTPUT_LIMIT.saturating_sub(output.len());
        let keep = remaining.min(read);
        output.extend_from_slice(&buffer[..keep]);
        *overflow |= keep != read;
    }
    Ok(())
}

#[cfg(unix)]
fn terminate_probe_group(child: &mut std::process::Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    let _ = child.wait();
}

#[cfg(unix)]
fn parse_zdotdir_probe(output: &[u8]) -> io::Result<Option<PathBuf>> {
    use std::os::unix::ffi::OsStringExt;

    let Some(value) = output
        .strip_prefix(ZDOTDIR_PROBE_BEGIN)
        .and_then(|value| value.strip_suffix(ZDOTDIR_PROBE_END))
    else {
        return Err(invalid_data(
            "zsh ZDOTDIR probe output was not exactly framed",
        ));
    };
    if value.is_empty() {
        return Ok(None);
    }
    if value.iter().any(|byte| byte.is_ascii_control()) {
        return Err(invalid_data(
            "zsh ZDOTDIR probe returned a path containing control bytes",
        ));
    }
    Ok(Some(PathBuf::from(std::ffi::OsString::from_vec(
        value.to_vec(),
    ))))
}

fn checked_root(root: &Path, home: &Path, label: &str) -> io::Result<PathBuf> {
    if !root.is_absolute() {
        return Err(invalid_data(format!("{label} must be an absolute path")));
    }
    let normalized = normalize_absolute(root)?;
    if !normalized.starts_with(home) {
        return Err(invalid_data(format!(
            "{label} must remain inside the home directory"
        )));
    }
    Ok(normalized)
}

fn canonical_home(home: &Path) -> io::Result<PathBuf> {
    let home = fs::canonicalize(home)?;
    if !home.is_absolute() {
        return Err(invalid_data("home directory is not absolute"));
    }
    Ok(home)
}

fn validated_existing_profile(requested: &Path, home: &Path) -> io::Result<Option<PathBuf>> {
    match fs::symlink_metadata(requested) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let target = fs::canonicalize(requested)?;
    if !target.starts_with(home) {
        return Err(invalid_data(format!(
            "legacy shell file {} resolves outside the home directory",
            requested.display()
        )));
    }
    let metadata = fs::metadata(&target)?;
    if !metadata.is_file() {
        return Err(invalid_data(format!(
            "legacy shell path {} is not a regular file",
            requested.display()
        )));
    }
    if metadata.len() > MAX_PROFILE_BYTES {
        return Err(invalid_data(format!(
            "legacy shell file {} exceeds 4 MiB",
            requested.display()
        )));
    }
    validate_same_owner(&metadata, requested)?;
    validate_single_link(&metadata, requested)?;
    let parent = target
        .parent()
        .ok_or_else(|| invalid_data("legacy shell file has no parent directory"))?;
    let parent_metadata = fs::metadata(parent)?;
    if !parent_metadata.is_dir() {
        return Err(invalid_data("legacy shell parent is not a directory"));
    }
    validate_same_owner(&parent_metadata, parent)?;
    validate_not_other_writable(&parent_metadata, parent)?;
    Ok(Some(target))
}

fn revalidate_requested_target(requested: &Path, expected: &Path, home: &Path) -> io::Result<()> {
    let current = fs::canonicalize(requested).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot revalidate legacy shell path {}: {error}",
                requested.display()
            ),
        )
    })?;
    if current != expected || !current.starts_with(home) {
        return Err(invalid_data(format!(
            "legacy shell path {} changed its target during cleanup",
            requested.display()
        )));
    }
    Ok(())
}

fn read_profile_bounded(path: &Path, expected: &fs::Metadata) -> io::Result<Vec<u8>> {
    if !expected.is_file() {
        return Err(invalid_data(format!(
            "legacy shell path {} is not a regular file",
            path.display()
        )));
    }
    if expected.len() > MAX_PROFILE_BYTES {
        return Err(invalid_data(format!(
            "legacy shell file {} exceeds 4 MiB",
            path.display()
        )));
    }
    validate_same_owner(expected, path)?;
    validate_single_link(expected, path)?;

    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut file = File::open(path)?;

    let opened = file.metadata()?;
    if !opened.is_file() || !same_file_snapshot(expected, &opened) {
        return Err(invalid_data(format!(
            "legacy shell file {} changed before it could be inspected",
            path.display()
        )));
    }
    validate_same_owner(&opened, path)?;
    validate_single_link(&opened, path)?;

    let mut bytes = Vec::with_capacity(opened.len().min(MAX_PROFILE_BYTES) as usize);
    {
        let mut limited = (&mut file).take(MAX_PROFILE_BYTES + 1);
        limited.read_to_end(&mut bytes)?;
    }
    if bytes.len() as u64 > MAX_PROFILE_BYTES {
        return Err(invalid_data(format!(
            "legacy shell file {} exceeds 4 MiB",
            path.display()
        )));
    }

    let descriptor_after = file.metadata()?;
    let path_after = fs::symlink_metadata(path)?;
    if !same_file_snapshot(&opened, &descriptor_after)
        || !same_file_snapshot(&descriptor_after, &path_after)
        || descriptor_after.len() != bytes.len() as u64
    {
        return Err(invalid_data(format!(
            "legacy shell file {} changed while it was inspected",
            path.display()
        )));
    }
    validate_same_owner(&descriptor_after, path)?;
    validate_single_link(&descriptor_after, path)?;
    Ok(bytes)
}

struct AtomicReplacement<'a> {
    requested_path: &'a Path,
    path: &'a Path,
    home: &'a Path,
    bytes: &'a [u8],
    permissions: &'a fs::Permissions,
    expected_identity: &'a FileIdentity,
    expected_bytes: &'a [u8],
    lock: &'a LegacyHomeLock,
}

fn atomic_replace<F>(replacement: AtomicReplacement<'_>, on_state: F) -> io::Result<()>
where
    F: FnMut(Option<FileIdentity>),
{
    atomic_replace_with_before_exchange(replacement, on_state, || {})
}

fn atomic_replace_with_before_exchange<F, B>(
    replacement: AtomicReplacement<'_>,
    mut on_state: F,
    before_exchange: B,
) -> io::Result<()>
where
    F: FnMut(Option<FileIdentity>),
    B: FnOnce(),
{
    let AtomicReplacement {
        requested_path,
        path,
        home,
        bytes,
        permissions,
        expected_identity,
        expected_bytes,
        lock,
    } = replacement;
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("legacy shell file has no parent directory"))?;
    let canonical_parent = fs::canonicalize(parent)?;
    if canonical_parent != parent || !canonical_parent.starts_with(home) {
        return Err(invalid_data(format!(
            "legacy shell parent {} changed during cleanup",
            parent.display()
        )));
    }
    let parent_metadata = fs::symlink_metadata(parent)?;
    if !parent_metadata.is_dir() {
        return Err(invalid_data("legacy shell parent is not a directory"));
    }
    validate_same_owner(&parent_metadata, parent)?;
    validate_not_other_writable(&parent_metadata, parent)?;
    let parent_identity = FileIdentity::from_metadata(&parent_metadata);
    #[cfg(unix)]
    let parent_handle = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(parent)?
    };
    #[cfg(unix)]
    if !parent_identity.matches(&parent_handle.metadata()?) {
        return Err(invalid_data(format!(
            "legacy shell parent {} changed while it was opened",
            parent.display()
        )));
    }

    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary
        .as_file_mut()
        .set_permissions(permissions.clone())?;
    temporary.write_all(bytes)?;
    temporary.as_file_mut().sync_all()?;

    // Temp-file preparation may take time. Revalidate the requested alias,
    // canonical leaf, parent, identity, and bytes together immediately before
    // the replacement. Any observed concurrent edit fails closed.
    revalidate_requested_target(requested_path, path, home)?;
    let current_parent = fs::symlink_metadata(parent)?;
    if !parent_identity.matches(&current_parent) || !current_parent.is_dir() {
        return Err(invalid_data(format!(
            "legacy shell parent {} was replaced during cleanup",
            parent.display()
        )));
    }
    validate_same_owner(&current_parent, parent)?;
    validate_not_other_writable(&current_parent, parent)?;
    let current_metadata = fs::symlink_metadata(path)?;
    if !current_metadata.is_file() || !expected_identity.matches(&current_metadata) {
        return Err(invalid_data(format!(
            "legacy shell file {} was replaced immediately before cleanup",
            path.display()
        )));
    }
    validate_same_owner(&current_metadata, path)?;
    validate_single_link(&current_metadata, path)?;
    let current_bytes = read_profile_bounded(path, &current_metadata)?;
    if current_bytes != expected_bytes {
        return Err(invalid_data(format!(
            "legacy shell file {} changed immediately before cleanup",
            path.display()
        )));
    }
    revalidate_requested_target(requested_path, path, home)?;
    if !expected_identity.matches(&fs::symlink_metadata(path)?)
        || !parent_identity.matches(&fs::symlink_metadata(parent)?)
    {
        return Err(invalid_data(
            "legacy shell file or parent changed at the replacement boundary",
        ));
    }
    lock.revalidate()?;

    let replacement_metadata = temporary.as_file().metadata()?;
    if !replacement_metadata.is_file()
        || replacement_metadata.len() != bytes.len() as u64
        || !same_file_snapshot(&replacement_metadata, &temporary.as_file().metadata()?)
    {
        return Err(invalid_data(
            "legacy shell temporary replacement changed before installation",
        ));
    }
    validate_same_owner(&replacement_metadata, temporary.path())?;
    validate_single_link(&replacement_metadata, temporary.path())?;
    let replacement_identity = FileIdentity::from_metadata(&replacement_metadata);

    #[cfg(unix)]
    {
        let temporary_name = temporary
            .path()
            .file_name()
            .ok_or_else(|| invalid_data("legacy shell temporary replacement has no file name"))?;
        let target_name = path
            .file_name()
            .ok_or_else(|| invalid_data("legacy shell file has no file name"))?;
        if !relative_regular_file_matches(&parent_handle, temporary_name, &replacement_metadata)? {
            return Err(invalid_data(
                "legacy shell temporary replacement escaped its pinned parent",
            ));
        }
        if !relative_regular_file_matches(&parent_handle, target_name, &current_metadata)? {
            return Err(invalid_data(
                "legacy shell target changed at the handle-relative rename boundary",
            ));
        }

        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let temporary_name = CString::new(temporary_name.as_bytes())
            .map_err(|_| invalid_data("legacy shell temporary file name contains NUL"))?;
        let target_name = CString::new(target_name.as_bytes())
            .map_err(|_| invalid_data("legacy shell file name contains NUL"))?;

        // Cleanup is disabled before the handle-relative exchange so a
        // path-based TempPath drop can never unlink an attacker-created name.
        // The exchange makes the replaced inode available under the temporary
        // name, allowing an exact post-syscall compare-and-swap check.
        temporary.disable_cleanup(true);
        before_exchange();
        if let Err(exchange_error) =
            atomic_exchange_at(&parent_handle, &temporary_name, &target_name)
        {
            let cleanup_failed =
                unsafe { libc::unlinkat(parent_handle.as_raw_fd(), temporary_name.as_ptr(), 0) }
                    != 0;
            if cleanup_failed {
                let cleanup_error = io::Error::last_os_error();
                return Err(io::Error::new(
                    exchange_error.kind(),
                    format!(
                        "{exchange_error}; additionally failed to remove the pinned temporary file: {cleanup_error}"
                    ),
                ));
            }
            return Err(exchange_error);
        }
        // This callback is deliberately the first operation after exchange.
        // It contains no I/O and records which inode rollback now owns.
        on_state(Some(replacement_identity.clone()));

        let swapped_metadata =
            relative_regular_file_metadata_cstr(&parent_handle, &temporary_name)?;
        let swapped_matches = expected_identity.matches(&swapped_metadata)
            && read_profile_bounded(temporary.path(), &swapped_metadata)? == expected_bytes;
        if !swapped_matches {
            let target_metadata =
                relative_regular_file_metadata_cstr(&parent_handle, &target_name)?;
            if !replacement_identity.matches(&target_metadata) {
                return Err(invalid_data(format!(
                    "legacy shell file {} changed after a conflicting atomic exchange; both filesystem entries were preserved",
                    path.display()
                )));
            }
            atomic_exchange_at(&parent_handle, &temporary_name, &target_name)?;
            let restored_temporary =
                relative_regular_file_metadata_cstr(&parent_handle, &temporary_name)?;
            if !replacement_identity.matches(&restored_temporary) {
                return Err(invalid_data(format!(
                    "legacy shell file {} changed while a conflicting edit was restored; both filesystem entries were preserved",
                    path.display()
                )));
            }
            if unsafe { libc::unlinkat(parent_handle.as_raw_fd(), temporary_name.as_ptr(), 0) } != 0
            {
                return Err(io::Error::last_os_error());
            }
            on_state(None);
            return Err(invalid_data(format!(
                "legacy shell file {} was concurrently replaced at the atomic exchange boundary; the concurrent bytes were preserved",
                path.display()
            )));
        }
        if unsafe { libc::unlinkat(parent_handle.as_raw_fd(), temporary_name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    #[cfg(not(unix))]
    {
        before_exchange();
        temporary.persist(path).map_err(|error| error.error)?;
        // This callback is deliberately the first operation after rename.
        // It contains no I/O and records which inode rollback now owns.
        on_state(Some(replacement_identity.clone()));
    }

    let installed = fs::symlink_metadata(path)?;
    if !installed.is_file() || !replacement_identity.matches(&installed) {
        return Err(invalid_data(format!(
            "legacy shell file {} changed immediately after cleanup",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        let relative_installed = relative_regular_file_metadata(
            &parent_handle,
            path.file_name()
                .ok_or_else(|| invalid_data("legacy shell file has no file name"))?,
        )?;
        if !replacement_identity.matches(&relative_installed)
            || relative_installed.len() != bytes.len() as u64
            || !same_file_snapshot(&installed, &relative_installed)
        {
            return Err(invalid_data(
                "legacy shell replacement changed inside its pinned parent",
            ));
        }
        parent_handle.sync_all()?;
    }
    Ok(())
}

#[cfg(unix)]
fn relative_regular_file_matches(
    parent: &File,
    name: &std::ffi::OsStr,
    expected: &fs::Metadata,
) -> io::Result<bool> {
    Ok(same_file_snapshot(
        expected,
        &relative_regular_file_metadata(parent, name)?,
    ))
}

#[cfg(unix)]
fn relative_regular_file_metadata(
    parent: &File,
    name: &std::ffi::OsStr,
) -> io::Result<fs::Metadata> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let name = CString::new(name.as_bytes())
        .map_err(|_| invalid_data("legacy shell relative file name contains NUL"))?;
    relative_regular_file_metadata_cstr(parent, &name)
}

#[cfg(unix)]
fn relative_regular_file_metadata_cstr(
    parent: &File,
    name: &std::ffi::CStr,
) -> io::Result<fs::Metadata> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::MetadataExt;

    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(invalid_data(
            "legacy shell relative entry is not a single-link regular file",
        ));
    }
    Ok(metadata)
}

#[cfg(target_os = "macos")]
fn atomic_exchange_at(
    parent: &File,
    left: &std::ffi::CStr,
    right: &std::ffi::CStr,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    if unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            left.as_ptr(),
            parent.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_SWAP,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn atomic_exchange_at(
    parent: &File,
    left: &std::ffi::CStr,
    right: &std::ffi::CStr,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.as_raw_fd(),
            left.as_ptr(),
            parent.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn atomic_exchange_at(
    _parent: &File,
    _left: &std::ffi::CStr,
    _right: &std::ffi::CStr,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic legacy-profile exchange is supported only on macOS and Linux",
    ))
}

#[derive(Debug, Clone, Copy)]
enum LockMode {
    Shared,
    Exclusive,
}

#[cfg(unix)]
#[derive(Debug)]
struct LegacyHomeLock {
    file: File,
    path: PathBuf,
    identity: FileIdentity,
    home_path: PathBuf,
    home_identity: FileIdentity,
}

#[cfg(unix)]
impl LegacyHomeLock {
    fn acquire(home: &Path, mode: LockMode) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let home_metadata = fs::symlink_metadata(home)?;
        if !home_metadata.is_dir() {
            return Err(invalid_data("legacy migration home is not a directory"));
        }
        validate_same_owner(&home_metadata, home)?;
        validate_not_other_writable(&home_metadata, home)?;
        let home_identity = FileIdentity::from_metadata(&home_metadata);
        let path = home.join(LEGACY_HOME_LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(invalid_data("legacy cleanup lock is not a regular file"));
        }
        validate_same_owner(&metadata, &path)?;
        validate_single_link(&metadata, &path)?;
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(invalid_data(format!(
                "legacy cleanup lock {} must have mode 0600",
                path.display()
            )));
        }
        let operation = match mode {
            LockMode::Shared => libc::LOCK_SH,
            LockMode::Exclusive => libc::LOCK_EX,
        } | libc::LOCK_NB;
        if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("legacy migration is locked by another operation: {error}"),
                ));
            }
            return Err(error);
        }

        let lock = Self {
            identity: FileIdentity::from_metadata(&metadata),
            file,
            path,
            home_path: home.to_path_buf(),
            home_identity,
        };
        lock.revalidate()?;
        Ok(lock)
    }

    fn revalidate(&self) -> io::Result<()> {
        let home_metadata = fs::symlink_metadata(&self.home_path)?;
        if !home_metadata.is_dir()
            || !self.home_identity.matches(&home_metadata)
            || fs::canonicalize(&self.home_path)? != self.home_path
        {
            return Err(invalid_data(
                "legacy migration home changed while its lock was held",
            ));
        }
        validate_same_owner(&home_metadata, &self.home_path)?;
        validate_not_other_writable(&home_metadata, &self.home_path)?;
        let descriptor_metadata = self.file.metadata()?;
        if !descriptor_metadata.is_file()
            || !self.identity.matches(&descriptor_metadata)
            || !self.identity.matches(&fs::symlink_metadata(&self.path)?)
        {
            return Err(invalid_data(format!(
                "legacy cleanup lock {} was replaced",
                self.path.display()
            )));
        }
        validate_same_owner(&descriptor_metadata, &self.path)?;
        validate_single_link(&descriptor_metadata, &self.path)?;
        use std::os::unix::fs::PermissionsExt;
        if descriptor_metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(invalid_data(format!(
                "legacy cleanup lock {} must have mode 0600",
                self.path.display()
            )));
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for LegacyHomeLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(not(unix))]
#[derive(Debug)]
struct LegacyHomeLock;

#[cfg(not(unix))]
impl LegacyHomeLock {
    fn acquire(_home: &Path, _mode: LockMode) -> io::Result<Self> {
        Ok(Self)
    }

    fn revalidate(&self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    length: u64,
    #[cfg(not(unix))]
    modified: Option<SystemTime>,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                length: metadata.len(),
                modified: metadata.modified().ok(),
            }
        }
    }

    fn matches(&self, metadata: &fs::Metadata) -> bool {
        self == &Self::from_metadata(metadata)
    }
}

#[cfg(unix)]
fn validate_same_owner(metadata: &fs::Metadata, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(invalid_data(format!(
            "legacy shell file {} is not owned by the current user",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_single_link(metadata: &fs::Metadata, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() != 1 {
        return Err(invalid_data(format!(
            "legacy shell file {} has multiple hard links",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_single_link(_metadata: &fs::Metadata, _path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn validate_not_other_writable(metadata: &fs::Metadata, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(invalid_data(format!(
            "legacy shell directory {} is writable by another user",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_not_other_writable(_metadata: &fs::Metadata, _path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn validate_same_owner(_metadata: &fs::Metadata, _path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(unix)]
fn same_file_snapshot(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    same_file_identity(left, right)
        && left.len() == right.len()
        && left.mode() == right.mode()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(not(unix))]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

#[cfg(not(unix))]
fn same_file_snapshot(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_file_identity(left, right)
}

fn normalize_absolute(path: &Path) -> io::Result<PathBuf> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err(invalid_data("path must be absolute"));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(invalid_data("path escapes the filesystem root"));
                }
            }
        }
    }
    Ok(normalized)
}

#[derive(Default)]
struct MarkerLocations {
    starts: Vec<LineLocation>,
    ends: Vec<LineLocation>,
}

#[derive(Clone, Copy)]
struct LineLocation {
    body_start: usize,
    full_end: usize,
}

fn marker_locations(content: &str) -> io::Result<MarkerLocations> {
    let mut result = MarkerLocations::default();
    let mut offset = 0;
    while offset < content.len() {
        let relative_end = content[offset..].find('\n');
        let full_end = relative_end
            .map(|position| offset + position + 1)
            .unwrap_or(content.len());
        let mut body_end = relative_end
            .map(|position| offset + position)
            .unwrap_or(content.len());
        if body_end > offset && content.as_bytes()[body_end - 1] == b'\r' {
            body_end -= 1;
        }
        let body = &content[offset..body_end];
        let location = LineLocation {
            body_start: offset,
            full_end,
        };
        if body == LEGACY_START_MARKER {
            result.starts.push(location);
        } else if body == LEGACY_END_MARKER {
            result.ends.push(location);
        } else if body.contains(LEGACY_MARKER_NAMESPACE) {
            return Err(invalid_data(
                "legacy shell file contains a malformed managed Codex marker",
            ));
        }
        offset = full_end;
    }
    Ok(result)
}

fn powershell_guidance() -> Option<String> {
    #[cfg(windows)]
    {
        Some(format!(
            "Open $PROFILE.CurrentUserCurrentHost and remove exactly one block from `{LEGACY_START_MARKER}` through `{LEGACY_END_MARKER}`. Stop without editing if either marker is missing, duplicated, malformed, or out of order. Do not remove any unrelated Codex alias or function."
        ))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_the_exact_marker_block() {
        let input = format!(
            "before\n{LEGACY_START_MARKER}\nfunction codex {{ old; }}\n{LEGACY_END_MARKER}\nafter\n"
        );
        assert_eq!(
            remove_marked_block(&input).unwrap(),
            Some("before\nafter\n".to_string())
        );
    }

    #[test]
    fn no_marker_is_a_successful_noop() {
        assert_eq!(remove_marked_block("alias codex=custom\n").unwrap(), None);
    }

    #[test]
    fn duplicate_unmatched_and_lookalike_markers_fail_closed() {
        let duplicate = format!(
            "{LEGACY_START_MARKER}\n{LEGACY_END_MARKER}\n{LEGACY_START_MARKER}\n{LEGACY_END_MARKER}\n"
        );
        assert!(remove_marked_block(&duplicate).is_err());
        assert!(remove_marked_block(LEGACY_START_MARKER).is_err());
        assert!(remove_marked_block("# >>> abtop managed codex >>> trailing\n").is_err());
    }

    #[test]
    fn preserves_crlf_and_handles_a_final_marker_without_newline() {
        let crlf =
            format!("before\r\n{LEGACY_START_MARKER}\r\nold\r\n{LEGACY_END_MARKER}\r\nafter\r\n");
        assert_eq!(
            remove_marked_block(&crlf).unwrap(),
            Some("before\r\nafter\r\n".to_string())
        );
        let no_final_newline = format!("before\n{LEGACY_START_MARKER}\nold\n{LEGACY_END_MARKER}");
        assert_eq!(
            remove_marked_block(&no_final_newline).unwrap(),
            Some("before\n".to_string())
        );
    }

    #[test]
    fn reversed_markers_fail_closed() {
        let reversed = format!("{LEGACY_END_MARKER}\nold\n{LEGACY_START_MARKER}\n");
        assert!(remove_marked_block(&reversed).is_err());
    }

    #[test]
    fn cleanup_transaction_can_roll_back_exact_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".zshrc");
        let original = format!("keep\r\n{LEGACY_START_MARKER}\r\nold\r\n{LEGACY_END_MARKER}\r\n");
        fs::write(&profile, &original).unwrap();

        let mut transaction = LegacyCleanupTransaction::begin_at(temp.path()).unwrap();
        assert_eq!(fs::read_to_string(&profile).unwrap(), "keep\r\n");
        transaction.rollback().unwrap();
        assert_eq!(fs::read_to_string(&profile).unwrap(), original);
    }

    #[test]
    fn rollback_refuses_to_overwrite_a_later_profile_edit() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".zshrc");
        let original = format!("keep\n{LEGACY_START_MARKER}\nold\n{LEGACY_END_MARKER}\n");
        fs::write(&profile, original).unwrap();

        let mut transaction = LegacyCleanupTransaction::begin_at(temp.path()).unwrap();
        fs::write(&profile, "external edit\n").unwrap();

        assert!(transaction.rollback().is_err());
        assert_eq!(fs::read_to_string(&profile).unwrap(), "external edit\n");
    }

    #[test]
    #[cfg(unix)]
    fn apply_preserves_a_concurrent_atomic_save_at_the_exchange_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(temp.path()).unwrap();
        let profile = home.join("profile");
        fs::write(&profile, b"before").unwrap();
        let metadata = fs::symlink_metadata(&profile).unwrap();
        let identity = FileIdentity::from_metadata(&metadata);
        let permissions = metadata.permissions();
        let lock = LegacyHomeLock::acquire(&home, LockMode::Exclusive).unwrap();
        let mut states = Vec::new();

        let error = atomic_replace_with_before_exchange(
            AtomicReplacement {
                requested_path: &profile,
                path: &profile,
                home: &home,
                bytes: b"installed",
                permissions: &permissions,
                expected_identity: &identity,
                expected_bytes: b"before",
                lock: &lock,
            },
            |state| states.push(state),
            || {
                let saved = home.join("profile.concurrent-save");
                fs::write(&saved, b"concurrent apply edit").unwrap();
                fs::rename(saved, &profile).unwrap();
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error
            .to_string()
            .contains("concurrent bytes were preserved"));
        assert_eq!(states.len(), 2);
        assert!(states[0].is_some());
        assert!(states[1].is_none());
        assert_eq!(fs::read(&profile).unwrap(), b"concurrent apply edit");
        assert!(!home.join("profile.concurrent-save").exists());
    }

    #[test]
    #[cfg(unix)]
    fn rollback_preserves_a_concurrent_atomic_save_at_the_exchange_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(temp.path()).unwrap();
        let profile = home.join("profile");
        fs::write(&profile, b"installed").unwrap();
        let metadata = fs::symlink_metadata(&profile).unwrap();
        let identity = FileIdentity::from_metadata(&metadata);
        let permissions = metadata.permissions();
        let lock = LegacyHomeLock::acquire(&home, LockMode::Exclusive).unwrap();
        let mut states = Vec::new();

        let error = atomic_replace_with_before_exchange(
            AtomicReplacement {
                requested_path: &profile,
                path: &profile,
                home: &home,
                bytes: b"before",
                permissions: &permissions,
                expected_identity: &identity,
                expected_bytes: b"installed",
                lock: &lock,
            },
            |state| states.push(state),
            || {
                let saved = home.join("profile.concurrent-save");
                fs::write(&saved, b"concurrent rollback edit").unwrap();
                fs::rename(saved, &profile).unwrap();
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error
            .to_string()
            .contains("concurrent bytes were preserved"));
        assert_eq!(states.len(), 2);
        assert!(states[0].is_some());
        assert!(states[1].is_none());
        assert_eq!(fs::read(&profile).unwrap(), b"concurrent rollback edit");
        assert!(!home.join("profile.concurrent-save").exists());
    }

    #[test]
    fn oversized_profile_is_rejected_before_unbounded_allocation() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".zshrc");
        let file = File::create(&profile).unwrap();
        file.set_len(MAX_PROFILE_BYTES + 1).unwrap();

        let error = LegacyCleanupTransaction::begin_at(temp.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    #[cfg(unix)]
    fn bounded_profile_reader_refuses_a_symlink_leaf() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let link = temp.path().join("link");
        fs::write(&target, b"unchanged").unwrap();
        symlink("target", &link).unwrap();
        let followed_metadata = fs::metadata(&link).unwrap();

        assert!(read_profile_bounded(&link, &followed_metadata).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"unchanged");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_preserves_an_owned_internal_profile_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("real-zshrc");
        let profile = temp.path().join(".zshrc");
        let original = format!("keep\n{LEGACY_START_MARKER}\nold\n{LEGACY_END_MARKER}\n");
        fs::write(&target, original).unwrap();
        symlink("real-zshrc", &profile).unwrap();

        let transaction = LegacyCleanupTransaction::begin_at(temp.path()).unwrap();
        assert!(fs::symlink_metadata(&profile)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep\n");
        let _ = transaction.commit();
        assert!(fs::symlink_metadata(&profile)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    #[cfg(unix)]
    fn no_marker_transaction_retains_one_stable_home_lock() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join(LEGACY_HOME_LOCK_FILE);

        let transaction = LegacyCleanupTransaction::begin_at(temp.path()).unwrap();
        let first = fs::symlink_metadata(&lock_path).unwrap();
        assert!(first.is_file());
        assert_eq!(first.permissions().mode() & 0o777, 0o600);
        let first_identity = (first.dev(), first.ino());
        let _ = transaction.commit();

        assert!(lock_path.exists());
        let second_transaction = LegacyCleanupTransaction::begin_at(temp.path()).unwrap();
        let second = fs::symlink_metadata(&lock_path).unwrap();
        assert_eq!(first_identity, (second.dev(), second.ino()));
        let _ = second_transaction.commit();
        assert!(lock_path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn exclusive_transaction_blocks_cleanup_before_profile_scanning() {
        let temp = tempfile::tempdir().unwrap();
        let transaction = LegacyCleanupTransaction::begin_at(temp.path()).unwrap();
        fs::write(
            temp.path().join(".zshrc"),
            format!("{LEGACY_START_MARKER}\nmissing end\n"),
        )
        .unwrap();

        let error = LegacyCleanupTransaction::begin_at(temp.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

        let _ = transaction.commit();
    }

    #[test]
    #[cfg(unix)]
    fn inspection_uses_the_same_shared_home_lock() {
        let temp = tempfile::tempdir().unwrap();
        let transaction = LegacyCleanupTransaction::begin_at(temp.path()).unwrap();

        let error = inspect_legacy_shell_integration_at(temp.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

        let _ = transaction.commit();
        assert!(inspect_legacy_shell_integration_at(temp.path())
            .unwrap()
            .is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn unsafe_lock_mode_is_rejected_without_repairing_it() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join(LEGACY_HOME_LOCK_FILE);
        fs::write(&lock_path, b"").unwrap();
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644)).unwrap();

        let error = LegacyCleanupTransaction::begin_at(temp.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::symlink_metadata(&lock_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlink_lock_is_rejected_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("unrelated");
        let lock_path = temp.path().join(LEGACY_HOME_LOCK_FILE);
        fs::write(&target, b"keep").unwrap();
        symlink("unrelated", &lock_path).unwrap();

        assert!(LegacyCleanupTransaction::begin_at(temp.path()).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert!(fs::symlink_metadata(&lock_path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    #[cfg(unix)]
    fn hard_linked_and_nonregular_locks_are_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let linked_home = tempfile::tempdir().unwrap();
        let linked_target = linked_home.path().join("other-lock-name");
        let linked_lock = linked_home.path().join(LEGACY_HOME_LOCK_FILE);
        fs::write(&linked_target, b"").unwrap();
        fs::set_permissions(&linked_target, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&linked_target, &linked_lock).unwrap();
        assert!(LegacyCleanupTransaction::begin_at(linked_home.path()).is_err());

        let directory_home = tempfile::tempdir().unwrap();
        fs::create_dir(directory_home.path().join(LEGACY_HOME_LOCK_FILE)).unwrap();
        assert!(LegacyCleanupTransaction::begin_at(directory_home.path()).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn other_writable_legacy_home_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o777)).unwrap();
        let error = LegacyCleanupTransaction::begin_at(temp.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!temp.path().join(LEGACY_HOME_LOCK_FILE).exists());
    }

    #[test]
    #[cfg(unix)]
    fn atomic_replace_records_the_rename_before_later_validation_failure() {
        let temp = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(temp.path()).unwrap();
        let profile = home.join("profile");
        fs::write(&profile, b"before").unwrap();
        let metadata = fs::symlink_metadata(&profile).unwrap();
        let identity = FileIdentity::from_metadata(&metadata);
        let permissions = metadata.permissions();
        let lock = LegacyHomeLock::acquire(&home, LockMode::Exclusive).unwrap();
        let mut renamed = false;

        let error = atomic_replace(
            AtomicReplacement {
                requested_path: &profile,
                path: &profile,
                home: &home,
                bytes: b"after",
                permissions: &permissions,
                expected_identity: &identity,
                expected_bytes: b"before",
                lock: &lock,
            },
            |_| {
                renamed = true;
                fs::remove_file(&profile).unwrap();
                fs::write(&profile, b"external").unwrap();
            },
        )
        .unwrap_err();

        assert!(renamed);
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&profile).unwrap(), b"external");
    }

    #[test]
    #[cfg(unix)]
    fn zdotdir_probe_accepts_only_one_exact_frame() {
        assert_eq!(
            parse_zdotdir_probe(b"\x1eABTOP_ZDOTDIR_V1_BEGIN\x1f\x1eABTOP_ZDOTDIR_V1_END\x1f")
                .unwrap(),
            None
        );
        assert_eq!(
            parse_zdotdir_probe(
                b"\x1eABTOP_ZDOTDIR_V1_BEGIN\x1f/private/home/.config/zsh\x1eABTOP_ZDOTDIR_V1_END\x1f"
            )
            .unwrap(),
            Some(PathBuf::from("/private/home/.config/zsh"))
        );
        for malformed in [
            b"/private/home/.config/zsh".as_slice(),
            b"prefix\x1eABTOP_ZDOTDIR_V1_BEGIN\x1f/path\x1eABTOP_ZDOTDIR_V1_END\x1f",
            b"\x1eABTOP_ZDOTDIR_V1_BEGIN\x1f/path\nother\x1eABTOP_ZDOTDIR_V1_END\x1f",
        ] {
            assert!(parse_zdotdir_probe(malformed).is_err());
        }
    }

    #[test]
    #[cfg(unix)]
    fn zdotdir_probe_finds_a_value_set_only_by_zsh_startup() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(temp.path()).unwrap();
        let root = home.join("startup-only-zdotdir");
        fs::create_dir(&root).unwrap();
        let shell = home.join("zsh");
        let quoted_root = root.to_string_lossy().replace('\'', "'\"'\"'");
        fs::write(
            &shell,
            format!(
                "#!/bin/sh\nZDOTDIR='{quoted_root}'\nprintf '\\036ABTOP_ZDOTDIR_V1_BEGIN\\037%s\\036ABTOP_ZDOTDIR_V1_END\\037' \"$ZDOTDIR\"\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(
            probe_zdotdir_with_shell_timeout(&shell, &home, Duration::from_secs(15)).unwrap(),
            Some(root)
        );
    }

    #[test]
    #[cfg(unix)]
    fn zdotdir_probe_does_not_wait_for_inherited_background_pipes() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let shell = temp.path().join("zsh");
        let pid_file = temp.path().join("background.pid");
        let quoted_pid_file = pid_file.to_string_lossy().replace('\'', "'\"'\"'");
        fs::write(
            &shell,
            format!(
                "#!/bin/sh\nsleep 30 &\nprintf '%s' \"$!\" > '{quoted_pid_file}'\nprintf '\\036ABTOP_ZDOTDIR_V1_BEGIN\\037%s\\036ABTOP_ZDOTDIR_V1_END\\037' '/private/tmp/zsh'\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();

        let started = Instant::now();
        assert_eq!(
            run_zdotdir_probe_with_timeout(&shell, false, Duration::from_secs(15)).unwrap(),
            Some(PathBuf::from("/private/tmp/zsh"))
        );
        assert!(started.elapsed() < Duration::from_secs(20));

        let background_pid = fs::read_to_string(pid_file)
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let _ = unsafe { libc::kill(background_pid, libc::SIGKILL) };
    }

    #[test]
    #[cfg(unix)]
    fn handle_relative_replacement_leaves_no_temporary_name() {
        let temp = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(temp.path()).unwrap();
        let profile = home.join("profile");
        fs::write(&profile, b"before").unwrap();
        let metadata = fs::symlink_metadata(&profile).unwrap();
        let identity = FileIdentity::from_metadata(&metadata);
        let permissions = metadata.permissions();
        let lock = LegacyHomeLock::acquire(&home, LockMode::Exclusive).unwrap();
        let mut renamed = false;

        atomic_replace(
            AtomicReplacement {
                requested_path: &profile,
                path: &profile,
                home: &home,
                bytes: b"after",
                permissions: &permissions,
                expected_identity: &identity,
                expected_bytes: b"before",
                lock: &lock,
            },
            |_| renamed = true,
        )
        .unwrap();

        assert!(renamed);
        assert_eq!(fs::read(&profile).unwrap(), b"after");
        let mut names = fs::read_dir(&home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            vec![
                std::ffi::OsString::from(LEGACY_HOME_LOCK_FILE),
                std::ffi::OsString::from("profile"),
            ]
        );
    }
}
