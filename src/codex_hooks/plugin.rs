//! Installation and inspection of abtop's local Codex lifecycle-hook plugin.
//!
//! The integration never replaces the `codex` command. It installs an
//! isolated local marketplace through the native Codex plugin CLI and keeps
//! its own source bundle under the active `CODEX_HOME`.

use super::migration::{self, LegacyCleanupTransaction, MigrationReport};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
#[cfg(not(unix))]
use std::sync::mpsc;
#[cfg(not(unix))]
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const PLUGIN_NAME: &str = "abtop";
pub const MARKETPLACE_NAME: &str = "abtop-local";
pub const PLUGIN_ID: &str = "abtop@abtop-local";
pub const HOOK_SCHEMA_REVISION: &str = "1";
pub(crate) const SUPPORTED_CODEX_VERSION: &str = "0.146.0";
pub const INSTALL_ATTESTATION_FILE: &str = "installation.json";
pub(crate) const HOOK_FAULT_TOKEN_ENV: &str = "ABTOP_CODEX_HOOK_FAULT_TOKEN";
pub(crate) const HOOK_STATE_DIR_NAME: &str = "states";
pub(crate) const HOOK_FAULT_DIR_NAME: &str = "faults";
pub const HOOK_EVENTS: [&str; 11] = [
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "SubagentStart",
    "SubagentStop",
    "Stop",
];

const HELPER_IDENTITY_REVISION: &str = "abtop-codex-hook-helper-v1";
const MARKETPLACE_MANIFEST_RELATIVE: &str = ".agents/plugins/marketplace.json";
const PLUGIN_MANIFEST_RELATIVE: &str = "plugins/abtop/.codex-plugin/plugin.json";
const HOOKS_RELATIVE: &str = "plugins/abtop/hooks/hooks.json";
const POSIX_LAUNCHER_RELATIVE: &str = "plugins/abtop/scripts/abtop-codex-hook.sh";
const WINDOWS_LAUNCHER_RELATIVE: &str = "plugins/abtop/scripts/abtop-codex-hook.cmd";
const SETUP_LOCK_FILE: &str = ".abtop-codex-plugin.lock";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const INHERITED_PIPE_GRACE: Duration = Duration::from_millis(100);
const MAX_COMMAND_OUTPUT: usize = 1024 * 1024;
const MAX_MANAGED_FILE: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPaths {
    pub codex_home: PathBuf,
    pub marketplace_root: PathBuf,
    pub marketplace_manifest: PathBuf,
    pub plugin_root: PathBuf,
    pub plugin_manifest: PathBuf,
    pub hooks_manifest: PathBuf,
    pub posix_launcher: PathBuf,
    pub windows_launcher: PathBuf,
    pub plugin_data_root: PathBuf,
    pub install_attestation: PathBuf,
}

impl PluginPaths {
    pub fn new(codex_home: &Path) -> io::Result<Self> {
        let codex_home = normalize_absolute(codex_home)?;
        let marketplace_root = codex_home.join("abtop/marketplace");
        let plugin_root = marketplace_root.join("plugins/abtop");
        let plugin_data_root = codex_home.join("plugins/data/abtop-abtop-local");
        Ok(Self {
            marketplace_manifest: marketplace_root.join(MARKETPLACE_MANIFEST_RELATIVE),
            plugin_manifest: marketplace_root.join(PLUGIN_MANIFEST_RELATIVE),
            hooks_manifest: marketplace_root.join(HOOKS_RELATIVE),
            posix_launcher: marketplace_root.join(POSIX_LAUNCHER_RELATIVE),
            windows_launcher: marketplace_root.join(WINDOWS_LAUNCHER_RELATIVE),
            install_attestation: plugin_data_root.join(INSTALL_ATTESTATION_FILE),
            codex_home,
            marketplace_root,
            plugin_root,
            plugin_data_root,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallationAttestation {
    pub schema_version: u32,
    pub hook_schema_revision: String,
    pub helper_digest: String,
    pub installation_id: String,
    pub plugin_id: String,
    pub plugin_version: String,
    pub hooks_digest: String,
    pub hook_events: Vec<String>,
    pub installed_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupReport {
    pub paths: PluginPaths,
    pub codex_binary: PathBuf,
    pub abtop_binary: PathBuf,
    pub helper_digest: String,
    pub plugin_version: String,
    pub hook_schema_revision: &'static str,
    pub hook_count: usize,
    pub base_config_trusted_hooks: usize,
    pub base_config_enabled_hooks: usize,
    pub review_required: bool,
    pub legacy_cleanup: MigrationReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallReport {
    pub paths: PluginPaths,
    pub codex_binary: PathBuf,
    pub plugin_removed: bool,
    pub marketplace_removed: bool,
    pub source_files_removed: Vec<PathBuf>,
    pub preserved_data_root: PathBuf,
    pub legacy_cleanup: MigrationReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationStatus {
    pub paths: PluginPaths,
    pub codex_binary: Option<PathBuf>,
    pub helper_digest: Option<String>,
    pub hook_schema_revision: &'static str,
    pub hook_count: usize,
    pub marketplace_registered: bool,
    pub plugin_installed: bool,
    pub plugin_enabled: bool,
    pub installed_version: Option<String>,
    pub bundle_valid: bool,
    pub attestation_valid: bool,
    /// Trust state from the base `$CODEX_HOME/config.toml` only. Per-process
    /// CLI overrides and higher-precedence managed layers can differ.
    pub base_config_trusted_hooks: usize,
    /// Enabled state from the base config, where an absent `enabled` field is
    /// Codex's default-enabled state. This does not imply that a hook is trusted.
    pub base_config_enabled_hooks: usize,
    pub base_config_state_entries: usize,
    pub legacy_marker_files: Vec<PathBuf>,
    /// False when the legacy-shell inspection could not be completed safely.
    /// An empty marker list is meaningful only while this flag is true.
    pub legacy_inspection_valid: bool,
    pub healthy: bool,
    pub details: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeHookConfig {
    pub config_digest: String,
    pub complete_hook_set: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderedBundle {
    helper_digest: String,
    plugin_version: String,
    marketplace_manifest: Vec<u8>,
    plugin_manifest: Vec<u8>,
    hooks_manifest: Vec<u8>,
    posix_launcher: Vec<u8>,
    windows_launcher: Vec<u8>,
    hooks_digest: String,
    hook_commands: Vec<HookCommandIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HookCommandIdentity {
    event: &'static str,
    event_key: &'static str,
    command: String,
    command_windows: String,
}

#[derive(Debug)]
struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    overflowed: bool,
}

#[derive(Debug, Default)]
struct CliState {
    marketplace_registered: bool,
    marketplace_conflict: Option<PathBuf>,
    marketplace_malformed: bool,
    plugin_configured: bool,
    plugin_config_malformed: bool,
    plugin_installed: bool,
    plugin_enabled: bool,
    installed_version: Option<String>,
}

#[derive(Debug, Default)]
struct BaseHookState {
    trusted: usize,
    enabled: usize,
    entries: usize,
}

#[derive(Debug)]
struct PreparedInstall {
    codex_home: PathBuf,
    codex_binary: PathBuf,
    codex_binary_digest: String,
    abtop_binary: PathBuf,
    paths: PluginPaths,
    bundle: RenderedBundle,
}

#[cfg(unix)]
#[derive(Debug)]
struct SetupLock {
    file: File,
    codex_home_directory: File,
    lock_metadata: fs::Metadata,
    codex_home: PathBuf,
}

#[cfg(unix)]
impl SetupLock {
    fn acquire(codex_home: &Path) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let codex_home_directory = open_unix_directory(codex_home, false)?;
        let codex_home_metadata = codex_home_directory.metadata()?;
        let lock_path = codex_home.join(SETUP_LOCK_FILE);
        let file = openat_create_unix(
            &codex_home_directory,
            std::ffi::OsStr::new(SETUP_LOCK_FILE),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(invalid_data(format!(
                "unsafe Codex integration setup lock {}",
                lock_path.display()
            )));
        }
        validate_single_link(&metadata, &lock_path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another abtop Codex integration setup is running",
            ));
        }
        let current_home = open_unix_directory(codex_home, false)?;
        if !same_file_metadata(&codex_home_metadata, &current_home.metadata()?) {
            return Err(invalid_data(
                "CODEX_HOME changed while the integration lock was acquired",
            ));
        }
        let current_lock = openat_unix(
            &current_home,
            std::ffi::OsStr::new(SETUP_LOCK_FILE),
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )?
        .ok_or_else(|| invalid_data("Codex integration setup lock disappeared"))?;
        if !same_file_metadata(&metadata, &current_lock.metadata()?) {
            return Err(invalid_data(
                "Codex integration setup lock changed while it was acquired",
            ));
        }
        Ok(Self {
            file,
            codex_home_directory,
            lock_metadata: metadata,
            codex_home: codex_home.to_path_buf(),
        })
    }

    fn revalidate(&self) -> io::Result<()> {
        let current_home = open_unix_directory(&self.codex_home, false)?;
        if !same_file_metadata(
            &self.codex_home_directory.metadata()?,
            &current_home.metadata()?,
        ) {
            return Err(invalid_data(
                "CODEX_HOME changed while the integration lock was held",
            ));
        }
        let lock_path = self.codex_home.join(SETUP_LOCK_FILE);
        let current_lock = openat_unix(
            &current_home,
            std::ffi::OsStr::new(SETUP_LOCK_FILE),
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )?
        .ok_or_else(|| invalid_data("Codex integration setup lock disappeared"))?;
        let metadata = current_lock.metadata()?;
        if !same_file_metadata(&self.lock_metadata, &metadata) {
            return Err(invalid_data(format!(
                "Codex integration setup lock {} was replaced",
                lock_path.display()
            )));
        }
        validate_owned_regular_file(&lock_path, &metadata, true)?;
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(invalid_data(format!(
                "Codex integration setup lock {} must have mode 0600",
                lock_path.display()
            )));
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for SetupLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(not(unix))]
#[derive(Debug)]
struct SetupLock {
    file: File,
    metadata: fs::Metadata,
    path: PathBuf,
}

#[cfg(not(unix))]
impl SetupLock {
    fn acquire(codex_home: &Path) -> io::Result<Self> {
        let lock = codex_home.join(SETUP_LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Permit read-only inspection while denying another writer or a
            // delete/replace of the stable lock for this transaction.
            options.share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ);
        }
        let file = options.open(&lock)?;
        let metadata = file.metadata()?;
        let path_metadata = fs::symlink_metadata(&lock)?;
        if path_metadata.file_type().is_symlink()
            || !metadata.is_file()
            || !same_file_metadata(&metadata, &path_metadata)
        {
            return Err(invalid_data(format!(
                "unsafe Codex integration setup lock {}",
                lock.display()
            )));
        }
        Ok(Self {
            file,
            metadata,
            path: lock,
        })
    }

    fn revalidate(&self) -> io::Result<()> {
        let descriptor = self.file.metadata()?;
        let path = fs::symlink_metadata(&self.path)?;
        if path.file_type().is_symlink()
            || !descriptor.is_file()
            || !same_file_metadata(&self.metadata, &descriptor)
            || !same_file_metadata(&self.metadata, &path)
        {
            return Err(invalid_data(format!(
                "Codex integration setup lock {} changed while it was held",
                self.path.display()
            )));
        }
        Ok(())
    }
}

pub fn setup() -> io::Result<SetupReport> {
    let codex_home = current_codex_home()?;
    let codex_binary = resolve_codex_binary()?;
    let abtop_binary = current_abtop_binary()?;
    setup_with(&codex_home, &codex_binary, &abtop_binary)
}

/// Install using explicit paths. This is public to make setup behavior
/// auditable and testable; all three paths must be absolute.
pub fn setup_with(
    codex_home: &Path,
    codex_binary: &Path,
    abtop_binary: &Path,
) -> io::Result<SetupReport> {
    let home = dirs::home_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cannot determine the current user's home directory",
        )
    })?;
    setup_with_home(codex_home, codex_binary, abtop_binary, &home)
}

pub fn setup_with_home(
    codex_home: &Path,
    codex_binary: &Path,
    abtop_binary: &Path,
    legacy_home: &Path,
) -> io::Result<SetupReport> {
    // Complete every read-only compatibility, helper, source-tree, and base
    // registration check before the legacy transaction temporarily edits a
    // shell profile. Later checks are repeated under the setup lock to close
    // the gap before mutation.
    let prepared = prepare_install(codex_home, codex_binary, abtop_binary)?;
    let mut legacy = LegacyCleanupTransaction::begin_at(legacy_home)?;
    match install_prepared(prepared) {
        Ok(mut report) => {
            report.legacy_cleanup = legacy.commit();
            Ok(report)
        }
        Err(error) => match legacy.rollback() {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(io::Error::new(
                error.kind(),
                format!(
                    "{error}; additionally failed to restore the legacy shell integration: {rollback_error}"
                ),
            )),
        },
    }
}

#[cfg(all(test, unix))]
fn install_after_legacy_cleanup(
    codex_home: &Path,
    codex_binary: &Path,
    abtop_binary: &Path,
) -> io::Result<SetupReport> {
    install_prepared(prepare_install(codex_home, codex_binary, abtop_binary)?)
}

fn prepare_install(
    codex_home: &Path,
    codex_binary: &Path,
    abtop_binary: &Path,
) -> io::Result<PreparedInstall> {
    ensure_hook_state_platform_supported()?;
    let codex_home = prepare_codex_home(codex_home)?;
    let (codex_binary, codex_binary_digest) =
        capture_codex_binary_compatibility(codex_binary, &codex_home)?;
    let abtop_binary = validate_abtop_binary(abtop_binary)?;
    let paths = PluginPaths::new(&codex_home)?;
    let bundle = render_bundle(&abtop_binary, &paths.plugin_data_root)?;
    audit_owned_source_tree(&paths, false)?;
    let prior = inspect_config_state(&paths, None)?;
    validate_setup_registration(&prior)?;
    Ok(PreparedInstall {
        codex_home,
        codex_binary,
        codex_binary_digest,
        abtop_binary,
        paths,
        bundle,
    })
}

fn install_prepared(prepared: PreparedInstall) -> io::Result<SetupReport> {
    let PreparedInstall {
        codex_home,
        codex_binary,
        codex_binary_digest,
        abtop_binary,
        paths,
        bundle,
    } = prepared;
    let _setup_lock = SetupLock::acquire(&paths.codex_home)?;

    let revalidated_codex = validate_codex_binary_compatibility(&codex_binary, &codex_home)?;
    if revalidated_codex != codex_binary
        || executable_path_digest(&revalidated_codex)? != codex_binary_digest
    {
        return Err(invalid_data(
            "the native Codex executable changed after compatibility preflight",
        ));
    }
    let revalidated_bundle = render_bundle(&abtop_binary, &paths.plugin_data_root)?;
    if revalidated_bundle != bundle {
        return Err(invalid_data(
            "the abtop helper changed after integration preflight",
        ));
    }

    _setup_lock.revalidate()?;
    audit_owned_source_tree(&paths, false)?;
    let prior = inspect_config_state(&paths, None)?;
    validate_setup_registration(&prior)?;
    write_bundle(&paths, &bundle)?;
    write_attestation(&paths, &bundle)?;
    if !bundle_matches_disk(&paths, &bundle)? || !attestation_matches(&paths, &bundle)? {
        return Err(invalid_data(
            "the abtop plugin source bundle did not validate after it was written",
        ));
    }

    _setup_lock.revalidate()?;
    let marketplace_added = !prior.marketplace_registered;
    if marketplace_added {
        let marketplace_add = run_mutating_codex(
            &codex_binary,
            &codex_binary_digest,
            &codex_home,
            &[
                OsString::from("plugin"),
                OsString::from("marketplace"),
                OsString::from("add"),
                paths.marketplace_root.as_os_str().to_owned(),
                OsString::from("--json"),
            ],
        )
        .and_then(|output| {
            require_success(&output, "adding the abtop local marketplace")?;
            require_json_object(&output.stdout, "marketplace add")
        });
        if let Err(error) = marketplace_add {
            let cleanup = remove_marketplace_if_owned(
                &paths,
                &codex_binary,
                &codex_binary_digest,
                &codex_home,
            )
            .err()
            .into_iter()
            .collect();
            return Err(with_cleanup_errors(error, cleanup));
        }
    }

    let plugin_add = run_mutating_codex(
        &codex_binary,
        &codex_binary_digest,
        &codex_home,
        &[
            OsString::from("plugin"),
            OsString::from("add"),
            OsString::from(PLUGIN_ID),
            OsString::from("--json"),
        ],
    )
    .and_then(|output| {
        require_success(&output, "installing the abtop plugin")?;
        require_json_object(&output.stdout, "plugin add")
    });
    if let Err(error) = plugin_add {
        let mut cleanup = Vec::new();
        if !prior.plugin_configured {
            if let Err(cleanup_error) =
                remove_plugin_cli(&codex_binary, &codex_binary_digest, &codex_home)
            {
                cleanup.push(cleanup_error);
            }
        }
        if marketplace_added {
            if let Err(cleanup_error) = remove_marketplace_if_owned(
                &paths,
                &codex_binary,
                &codex_binary_digest,
                &codex_home,
            ) {
                cleanup.push(cleanup_error);
            }
        }
        return Err(with_cleanup_errors(error, cleanup));
    }

    let verified = inspect_config_state(&paths, Some(&bundle))?;
    if !verified.marketplace_registered
        || !verified.plugin_installed
        || !verified.plugin_enabled
        || verified.installed_version.as_deref() != Some(bundle.plugin_version.as_str())
    {
        let error = invalid_data(
            "Codex did not install the exact validated abtop plugin payload and enable it after setup",
        );
        let mut cleanup = Vec::new();
        if !prior.plugin_configured {
            if let Err(cleanup_error) =
                remove_plugin_cli(&codex_binary, &codex_binary_digest, &codex_home)
            {
                cleanup.push(cleanup_error);
            }
        }
        if marketplace_added {
            if let Err(cleanup_error) = remove_marketplace_if_owned(
                &paths,
                &codex_binary,
                &codex_binary_digest,
                &codex_home,
            ) {
                cleanup.push(cleanup_error);
            }
        }
        return Err(with_cleanup_errors(error, cleanup));
    }

    let base = inspect_base_hook_state(&paths, &bundle)?;
    Ok(SetupReport {
        paths,
        codex_binary,
        abtop_binary,
        helper_digest: bundle.helper_digest,
        plugin_version: bundle.plugin_version,
        hook_schema_revision: HOOK_SCHEMA_REVISION,
        hook_count: HOOK_EVENTS.len(),
        base_config_trusted_hooks: base.trusted,
        base_config_enabled_hooks: base.enabled,
        review_required: base.trusted != HOOK_EVENTS.len() || base.enabled != HOOK_EVENTS.len(),
        legacy_cleanup: MigrationReport::default(),
    })
}

fn validate_setup_registration(prior: &CliState) -> io::Result<()> {
    if let Some(conflict) = prior.marketplace_conflict.as_ref() {
        return Err(invalid_data(format!(
            "Codex marketplace `{MARKETPLACE_NAME}` is already registered from {}, not abtop's isolated marketplace; remove or rename that conflicting marketplace first",
            conflict.display()
        )));
    }
    if prior.marketplace_malformed || prior.plugin_config_malformed {
        return Err(invalid_data(
            "the existing abtop Codex registration is malformed; run `abtop --uninstall-codex` for guarded recovery, and repair the named abtop entry in CODEX_HOME/config.toml manually if ownership cannot be proved",
        ));
    }
    Ok(())
}

pub fn status() -> io::Result<IntegrationStatus> {
    let codex_home = current_codex_home()?;
    let paths = PluginPaths::new(&normalize_existing_or_lexical(&codex_home)?)?;
    let abtop_binary = current_abtop_binary().ok();
    let bundle = abtop_binary
        .as_ref()
        .and_then(|path| render_bundle(path, &paths.plugin_data_root).ok());
    let (codex_binary, compatibility_error) = match resolve_codex_binary()
        .and_then(|binary| validate_codex_binary_compatibility(&binary, &paths.codex_home))
    {
        Ok(binary) => (Some(binary), None),
        Err(error) => (None, Some(error.to_string())),
    };
    status_with_parts(paths, codex_binary, compatibility_error, bundle)
}

#[allow(dead_code)]
pub fn status_with(
    codex_home: &Path,
    codex_binary: &Path,
    abtop_binary: &Path,
) -> io::Result<IntegrationStatus> {
    let paths = PluginPaths::new(&normalize_existing_or_lexical(codex_home)?)?;
    let (codex_binary, compatibility_error) =
        match validate_codex_binary_compatibility(codex_binary, &paths.codex_home) {
            Ok(binary) => (Some(binary), None),
            Err(error) => (None, Some(error.to_string())),
        };
    let bundle = render_bundle(abtop_binary, &paths.plugin_data_root).ok();
    status_with_parts(paths, codex_binary, compatibility_error, bundle)
}

fn status_with_parts(
    paths: PluginPaths,
    codex_binary: Option<PathBuf>,
    compatibility_error: Option<String>,
    bundle: Option<RenderedBundle>,
) -> io::Result<IntegrationStatus> {
    let mut details = Vec::new();
    let expected_version = bundle.as_ref().map(|bundle| bundle.plugin_version.clone());
    let (legacy_marker_files, legacy_inspection_valid) =
        match migration::inspect_legacy_shell_integration() {
            Ok(files) => (files, true),
            Err(error) => {
                details.push(format!("legacy shell inspection failed: {error}"));
                (Vec::new(), false)
            }
        };
    if !legacy_marker_files.is_empty() {
        details.push("the retired managed-Codex shell marker is still present".to_string());
    }

    let config = match read_base_config(&paths) {
        Ok(config) => Some(config),
        Err(error) => {
            details.push(format!("base Codex config inspection failed: {error}"));
            None
        }
    };

    let (bundle_valid, attestation_valid, helper) = match &bundle {
        Some(bundle) => {
            let bundle_valid = bundle_matches_disk(&paths, bundle).unwrap_or_else(|error| {
                details.push(format!("plugin bundle validation failed: {error}"));
                false
            });
            let attestation_valid = attestation_matches(&paths, bundle).unwrap_or_else(|error| {
                details.push(format!("plugin attestation validation failed: {error}"));
                false
            });
            (
                bundle_valid,
                attestation_valid,
                Some(bundle.helper_digest.clone()),
            )
        }
        None => {
            details.push("the current abtop helper identity could not be validated".to_string());
            (false, false, None)
        }
    };

    let base = match (config.as_ref(), bundle.as_ref()) {
        (Some(config), Some(bundle)) => inspect_base_hook_state_from_config(config, bundle),
        _ => BaseHookState::default(),
    };
    let base_runtime_config_safe = config.as_ref().is_some_and(base_runtime_hook_config_safe);
    if config.as_ref().is_some_and(base_config_lock_selected) {
        details.push(
            "the base Codex config selects a config lock, so complete runtime hook coverage cannot be established"
                .to_string(),
        );
    }
    if config
        .as_ref()
        .is_some_and(|config| !base_hook_features_enabled(config))
    {
        details.push("the base Codex config disables a required hook/plugin feature".to_string());
    }
    let cli = match config.as_ref() {
        Some(config) => inspect_config_state_from_config(&paths, bundle.as_ref(), config)
            .unwrap_or_else(|error| {
                details.push(format!(
                    "Codex plugin installation inspection failed: {error}"
                ));
                CliState::default()
            }),
        None => CliState::default(),
    };
    if let Some(error) = compatibility_error {
        details.push(format!(
            "native Codex compatibility preflight failed: {error}"
        ));
    }
    if cli.marketplace_conflict.is_some() {
        details.push("the abtop-local marketplace name points at another source".to_string());
    } else if cli.marketplace_malformed {
        details.push("the abtop-local marketplace registration is malformed".to_string());
    } else if !cli.marketplace_registered {
        details.push("the abtop-local marketplace is not registered".to_string());
    }
    if cli.plugin_config_malformed {
        details.push("the abtop plugin registration is malformed".to_string());
    }
    if !cli.plugin_installed {
        details.push("the abtop plugin is not installed".to_string());
    } else if cli.installed_version.as_ref() != expected_version.as_ref() {
        details.push(format!(
            "installed plugin version {} does not match the current helper version {}",
            cli.installed_version.as_deref().unwrap_or("<unknown>"),
            expected_version.as_deref().unwrap_or("<unavailable>")
        ));
    }
    if base.trusted != HOOK_EVENTS.len() {
        details.push(format!(
            "base config trusts {}/{} abtop hooks; a new native Codex session must review the remainder",
            base.trusted,
            HOOK_EVENTS.len()
        ));
    }
    if base.enabled != HOOK_EVENTS.len() {
        details.push(format!(
            "base config enables {}/{} abtop hooks",
            base.enabled,
            HOOK_EVENTS.len()
        ));
    }

    let healthy = cli.marketplace_registered
        && cli.plugin_installed
        && cli.plugin_enabled
        && cli.installed_version.as_ref() == expected_version.as_ref()
        && bundle_valid
        && attestation_valid
        && base_runtime_config_safe
        && base.trusted == HOOK_EVENTS.len()
        && base.enabled == HOOK_EVENTS.len()
        && legacy_marker_files.is_empty()
        && legacy_inspection_valid
        && codex_binary.is_some();
    Ok(IntegrationStatus {
        paths,
        codex_binary,
        helper_digest: helper,
        hook_schema_revision: HOOK_SCHEMA_REVISION,
        hook_count: HOOK_EVENTS.len(),
        marketplace_registered: cli.marketplace_registered,
        plugin_installed: cli.plugin_installed,
        plugin_enabled: cli.plugin_enabled,
        installed_version: cli.installed_version,
        bundle_valid,
        attestation_valid,
        base_config_trusted_hooks: base.trusted,
        base_config_enabled_hooks: base.enabled,
        base_config_state_entries: base.entries,
        legacy_marker_files,
        legacy_inspection_valid,
        healthy,
        details,
    })
}

pub fn uninstall() -> io::Result<UninstallReport> {
    let codex_home = current_codex_home()?;
    let codex_binary = resolve_codex_binary()?;
    uninstall_with(&codex_home, &codex_binary)
}

pub fn uninstall_with(codex_home: &Path, codex_binary: &Path) -> io::Result<UninstallReport> {
    let mut legacy = LegacyCleanupTransaction::begin()?;
    let result = uninstall_after_legacy_cleanup(codex_home, codex_binary);
    match result {
        Ok(mut report) => {
            report.legacy_cleanup = legacy.commit();
            Ok(report)
        }
        Err(error) => match legacy.rollback() {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(io::Error::new(
                error.kind(),
                format!(
                    "{error}; additionally failed to restore the legacy shell integration: {rollback_error}"
                ),
            )),
        },
    }
}

fn uninstall_after_legacy_cleanup(
    codex_home: &Path,
    codex_binary: &Path,
) -> io::Result<UninstallReport> {
    let codex_home = normalize_existing_or_lexical(codex_home)?;
    let paths = PluginPaths::new(&codex_home)?;
    let setup_lock = SetupLock::acquire(&paths.codex_home)?;
    // Uninstall deliberately accepts future native Codex versions: the
    // idempotent remove command is the recovery path when an upgrade makes the
    // currently supported hook contract unavailable.
    let (codex_binary, codex_binary_digest) =
        capture_codex_binary_identity(codex_binary, &codex_home)?;
    setup_lock.revalidate()?;

    // Codex documents plugin removal as idempotent, and it remains usable when
    // the marketplace source or snapshot is already gone. Always issue it so a
    // partially broken prior installation can recover without a global list.
    remove_plugin_cli(&codex_binary, &codex_binary_digest, &codex_home)?;
    ensure_plugin_absent(&paths)?;
    let plugin_removed = true;

    // Source safety must never prevent the unconditional reserved-plugin
    // removal above. It gates only marketplace/source cleanup.
    audit_owned_source_tree(&paths, false)?;
    let marketplace_removed =
        remove_marketplace_if_owned(&paths, &codex_binary, &codex_binary_digest, &codex_home)?;
    // Re-audit after invoking Codex and immediately before path-based leaf
    // deletion. Any substituted ancestor, symlink, or unexpected capability
    // fails closed and is preserved for manual inspection.
    ensure_plugin_absent(&paths)?;
    ensure_marketplace_absent(&paths)?;
    setup_lock.revalidate()?;
    audit_owned_source_tree(&paths, false)?;
    let source_files_removed = remove_owned_bundle_files(&paths)?;
    ensure_plugin_absent(&paths)?;
    ensure_marketplace_absent(&paths)?;
    setup_lock.revalidate()?;
    Ok(UninstallReport {
        preserved_data_root: paths.plugin_data_root.clone(),
        paths,
        codex_binary,
        plugin_removed,
        marketplace_removed,
        source_files_removed,
        legacy_cleanup: MigrationReport::default(),
    })
}

/// Return a digest that binds the hook schema, helper interface revision,
/// exact abtop executable path, and executable contents.
pub fn helper_digest(abtop_binary: &Path) -> io::Result<String> {
    let binary = validate_abtop_binary(abtop_binary)?;
    let path = binary.to_str().ok_or_else(|| {
        invalid_data("the abtop executable path must be valid UTF-8 for hook installation")
    })?;
    validate_embedded_text(path, "abtop executable path")?;
    let executable_digest = hash_file(&binary)?;
    let mut hasher = Sha256::new();
    hasher.update(HELPER_IDENTITY_REVISION.as_bytes());
    hasher.update([0]);
    hasher.update(HOOK_SCHEMA_REVISION.as_bytes());
    hasher.update([0]);
    hasher.update(path.as_bytes());
    hasher.update([0]);
    hasher.update(executable_digest.as_bytes());
    Ok(format!("sha256:{}", hex(&hasher.finalize())))
}

/// Read the content-free installation identity without invoking Codex or
/// hashing the helper executable. Runtime ingestion and collection use this
/// as the stable generation boundary; full helper/bundle verification remains
/// the responsibility of [`status`] and setup.
pub fn read_installation_attestation(
    codex_home: &Path,
) -> io::Result<Option<InstallationAttestation>> {
    let paths = PluginPaths::new(&normalize_existing_or_lexical(codex_home)?)?;
    let Some(bytes) = read_installation_attestation_bytes(&paths)? else {
        return Ok(None);
    };
    let attestation: InstallationAttestation = serde_json::from_slice(&bytes)
        .map_err(|error| invalid_data(format!("invalid installation attestation: {error}")))?;
    if valid_attestation_shape(&attestation) {
        Ok(Some(attestation))
    } else {
        Err(invalid_data(
            "installation attestation has an invalid identity shape",
        ))
    }
}

#[allow(dead_code)]
pub fn read_current_installation_attestation() -> io::Result<Option<InstallationAttestation>> {
    read_installation_attestation(&current_codex_home()?)
}

/// Validate the static, base-config portion of runtime hook identity without
/// invoking Codex or mutating its configuration.
///
/// In Codex 0.146.0, individual hook state is merged only from the base/selected
/// User layer and SessionFlags. This function validates the unprofiled base
/// layer only; process/runtime correlation must separately reject profiles,
/// relevant session flags, config locks, and any lifecycle without fresh hook
/// evidence. It is not a proof of the complete effective config stack.
pub(crate) fn runtime_hook_config(
    codex_home: &Path,
    abtop_binary: &Path,
) -> io::Result<RuntimeHookConfig> {
    ensure_hook_state_platform_supported()?;
    let paths = PluginPaths::new(&normalize_existing_or_lexical(codex_home)?)?;
    let bundle = render_bundle(abtop_binary, &paths.plugin_data_root)?;
    let (config_bytes, config) = read_base_config_snapshot(&paths)?;
    let mut identity = Sha256::new();
    identity.update(&config_bytes);
    identity.update([0]);
    identity.update(bundle.hooks_digest.as_bytes());
    identity.update([0]);
    identity.update(bundle.plugin_version.as_bytes());
    let base = inspect_base_hook_state_from_config(&config, &bundle);
    Ok(RuntimeHookConfig {
        config_digest: format!("sha256:{}", hex(&identity.finalize())),
        complete_hook_set: bundle_matches_disk(&paths, &bundle)?
            && cached_bundle_matches_disk(&paths, &bundle)?
            && attestation_matches(&paths, &bundle)?
            && base_runtime_hook_config_safe(&config)
            && base.trusted == HOOK_EVENTS.len()
            && base.enabled == HOOK_EVENTS.len(),
    })
}

fn base_runtime_hook_config_safe(config: &toml::Value) -> bool {
    base_plugin_enabled(config)
        && base_hook_features_enabled(config)
        && !base_config_lock_selected(config)
}

fn base_plugin_enabled(config: &toml::Value) -> bool {
    config
        .get("plugins")
        .and_then(toml::Value::as_table)
        .and_then(|plugins| plugins.get(PLUGIN_ID))
        .and_then(toml::Value::as_table)
        .is_some_and(|plugin| plugin.get("enabled").and_then(toml::Value::as_bool) != Some(false))
}

fn base_hook_features_enabled(config: &toml::Value) -> bool {
    let Some(features) = config.get("features") else {
        return true;
    };
    let Some(features) = features.as_table() else {
        return false;
    };
    ["hooks", "codex_hooks", "plugins"]
        .iter()
        .all(|name| features.get(*name).and_then(toml::Value::as_bool) != Some(false))
}

fn base_config_lock_selected(config: &toml::Value) -> bool {
    config
        .get("debug")
        .and_then(|debug| debug.get("config_lockfile"))
        .and_then(|lock| lock.get("load_path"))
        .is_some()
}

fn valid_attestation_shape(attestation: &InstallationAttestation) -> bool {
    attestation.schema_version == 1
        && attestation.hook_schema_revision == HOOK_SCHEMA_REVISION
        && valid_sha256_digest(&attestation.helper_digest)
        && attestation.installation_id.len() == 32
        && attestation
            .installation_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        && attestation.plugin_id == PLUGIN_ID
        && !attestation.plugin_version.is_empty()
        && valid_sha256_digest(&attestation.hooks_digest)
        && attestation.hook_events
            == HOOK_EVENTS
                .iter()
                .map(|event| (*event).to_string())
                .collect::<Vec<_>>()
}

fn valid_sha256_digest(value: &str) -> bool {
    value.len() == "sha256:".len() + 64
        && value.starts_with("sha256:")
        && value["sha256:".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
}

fn render_bundle(abtop_binary: &Path, plugin_data_root: &Path) -> io::Result<RenderedBundle> {
    let abtop_binary = validate_abtop_binary(abtop_binary)?;
    let plugin_data_root = normalize_absolute(plugin_data_root)?;
    let helper_digest = helper_digest(&abtop_binary)?;
    let suffix = helper_digest
        .strip_prefix("sha256:")
        .unwrap_or(&helper_digest)
        .chars()
        .take(12)
        .collect::<String>();
    let plugin_version = format!("{}+codex.{suffix}", env!("CARGO_PKG_VERSION"));
    let command = format!(
        "exec \"$PLUGIN_ROOT/scripts/abtop-codex-hook.sh\" --schema-revision {HOOK_SCHEMA_REVISION} --helper-digest {helper_digest}"
    );
    let command_windows = format!(
        "cmd.exe /D /C call \"%PLUGIN_ROOT%\\scripts\\abtop-codex-hook.cmd\" --schema-revision {HOOK_SCHEMA_REVISION} --helper-digest {helper_digest}"
    );
    let hook_commands = HOOK_EVENTS
        .iter()
        .map(|event| HookCommandIdentity {
            event,
            event_key: hook_event_key(event),
            command: command.clone(),
            command_windows: command_windows.clone(),
        })
        .collect::<Vec<_>>();

    let mut hooks = serde_json::Map::new();
    for identity in &hook_commands {
        hooks.insert(
            identity.event.to_string(),
            json!([{
                "hooks": [{
                    "type": "command",
                    "command": identity.command,
                    "commandWindows": identity.command_windows,
                    "timeout": 1
                }]
            }]),
        );
    }
    let hooks_manifest = pretty_json(&json!({
        "description": "Content-free lifecycle signals for the local abtop agent monitor.",
        "hooks": hooks
    }))?;
    let hooks_digest = hash_bytes(&hooks_manifest);

    let marketplace_manifest = pretty_json(&json!({
        "name": MARKETPLACE_NAME,
        "interface": { "displayName": "abtop Local" },
        "plugins": [{
            "name": PLUGIN_NAME,
            "source": { "source": "local", "path": "./plugins/abtop" },
            "policy": { "installation": "AVAILABLE", "authentication": "ON_INSTALL" },
            "category": "Productivity"
        }]
    }))?;
    let plugin_manifest = pretty_json(&json!({
        "name": PLUGIN_NAME,
        "version": plugin_version,
        "description": "Reports content-free Codex lifecycle events to the local abtop monitor.",
        "author": { "name": "abtop contributors" },
        "homepage": "https://github.com/graykode/abtop",
        "repository": "https://github.com/graykode/abtop",
        "license": "MIT",
        "keywords": ["monitoring", "codex", "terminal"],
        "interface": {
            "displayName": "abtop",
            "shortDescription": "Local lifecycle signals for the abtop monitor.",
            "longDescription": "Reports content-free Codex lifecycle events to the local abtop terminal monitor.",
            "developerName": "abtop contributors",
            "category": "Productivity",
            "capabilities": [],
            "defaultPrompt": ["Show the current abtop monitoring status."]
        }
    }))?;

    let binary = abtop_binary.to_str().ok_or_else(|| {
        invalid_data("the abtop executable path must be valid UTF-8 for hook installation")
    })?;
    let launcher_nonce = helper_digest
        .strip_prefix("sha256:")
        .unwrap_or(&helper_digest)
        .chars()
        .take(16)
        .collect::<String>();
    let fault_directory = plugin_data_root
        .join(HOOK_STATE_DIR_NAME)
        .join(HOOK_FAULT_DIR_NAME);
    let fault_directory_text = fault_directory.to_str().ok_or_else(|| {
        invalid_data("the Codex hook fault directory must be valid UTF-8 for hook installation")
    })?;
    validate_embedded_text(fault_directory_text, "Codex hook fault directory")?;
    let quoted_fault_directory = quote_posix(fault_directory_text);
    let quoted_binary = quote_posix(binary);
    let posix_launcher = format!(
        "#!/bin/sh\n# Generated by abtop. Do not add output or provider content.\n[ \"${{1-}}\" = '--schema-revision' ] || exit 0\n[ \"${{2-}}\" = '{HOOK_SCHEMA_REVISION}' ] || exit 0\n[ \"${{3-}}\" = '--helper-digest' ] || exit 0\n[ \"${{4-}}\" = '{helper_digest}' ] || exit 0\nunset {HOOK_FAULT_TOKEN_ENV}\nabtop_fault_dir={quoted_fault_directory}\nabtop_fault_path=\nif abtop_fault_path=$(umask 077; mktemp \"$abtop_fault_dir/launch-$$-pending.XXXXXXXXXXXXXXXX\" 2>/dev/null); then\n  abtop_fault_token=${{abtop_fault_path##*/}}\n  abtop_fault_nonce=${{abtop_fault_token#launch-$$-pending.}}\n  case \"$abtop_fault_path\" in\n    \"$abtop_fault_dir/launch-$$-pending.\"*)\n      case \"$abtop_fault_nonce\" in\n        ''|*[!A-Za-z0-9]*) ;;\n        *)\n          {HOOK_FAULT_TOKEN_ENV}=$abtop_fault_token\n          export {HOOK_FAULT_TOKEN_ENV}\n          ;;\n      esac\n      ;;\n  esac\nfi\nif [ -z \"${{{HOOK_FAULT_TOKEN_ENV}-}}\" ]; then\n  abtop_fault_slot=0\n  while [ \"$abtop_fault_slot\" -lt 16 ]; do\n    abtop_fault_token=launch-$abtop_fault_slot-abtopv1.pending\n    if (umask 077; set -C; : > \"$abtop_fault_dir/$abtop_fault_token\") 2>/dev/null; then\n      {HOOK_FAULT_TOKEN_ENV}=$abtop_fault_token\n      export {HOOK_FAULT_TOKEN_ENV}\n      break\n    fi\n    abtop_fault_slot=$((abtop_fault_slot + 1))\n  done\nfi\nif [ -z \"${{{HOOK_FAULT_TOKEN_ENV}-}}\" ]; then\n  (umask 077; set -C; : > \"$abtop_fault_dir/overflow.json\") 2>/dev/null || :\nfi\n{quoted_binary} --codex-hook-ingest --schema-revision '{HOOK_SCHEMA_REVISION}' --helper-digest '{helper_digest}' >/dev/null 2>&1 || :\nexit 0\n"
    )
    .into_bytes();
    let windows_binary = quote_cmd_path(binary)?;
    let windows_fault_directory = escape_cmd_set_value(fault_directory_text)?;
    let windows_launcher = format!(
        "@echo off\r\nrem Generated by abtop. Do not add output or provider content.\r\nsetlocal EnableExtensions DisableDelayedExpansion\r\nif not \"%~1\"==\"--schema-revision\" exit /b 0\r\nif not \"%~2\"==\"{HOOK_SCHEMA_REVISION}\" exit /b 0\r\nif not \"%~3\"==\"--helper-digest\" exit /b 0\r\nif not \"%~4\"==\"{helper_digest}\" exit /b 0\r\nset \"{HOOK_FAULT_TOKEN_ENV}=\"\r\nset \"abtop_fault_dir={windows_fault_directory}\"\r\ncall :abtop_create_fault_marker\r\n{windows_binary} --codex-hook-ingest --schema-revision {HOOK_SCHEMA_REVISION} --helper-digest {helper_digest} >nul 2>nul\r\nexit /b 0\r\n\r\n:abtop_create_fault_marker\r\nif not exist \"%abtop_fault_dir%\\.\" exit /b 0\r\nset /a abtop_fault_attempt=0 >nul 2>nul\r\n:abtop_create_fault_marker_retry\r\nset \"abtop_fault_token=launch-%RANDOM%-{launcher_nonce}%RANDOM%%RANDOM%.pending\"\r\n\"%SystemRoot%\\System32\\fsutil.exe\" file createnew \"%abtop_fault_dir%\\%abtop_fault_token%\" 0 >nul 2>nul\r\nif errorlevel 1 goto abtop_create_fault_marker_failed\r\nset \"{HOOK_FAULT_TOKEN_ENV}=%abtop_fault_token%\"\r\nexit /b 0\r\n:abtop_create_fault_marker_failed\r\nset /a abtop_fault_attempt+=1 >nul 2>nul\r\nif %abtop_fault_attempt% LSS 16 goto abtop_create_fault_marker_retry\r\nexit /b 0\r\n"
    )
    .into_bytes();

    Ok(RenderedBundle {
        helper_digest,
        plugin_version,
        marketplace_manifest,
        plugin_manifest,
        hooks_manifest,
        posix_launcher,
        windows_launcher,
        hooks_digest,
        hook_commands,
    })
}

fn write_bundle(paths: &PluginPaths, bundle: &RenderedBundle) -> io::Result<()> {
    #[cfg(unix)]
    {
        write_bundle_unix(paths, bundle)
    }
    #[cfg(not(unix))]
    {
        write_bundle_portable(paths, bundle)
    }
}

#[cfg(unix)]
fn write_bundle_unix(paths: &PluginPaths, bundle: &RenderedBundle) -> io::Result<()> {
    let home = open_unix_directory(&paths.codex_home, false)?;
    let source_root_path = paths.codex_home.join("abtop");
    let source_root =
        ensure_managed_directory_at(&home, "abtop", &source_root_path, true, Some(0o700))?;
    let marketplace_path = source_root_path.join("marketplace");
    let marketplace = ensure_managed_directory_at(
        &source_root,
        "marketplace",
        &marketplace_path,
        true,
        Some(0o700),
    )?;
    let agents_path = marketplace_path.join(".agents");
    let agents =
        ensure_managed_directory_at(&marketplace, ".agents", &agents_path, true, Some(0o700))?;
    let agents_plugins_path = agents_path.join("plugins");
    let agents_plugins =
        ensure_managed_directory_at(&agents, "plugins", &agents_plugins_path, true, Some(0o700))?;
    let source_plugins_path = marketplace_path.join("plugins");
    let source_plugins = ensure_managed_directory_at(
        &marketplace,
        "plugins",
        &source_plugins_path,
        true,
        Some(0o700),
    )?;
    let plugin_root_path = source_plugins_path.join(PLUGIN_NAME);
    let plugin_root = ensure_managed_directory_at(
        &source_plugins,
        PLUGIN_NAME,
        &plugin_root_path,
        true,
        Some(0o700),
    )?;
    let manifest_path = plugin_root_path.join(".codex-plugin");
    let manifest_directory = ensure_managed_directory_at(
        &plugin_root,
        ".codex-plugin",
        &manifest_path,
        true,
        Some(0o700),
    )?;
    let hooks_path = plugin_root_path.join("hooks");
    let hooks_directory =
        ensure_managed_directory_at(&plugin_root, "hooks", &hooks_path, true, Some(0o700))?;
    let scripts_path = plugin_root_path.join("scripts");
    let scripts_directory =
        ensure_managed_directory_at(&plugin_root, "scripts", &scripts_path, true, Some(0o700))?;

    let plugins_path = paths.codex_home.join("plugins");
    let plugins = ensure_managed_directory_at(&home, "plugins", &plugins_path, false, None)?;
    let data_path = plugins_path.join("data");
    let data = ensure_managed_directory_at(&plugins, "data", &data_path, false, None)?;
    let data_root = ensure_managed_directory_at(
        &data,
        "abtop-abtop-local",
        &paths.plugin_data_root,
        true,
        Some(0o700),
    )?;
    let states_path = paths.plugin_data_root.join(HOOK_STATE_DIR_NAME);
    let states = ensure_managed_directory_at(
        &data_root,
        HOOK_STATE_DIR_NAME,
        &states_path,
        true,
        Some(0o700),
    )?;
    let faults_path = states_path.join(HOOK_FAULT_DIR_NAME);
    ensure_managed_directory_at(
        &states,
        HOOK_FAULT_DIR_NAME,
        &faults_path,
        true,
        Some(0o700),
    )?;

    atomic_write_private_at(
        &agents_plugins,
        "marketplace.json",
        &paths.marketplace_manifest,
        &bundle.marketplace_manifest,
        false,
    )?;
    atomic_write_private_at(
        &manifest_directory,
        "plugin.json",
        &paths.plugin_manifest,
        &bundle.plugin_manifest,
        false,
    )?;
    atomic_write_private_at(
        &hooks_directory,
        "hooks.json",
        &paths.hooks_manifest,
        &bundle.hooks_manifest,
        false,
    )?;
    atomic_write_private_at(
        &scripts_directory,
        "abtop-codex-hook.sh",
        &paths.posix_launcher,
        &bundle.posix_launcher,
        true,
    )?;
    atomic_write_private_at(
        &scripts_directory,
        "abtop-codex-hook.cmd",
        &paths.windows_launcher,
        &bundle.windows_launcher,
        false,
    )?;
    if !bundle_matches_disk_unix(paths, bundle)? {
        return Err(invalid_data(
            "the live managed plugin source tree did not match the payload after writing",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn write_bundle_portable(paths: &PluginPaths, bundle: &RenderedBundle) -> io::Result<()> {
    // Keep this explicit rather than using create_dir_all: every component is
    // checked for ownership and symlink substitution before the next one is
    // created.
    for directory in [
        paths.codex_home.join("abtop"),
        paths.marketplace_root.clone(),
        paths.marketplace_root.join(".agents"),
        paths.marketplace_manifest.parent().unwrap().to_path_buf(),
        paths.marketplace_root.join("plugins"),
        paths.plugin_root.clone(),
        paths.plugin_manifest.parent().unwrap().to_path_buf(),
        paths.hooks_manifest.parent().unwrap().to_path_buf(),
        paths.posix_launcher.parent().unwrap().to_path_buf(),
    ] {
        ensure_private_dir(&directory)?;
    }
    ensure_private_data_ancestry(paths)?;
    ensure_private_dir(&paths.plugin_data_root)?;
    ensure_private_dir(&paths.plugin_data_root.join(HOOK_STATE_DIR_NAME))?;
    ensure_private_dir(
        &paths
            .plugin_data_root
            .join(HOOK_STATE_DIR_NAME)
            .join(HOOK_FAULT_DIR_NAME),
    )?;

    atomic_write_private(
        &paths.marketplace_manifest,
        &bundle.marketplace_manifest,
        false,
    )?;
    atomic_write_private(&paths.plugin_manifest, &bundle.plugin_manifest, false)?;
    atomic_write_private(&paths.hooks_manifest, &bundle.hooks_manifest, false)?;
    atomic_write_private(&paths.posix_launcher, &bundle.posix_launcher, true)?;
    atomic_write_private(&paths.windows_launcher, &bundle.windows_launcher, false)?;
    Ok(())
}

fn write_attestation(paths: &PluginPaths, bundle: &RenderedBundle) -> io::Result<()> {
    #[cfg(unix)]
    {
        write_attestation_unix(paths, bundle)
    }
    #[cfg(not(unix))]
    {
        let prior = read_private_regular_file(&paths.install_attestation)?;
        let bytes = render_attestation_bytes(prior, bundle)?;
        atomic_write_private(&paths.install_attestation, &bytes, false)
    }
}

fn render_attestation_bytes(
    prior: Option<Vec<u8>>,
    bundle: &RenderedBundle,
) -> io::Result<Vec<u8>> {
    let prior = prior
        .and_then(|bytes| serde_json::from_slice::<InstallationAttestation>(&bytes).ok())
        .filter(|attestation| attestation_identity_matches(attestation, bundle));
    let (installation_id, installed_at_unix_ms) = match prior {
        Some(attestation) if !attestation.installation_id.is_empty() => (
            attestation.installation_id,
            attestation.installed_at_unix_ms,
        ),
        _ => {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(io::Error::other)?;
            let installed_at_unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            (hex(&random), installed_at_unix_ms)
        }
    };
    let attestation = InstallationAttestation {
        schema_version: 1,
        hook_schema_revision: HOOK_SCHEMA_REVISION.to_string(),
        helper_digest: bundle.helper_digest.clone(),
        installation_id,
        plugin_id: PLUGIN_ID.to_string(),
        plugin_version: bundle.plugin_version.clone(),
        hooks_digest: bundle.hooks_digest.clone(),
        hook_events: HOOK_EVENTS
            .iter()
            .map(|event| (*event).to_string())
            .collect(),
        installed_at_unix_ms,
    };
    pretty_json(&serde_json::to_value(attestation).map_err(io::Error::other)?)
}

#[cfg(unix)]
fn write_attestation_unix(paths: &PluginPaths, bundle: &RenderedBundle) -> io::Result<()> {
    let home = open_unix_directory(&paths.codex_home, false)?;
    let home_metadata = home.metadata()?;
    let plugins_path = paths.codex_home.join("plugins");
    let plugins = ensure_managed_directory_at(&home, "plugins", &plugins_path, false, None)?;
    let data_path = plugins_path.join("data");
    let data = ensure_managed_directory_at(&plugins, "data", &data_path, false, None)?;
    let data_root = ensure_managed_directory_at(
        &data,
        "abtop-abtop-local",
        &paths.plugin_data_root,
        true,
        Some(0o700),
    )?;
    let prior = read_private_regular_file_at(
        &data_root,
        INSTALL_ATTESTATION_FILE,
        &paths.install_attestation,
    )?;
    let bytes = render_attestation_bytes(prior, bundle)?;
    atomic_write_private_at(
        &data_root,
        INSTALL_ATTESTATION_FILE,
        &paths.install_attestation,
        &bytes,
        false,
    )?;
    let pinned_attestation = open_matching_managed_file_at(
        &data_root,
        INSTALL_ATTESTATION_FILE,
        &paths.install_attestation,
        &bytes,
        false,
        true,
    )?
    .ok_or_else(|| invalid_data("installation attestation disappeared after writing"))?;

    let rebound_home = open_unix_directory(&paths.codex_home, false)?;
    if !same_file_metadata(&home_metadata, &rebound_home.metadata()?) {
        return Err(invalid_data(
            "CODEX_HOME changed while the installation attestation was written",
        ));
    }
    let rebound_plugins =
        reopen_same_directory_at(&rebound_home, "plugins", &plugins, &plugins_path, false)?;
    let rebound_data =
        reopen_same_directory_at(&rebound_plugins, "data", &data, &data_path, false)?;
    let rebound_data_root = reopen_same_directory_at(
        &rebound_data,
        "abtop-abtop-local",
        &data_root,
        &paths.plugin_data_root,
        true,
    )?;
    reopen_same_file_at(
        &rebound_data_root,
        INSTALL_ATTESTATION_FILE,
        &pinned_attestation,
        &paths.install_attestation,
        &bytes,
        false,
        true,
    )?;
    Ok(())
}

fn inspect_config_state(
    paths: &PluginPaths,
    bundle: Option<&RenderedBundle>,
) -> io::Result<CliState> {
    let config = read_base_config(paths)?;
    inspect_config_state_from_config(paths, bundle, &config)
}

fn inspect_config_state_from_config(
    paths: &PluginPaths,
    bundle: Option<&RenderedBundle>,
    config: &toml::Value,
) -> io::Result<CliState> {
    let mut state = CliState::default();

    if let Some(registration) = config
        .get("marketplaces")
        .and_then(|marketplaces| marketplaces.get(MARKETPLACE_NAME))
    {
        match registration.as_table() {
            Some(table) => {
                let source_type = table.get("source_type").and_then(toml::Value::as_str);
                let source = table.get("source").and_then(toml::Value::as_str);
                match (source_type, source) {
                    (Some("local"), Some(source)) => {
                        let source_path = PathBuf::from(source);
                        if !source_path.is_absolute() {
                            state.marketplace_malformed = true;
                        } else if paths_equal(&source_path, &paths.marketplace_root) {
                            state.marketplace_registered = true;
                        } else {
                            state.marketplace_conflict = Some(source_path);
                        }
                    }
                    (Some(source_type), Some(source)) if source_type != "local" => {
                        state.marketplace_conflict =
                            Some(PathBuf::from(format!("<{source_type} source: {source}>")));
                    }
                    _ => state.marketplace_malformed = true,
                }
            }
            None => state.marketplace_malformed = true,
        }
    }

    if let Some(plugin) = config
        .get("plugins")
        .and_then(|plugins| plugins.get(PLUGIN_ID))
    {
        state.plugin_configured = true;
        match plugin.as_table() {
            Some(table) => match table.get("enabled") {
                Some(value) => match value.as_bool() {
                    Some(enabled) => state.plugin_enabled = enabled,
                    None => state.plugin_config_malformed = true,
                },
                None => state.plugin_enabled = true,
            },
            None => state.plugin_config_malformed = true,
        }
    }

    if let Some(bundle) = bundle {
        let cache_valid = cached_bundle_matches_disk(paths, bundle)?;
        if cache_version_path(paths, &bundle.plugin_version).exists() {
            state.installed_version = Some(bundle.plugin_version.clone());
        }
        state.plugin_installed = state.plugin_configured && cache_valid;
    }
    Ok(state)
}

fn read_base_config(paths: &PluginPaths) -> io::Result<toml::Value> {
    read_base_config_snapshot(paths).map(|(_bytes, config)| config)
}

fn read_base_config_snapshot(paths: &PluginPaths) -> io::Result<(Vec<u8>, toml::Value)> {
    let config_path = paths.codex_home.join("config.toml");
    let metadata = match fs::symlink_metadata(&config_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((Vec::new(), toml::Value::Table(Default::default())));
        }
        Err(error) => return Err(error),
    };
    validate_owned_regular_file(&config_path, &metadata, false)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&config_path)?;
    let opened = file.metadata()?;
    if !same_file_content_snapshot(&metadata, &opened) {
        return Err(invalid_data(format!(
            "{} changed while it was opened",
            config_path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take(MAX_MANAGED_FILE + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANAGED_FILE {
        return Err(invalid_data(format!(
            "{} exceeds the 4 MiB inspection limit",
            config_path.display()
        )));
    }
    let descriptor_after = file.metadata()?;
    let after = fs::symlink_metadata(&config_path)?;
    if bytes.len() as u64 != metadata.len()
        || !same_file_content_snapshot(&metadata, &descriptor_after)
        || !same_file_content_snapshot(&metadata, &after)
    {
        return Err(invalid_data(format!(
            "{} changed while it was inspected",
            config_path.display()
        )));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| invalid_data(format!("{} is not valid UTF-8", config_path.display())))?;
    let config = if text.trim().is_empty() {
        toml::Value::Table(Default::default())
    } else {
        toml::from_str(text).map_err(|error| {
            invalid_data(format!(
                "{} contains invalid TOML: {error}",
                config_path.display()
            ))
        })?
    };
    Ok((bytes, config))
}

fn inspect_base_hook_state(
    paths: &PluginPaths,
    bundle: &RenderedBundle,
) -> io::Result<BaseHookState> {
    let config = read_base_config(paths)?;
    Ok(inspect_base_hook_state_from_config(&config, bundle))
}

fn inspect_base_hook_state_from_config(
    config: &toml::Value,
    bundle: &RenderedBundle,
) -> BaseHookState {
    let states = config
        .get("hooks")
        .and_then(|hooks| hooks.get("state"))
        .and_then(toml::Value::as_table);
    let mut result = BaseHookState::default();
    for identity in &bundle.hook_commands {
        let key = format!("{PLUGIN_ID}:hooks/hooks.json:{}:0:0", identity.event_key);
        let state = states.and_then(|states| states.get(&key));
        if state.is_some() {
            result.entries += 1;
        }
        let enabled = state
            .and_then(|state| state.get("enabled"))
            .and_then(toml::Value::as_bool)
            != Some(false);
        if enabled {
            result.enabled += 1;
        }
        let trusted = state
            .and_then(|state| state.get("trusted_hash"))
            .and_then(toml::Value::as_str);
        if trusted == Some(expected_trust_hash(identity).as_str()) {
            result.trusted += 1;
        }
    }
    result
}

fn expected_trust_hash(identity: &HookCommandIdentity) -> String {
    // Mirrors Codex 0.146.0's normalized command-hook identity: no matcher,
    // one synchronous command handler, an explicit one-second timeout, and
    // only the command selected for the current platform. Discovery clears
    // `commandWindows` before hashing the normalized handler.
    #[cfg(windows)]
    let command = &identity.command_windows;
    #[cfg(not(windows))]
    let command = &identity.command;
    let value = json!({
        "event_name": identity.event_key,
        "hooks": [{
            "async": false,
            "command": command,
            "timeout": 1,
            "type": "command"
        }]
    });
    let canonical = canonical_json(&value);
    hash_bytes(&serde_json::to_vec(&canonical).unwrap_or_default())
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonical_json(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        _ => value.clone(),
    }
}

fn audit_owned_source_tree(paths: &PluginPaths, require_complete: bool) -> io::Result<bool> {
    let root = paths.codex_home.join("abtop");
    let expected_directories = [
        "marketplace",
        "marketplace/.agents",
        "marketplace/.agents/plugins",
        "marketplace/plugins",
        "marketplace/plugins/abtop",
        "marketplace/plugins/abtop/.codex-plugin",
        "marketplace/plugins/abtop/hooks",
        "marketplace/plugins/abtop/scripts",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect::<BTreeSet<_>>();
    let expected_files = [
        "marketplace/.agents/plugins/marketplace.json",
        "marketplace/plugins/abtop/.codex-plugin/plugin.json",
        "marketplace/plugins/abtop/hooks/hooks.json",
        "marketplace/plugins/abtop/scripts/abtop-codex-hook.sh",
        "marketplace/plugins/abtop/scripts/abtop-codex-hook.cmd",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect::<BTreeSet<_>>();

    let root_metadata = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(!require_complete),
        Err(error) => return Err(error),
    };
    validate_owned_directory(&root, &root_metadata, true)?;

    let mut found_directories = BTreeSet::new();
    let mut found_files = BTreeSet::new();
    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(&root).map_err(|_| {
                invalid_data(format!(
                    "managed plugin path {} escaped its source root",
                    path.display()
                ))
            })?;
            let metadata = fs::symlink_metadata(&path)?;
            if relative == Path::new(".setup.lock") {
                validate_owned_regular_file(&path, &metadata, true)?;
                continue;
            }
            if expected_directories.contains(relative) {
                validate_owned_directory(&path, &metadata, true)?;
                found_directories.insert(relative.to_path_buf());
                pending.push(path);
            } else if expected_files.contains(relative) {
                validate_owned_regular_file(&path, &metadata, true)?;
                found_files.insert(relative.to_path_buf());
            } else {
                return Err(invalid_data(format!(
                    "unexpected file or capability in the managed plugin source tree: {}",
                    path.display()
                )));
            }
        }
    }

    Ok(!require_complete
        || (found_directories == expected_directories && found_files == expected_files))
}

fn cache_version_path(paths: &PluginPaths, version: &str) -> PathBuf {
    paths
        .codex_home
        .join("plugins/cache")
        .join(MARKETPLACE_NAME)
        .join(PLUGIN_NAME)
        .join(version)
}

fn cached_bundle_matches_disk(paths: &PluginPaths, bundle: &RenderedBundle) -> io::Result<bool> {
    #[cfg(unix)]
    {
        cached_bundle_matches_disk_unix(paths, bundle)
    }
    #[cfg(not(unix))]
    {
        cached_bundle_matches_disk_portable(paths, bundle)
    }
}

#[cfg(not(unix))]
fn cached_bundle_matches_disk_portable(
    paths: &PluginPaths,
    bundle: &RenderedBundle,
) -> io::Result<bool> {
    let cache_plugin_root = paths
        .codex_home
        .join("plugins/cache")
        .join(MARKETPLACE_NAME)
        .join(PLUGIN_NAME);
    for directory in [
        paths.codex_home.join("plugins"),
        paths.codex_home.join("plugins/cache"),
        paths
            .codex_home
            .join("plugins/cache")
            .join(MARKETPLACE_NAME),
        cache_plugin_root.clone(),
    ] {
        let metadata = match fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        validate_owned_directory(&directory, &metadata, false)?;
    }

    let version_root = cache_version_path(paths, &bundle.plugin_version);
    let entries = fs::read_dir(&cache_plugin_root)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    if entries.len() != 1 || entries.first() != Some(&version_root) {
        if entries.is_empty() {
            return Ok(false);
        }
        return Err(invalid_data(format!(
            "the cached `{PLUGIN_ID}` payload does not contain exactly the current version"
        )));
    }

    let expected_directories = [".codex-plugin", "hooks", "scripts"]
        .into_iter()
        .map(PathBuf::from)
        .collect::<BTreeSet<_>>();
    let expected_files = [
        ".codex-plugin/plugin.json",
        "hooks/hooks.json",
        "scripts/abtop-codex-hook.sh",
        "scripts/abtop-codex-hook.cmd",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect::<BTreeSet<_>>();
    let root_metadata = fs::symlink_metadata(&version_root)?;
    validate_owned_directory(&version_root, &root_metadata, false)?;
    let mut found_directories = BTreeSet::new();
    let mut found_files = BTreeSet::new();
    let mut pending = vec![version_root.clone()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(&version_root).map_err(|_| {
                invalid_data(format!(
                    "cached plugin path {} escaped its root",
                    path.display()
                ))
            })?;
            let metadata = fs::symlink_metadata(&path)?;
            if expected_directories.contains(relative) {
                validate_owned_directory(&path, &metadata, false)?;
                found_directories.insert(relative.to_path_buf());
                pending.push(path);
            } else if expected_files.contains(relative) {
                validate_owned_regular_file(&path, &metadata, false)?;
                found_files.insert(relative.to_path_buf());
            } else {
                return Err(invalid_data(format!(
                    "unexpected file or capability in the cached plugin payload: {}",
                    path.display()
                )));
            }
        }
    }
    if found_directories != expected_directories || found_files != expected_files {
        return Ok(false);
    }

    for (path, bytes, executable) in [
        (
            version_root.join(".codex-plugin/plugin.json"),
            bundle.plugin_manifest.as_slice(),
            false,
        ),
        (
            version_root.join("hooks/hooks.json"),
            bundle.hooks_manifest.as_slice(),
            false,
        ),
        (
            version_root.join("scripts/abtop-codex-hook.sh"),
            bundle.posix_launcher.as_slice(),
            true,
        ),
        (
            version_root.join("scripts/abtop-codex-hook.cmd"),
            bundle.windows_launcher.as_slice(),
            false,
        ),
    ] {
        if !private_regular_file_matches(&path, bytes, executable)? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(unix)]
fn cached_bundle_matches_disk_unix(
    paths: &PluginPaths,
    bundle: &RenderedBundle,
) -> io::Result<bool> {
    let home = open_unix_directory(&paths.codex_home, false)?;
    let home_metadata = home.metadata()?;
    let plugins_path = paths.codex_home.join("plugins");
    let Some(plugins) = open_managed_directory_at(&home, "plugins", &plugins_path, false)? else {
        return Ok(false);
    };
    let cache_path = plugins_path.join("cache");
    let Some(cache) = open_managed_directory_at(&plugins, "cache", &cache_path, false)? else {
        return Ok(false);
    };
    let marketplace_path = cache_path.join(MARKETPLACE_NAME);
    let Some(marketplace) =
        open_managed_directory_at(&cache, MARKETPLACE_NAME, &marketplace_path, false)?
    else {
        return Ok(false);
    };
    let plugin_path = marketplace_path.join(PLUGIN_NAME);
    let Some(plugin) = open_managed_directory_at(&marketplace, PLUGIN_NAME, &plugin_path, false)?
    else {
        return Ok(false);
    };

    let expected_versions = BTreeSet::from([OsString::from(&bundle.plugin_version)]);
    if !unix_directory_has_exact_names(
        &plugin,
        &expected_versions,
        &plugin_path,
        "cached plugin versions",
    )? {
        return Ok(false);
    }
    let version_path = plugin_path.join(&bundle.plugin_version);
    let Some(version) =
        open_managed_directory_at(&plugin, &bundle.plugin_version, &version_path, false)?
    else {
        return Ok(false);
    };
    let version_names = BTreeSet::from([
        OsString::from(".codex-plugin"),
        OsString::from("hooks"),
        OsString::from("scripts"),
    ]);
    if !unix_directory_has_exact_names(
        &version,
        &version_names,
        &version_path,
        "cached plugin root",
    )? {
        return Ok(false);
    }

    let manifest_path = version_path.join(".codex-plugin");
    let hooks_path = version_path.join("hooks");
    let scripts_path = version_path.join("scripts");
    let Some(manifest_directory) =
        open_managed_directory_at(&version, ".codex-plugin", &manifest_path, false)?
    else {
        return Ok(false);
    };
    let Some(hooks_directory) = open_managed_directory_at(&version, "hooks", &hooks_path, false)?
    else {
        return Ok(false);
    };
    let Some(scripts_directory) =
        open_managed_directory_at(&version, "scripts", &scripts_path, false)?
    else {
        return Ok(false);
    };
    if !unix_directory_has_exact_names(
        &manifest_directory,
        &BTreeSet::from([OsString::from("plugin.json")]),
        &manifest_path,
        "cached plugin manifest directory",
    )? || !unix_directory_has_exact_names(
        &hooks_directory,
        &BTreeSet::from([OsString::from("hooks.json")]),
        &hooks_path,
        "cached hooks directory",
    )? || !unix_directory_has_exact_names(
        &scripts_directory,
        &BTreeSet::from([
            OsString::from("abtop-codex-hook.cmd"),
            OsString::from("abtop-codex-hook.sh"),
        ]),
        &scripts_path,
        "cached scripts directory",
    )? {
        return Ok(false);
    }

    let plugin_manifest_path = manifest_path.join("plugin.json");
    let hooks_file_path = hooks_path.join("hooks.json");
    let posix_path = scripts_path.join("abtop-codex-hook.sh");
    let windows_path = scripts_path.join("abtop-codex-hook.cmd");
    let Some(plugin_manifest) = open_matching_managed_file_at(
        &manifest_directory,
        "plugin.json",
        &plugin_manifest_path,
        &bundle.plugin_manifest,
        false,
        false,
    )?
    else {
        return Ok(false);
    };
    let Some(hooks_file) = open_matching_managed_file_at(
        &hooks_directory,
        "hooks.json",
        &hooks_file_path,
        &bundle.hooks_manifest,
        false,
        false,
    )?
    else {
        return Ok(false);
    };
    let Some(posix_launcher) = open_matching_managed_file_at(
        &scripts_directory,
        "abtop-codex-hook.sh",
        &posix_path,
        &bundle.posix_launcher,
        true,
        false,
    )?
    else {
        return Ok(false);
    };
    let Some(windows_launcher) = open_matching_managed_file_at(
        &scripts_directory,
        "abtop-codex-hook.cmd",
        &windows_path,
        &bundle.windows_launcher,
        false,
        false,
    )?
    else {
        return Ok(false);
    };

    // Rebuild the complete path from the current CODEX_HOME namespace. A
    // pinned descriptor can continue to describe a detached old tree after a
    // concurrent rename, so scanning only that descriptor is insufficient.
    let rebound_home = open_unix_directory(&paths.codex_home, false)?;
    if !same_file_metadata(&home_metadata, &rebound_home.metadata()?) {
        return Err(invalid_data("CODEX_HOME changed during cache validation"));
    }
    let rebound_plugins =
        reopen_same_directory_at(&rebound_home, "plugins", &plugins, &plugins_path, false)?;
    let rebound_cache =
        reopen_same_directory_at(&rebound_plugins, "cache", &cache, &cache_path, false)?;
    let rebound_marketplace = reopen_same_directory_at(
        &rebound_cache,
        MARKETPLACE_NAME,
        &marketplace,
        &marketplace_path,
        false,
    )?;
    let rebound_plugin = reopen_same_directory_at(
        &rebound_marketplace,
        PLUGIN_NAME,
        &plugin,
        &plugin_path,
        false,
    )?;
    let rebound_version = reopen_same_directory_at(
        &rebound_plugin,
        &bundle.plugin_version,
        &version,
        &version_path,
        false,
    )?;
    let rebound_manifest = reopen_same_directory_at(
        &rebound_version,
        ".codex-plugin",
        &manifest_directory,
        &manifest_path,
        false,
    )?;
    let rebound_hooks = reopen_same_directory_at(
        &rebound_version,
        "hooks",
        &hooks_directory,
        &hooks_path,
        false,
    )?;
    let rebound_scripts = reopen_same_directory_at(
        &rebound_version,
        "scripts",
        &scripts_directory,
        &scripts_path,
        false,
    )?;
    reopen_same_file_at(
        &rebound_manifest,
        "plugin.json",
        &plugin_manifest,
        &plugin_manifest_path,
        &bundle.plugin_manifest,
        false,
        false,
    )?;
    reopen_same_file_at(
        &rebound_hooks,
        "hooks.json",
        &hooks_file,
        &hooks_file_path,
        &bundle.hooks_manifest,
        false,
        false,
    )?;
    reopen_same_file_at(
        &rebound_scripts,
        "abtop-codex-hook.sh",
        &posix_launcher,
        &posix_path,
        &bundle.posix_launcher,
        true,
        false,
    )?;
    reopen_same_file_at(
        &rebound_scripts,
        "abtop-codex-hook.cmd",
        &windows_launcher,
        &windows_path,
        &bundle.windows_launcher,
        false,
        false,
    )?;
    Ok(unix_directory_has_exact_names(
        &rebound_plugin,
        &expected_versions,
        &plugin_path,
        "cached plugin versions",
    )? && unix_directory_has_exact_names(
        &rebound_version,
        &version_names,
        &version_path,
        "cached plugin root",
    )?)
}

#[cfg(unix)]
fn open_managed_directory_at(
    parent: &File,
    name: &str,
    path: &Path,
    private: bool,
) -> io::Result<Option<File>> {
    let Some(directory) = openat_unix(
        parent,
        std::ffi::OsStr::new(name),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )?
    else {
        return Ok(None);
    };
    validate_owned_directory(path, &directory.metadata()?, private)?;
    Ok(Some(directory))
}

#[cfg(unix)]
fn reopen_same_directory_at(
    parent: &File,
    name: &str,
    pinned: &File,
    path: &Path,
    private: bool,
) -> io::Result<File> {
    let current = open_managed_directory_at(parent, name, path, private)?.ok_or_else(|| {
        invalid_data(format!(
            "managed plugin directory {} disappeared during closing validation",
            path.display()
        ))
    })?;
    if !same_file_metadata(&pinned.metadata()?, &current.metadata()?) {
        return Err(invalid_data(format!(
            "managed plugin directory {} was replaced during validation",
            path.display()
        )));
    }
    Ok(current)
}

#[cfg(unix)]
fn unix_directory_has_exact_names(
    directory: &File,
    expected: &BTreeSet<OsString>,
    path: &Path,
    label: &str,
) -> io::Result<bool> {
    let actual = unix_directory_names(directory)?;
    if &actual == expected {
        return Ok(true);
    }
    if actual.is_subset(expected) {
        return Ok(false);
    }
    Err(invalid_data(format!(
        "unexpected file or capability in {label} {}",
        path.display()
    )))
}

#[cfg(unix)]
fn unix_directory_has_required_and_optional_names(
    directory: &File,
    required: &BTreeSet<OsString>,
    optional: &BTreeSet<OsString>,
    path: &Path,
    label: &str,
) -> io::Result<bool> {
    let actual = unix_directory_names(directory)?;
    let allowed = required.union(optional).cloned().collect::<BTreeSet<_>>();
    if !actual.is_subset(&allowed) {
        return Err(invalid_data(format!(
            "unexpected file or capability in {label} {}",
            path.display()
        )));
    }
    Ok(required.is_subset(&actual))
}

#[cfg(unix)]
fn unix_directory_names(directory: &File) -> io::Result<BTreeSet<OsString>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;

    // A duplicated directory descriptor shares its seek position with the
    // original open file description. Opening `.` relative to the pinned
    // directory gives each scan an independent cursor, so a closing
    // verification cannot silently observe EOF from the first scan.
    let dot = std::ffi::CString::new(".").expect("static path has no NUL");
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(descriptor) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        let _ = unsafe { libc::close(descriptor) };
        return Err(error);
    }
    let mut names = BTreeSet::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if names.len() >= 64 {
            let _ = unsafe { libc::closedir(stream) };
            return Err(invalid_data("cached plugin directory has too many entries"));
        }
        names.insert(OsString::from_vec(bytes.to_vec()));
    }
    if unsafe { libc::closedir(stream) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(names)
}

#[cfg(unix)]
fn open_matching_managed_file_at(
    parent: &File,
    name: &str,
    path: &Path,
    expected: &[u8],
    executable: bool,
    private: bool,
) -> io::Result<Option<File>> {
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    let Some(mut file) = openat_unix(parent, std::ffi::OsStr::new(name), flags)? else {
        return Ok(None);
    };
    let metadata = file.metadata()?;
    validate_owned_regular_file(path, &metadata, private)?;
    use std::os::unix::fs::PermissionsExt;
    let expected_mode = if executable { 0o700 } else { 0o600 };
    if metadata.permissions().mode() & 0o777 != expected_mode {
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take(MAX_MANAGED_FILE + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANAGED_FILE
        || bytes.len() as u64 != metadata.len()
        || !same_file_content_snapshot(&metadata, &file.metadata()?)
    {
        return Err(invalid_data(format!(
            "cached plugin file {} changed while it was read",
            path.display()
        )));
    }
    let current = openat_unix(parent, std::ffi::OsStr::new(name), flags)?.ok_or_else(|| {
        invalid_data(format!(
            "cached plugin file {} disappeared after it was read",
            path.display()
        ))
    })?;
    if !same_file_content_snapshot(&metadata, &current.metadata()?) {
        return Err(invalid_data(format!(
            "cached plugin file {} was replaced while it was read",
            path.display()
        )));
    }
    if bytes != expected {
        return Ok(None);
    }
    Ok(Some(current))
}

#[cfg(unix)]
fn reopen_same_file_at(
    parent: &File,
    name: &str,
    pinned: &File,
    path: &Path,
    expected: &[u8],
    executable: bool,
    private: bool,
) -> io::Result<File> {
    let current = open_matching_managed_file_at(parent, name, path, expected, executable, private)?
        .ok_or_else(|| {
            invalid_data(format!(
                "managed plugin file {} disappeared or changed during closing validation",
                path.display()
            ))
        })?;
    if !same_file_content_snapshot(&pinned.metadata()?, &current.metadata()?) {
        return Err(invalid_data(format!(
            "managed plugin file {} was replaced during validation",
            path.display()
        )));
    }
    Ok(current)
}

#[cfg(unix)]
fn read_private_regular_file_at(
    parent: &File,
    name: &str,
    path: &Path,
) -> io::Result<Option<Vec<u8>>> {
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    let Some(mut file) = openat_unix(parent, std::ffi::OsStr::new(name), flags)? else {
        return Ok(None);
    };
    let metadata = file.metadata()?;
    validate_owned_regular_file(path, &metadata, true)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take(MAX_MANAGED_FILE + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANAGED_FILE
        || bytes.len() as u64 != metadata.len()
        || !same_file_content_snapshot(&metadata, &file.metadata()?)
    {
        return Err(invalid_data(format!(
            "managed plugin file {} changed while it was read",
            path.display()
        )));
    }
    let current = openat_unix(parent, std::ffi::OsStr::new(name), flags)?.ok_or_else(|| {
        invalid_data(format!(
            "managed plugin file {} disappeared after it was read",
            path.display()
        ))
    })?;
    if !same_file_content_snapshot(&metadata, &current.metadata()?) {
        return Err(invalid_data(format!(
            "managed plugin file {} was replaced while it was read",
            path.display()
        )));
    }
    Ok(Some(bytes))
}

fn validate_owned_directory(path: &Path, metadata: &fs::Metadata, private: bool) -> io::Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_data(format!(
            "managed plugin path {} is not a safe directory",
            path.display()
        )));
    }
    validate_same_owner(metadata, path)?;
    if private {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(invalid_data(format!(
                    "managed plugin directory {} is accessible by another user",
                    path.display()
                )));
            }
        }
    } else {
        validate_not_other_writable(metadata, path)?;
    }
    Ok(())
}

fn validate_owned_regular_file(
    path: &Path,
    metadata: &fs::Metadata,
    private: bool,
) -> io::Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid_data(format!(
            "managed plugin path {} is not a safe regular file",
            path.display()
        )));
    }
    if metadata.len() > MAX_MANAGED_FILE {
        return Err(invalid_data(format!(
            "managed plugin file {} is oversized",
            path.display()
        )));
    }
    validate_same_owner(metadata, path)?;
    validate_single_link(metadata, path)?;
    if private {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(invalid_data(format!(
                    "managed plugin file {} is accessible by another user",
                    path.display()
                )));
            }
        }
    } else {
        validate_not_other_writable(metadata, path)?;
    }
    Ok(())
}

fn bundle_matches_disk(paths: &PluginPaths, bundle: &RenderedBundle) -> io::Result<bool> {
    #[cfg(unix)]
    {
        bundle_matches_disk_unix(paths, bundle)
    }
    #[cfg(not(unix))]
    {
        bundle_matches_disk_portable(paths, bundle)
    }
}

#[cfg(unix)]
fn bundle_matches_disk_unix(paths: &PluginPaths, bundle: &RenderedBundle) -> io::Result<bool> {
    let home = open_unix_directory(&paths.codex_home, false)?;
    let home_metadata = home.metadata()?;
    let source_root_path = paths.codex_home.join("abtop");
    let Some(source_root) = open_managed_directory_at(&home, "abtop", &source_root_path, true)?
    else {
        return Ok(false);
    };
    let root_required = BTreeSet::from([OsString::from("marketplace")]);
    let root_optional = BTreeSet::from([OsString::from(".setup.lock")]);
    if !unix_directory_has_required_and_optional_names(
        &source_root,
        &root_required,
        &root_optional,
        &source_root_path,
        "managed source root",
    )? {
        return Ok(false);
    }

    let marketplace_path = source_root_path.join("marketplace");
    let Some(marketplace) =
        open_managed_directory_at(&source_root, "marketplace", &marketplace_path, true)?
    else {
        return Ok(false);
    };
    let marketplace_names = BTreeSet::from([OsString::from(".agents"), OsString::from("plugins")]);
    if !unix_directory_has_exact_names(
        &marketplace,
        &marketplace_names,
        &marketplace_path,
        "managed marketplace root",
    )? {
        return Ok(false);
    }

    let agents_path = marketplace_path.join(".agents");
    let Some(agents) = open_managed_directory_at(&marketplace, ".agents", &agents_path, true)?
    else {
        return Ok(false);
    };
    let source_plugins_path = marketplace_path.join("plugins");
    let Some(source_plugins) =
        open_managed_directory_at(&marketplace, "plugins", &source_plugins_path, true)?
    else {
        return Ok(false);
    };
    if !unix_directory_has_exact_names(
        &agents,
        &BTreeSet::from([OsString::from("plugins")]),
        &agents_path,
        "managed marketplace metadata root",
    )? || !unix_directory_has_exact_names(
        &source_plugins,
        &BTreeSet::from([OsString::from(PLUGIN_NAME)]),
        &source_plugins_path,
        "managed plugin source parent",
    )? {
        return Ok(false);
    }

    let agents_plugins_path = agents_path.join("plugins");
    let Some(agents_plugins) =
        open_managed_directory_at(&agents, "plugins", &agents_plugins_path, true)?
    else {
        return Ok(false);
    };
    let plugin_root_path = source_plugins_path.join(PLUGIN_NAME);
    let Some(plugin_root) =
        open_managed_directory_at(&source_plugins, PLUGIN_NAME, &plugin_root_path, true)?
    else {
        return Ok(false);
    };
    let plugin_root_names = BTreeSet::from([
        OsString::from(".codex-plugin"),
        OsString::from("hooks"),
        OsString::from("scripts"),
    ]);
    if !unix_directory_has_exact_names(
        &agents_plugins,
        &BTreeSet::from([OsString::from("marketplace.json")]),
        &agents_plugins_path,
        "managed marketplace manifest directory",
    )? || !unix_directory_has_exact_names(
        &plugin_root,
        &plugin_root_names,
        &plugin_root_path,
        "managed plugin source root",
    )? {
        return Ok(false);
    }

    let manifest_path = plugin_root_path.join(".codex-plugin");
    let hooks_path = plugin_root_path.join("hooks");
    let scripts_path = plugin_root_path.join("scripts");
    let Some(manifest_directory) =
        open_managed_directory_at(&plugin_root, ".codex-plugin", &manifest_path, true)?
    else {
        return Ok(false);
    };
    let Some(hooks_directory) =
        open_managed_directory_at(&plugin_root, "hooks", &hooks_path, true)?
    else {
        return Ok(false);
    };
    let Some(scripts_directory) =
        open_managed_directory_at(&plugin_root, "scripts", &scripts_path, true)?
    else {
        return Ok(false);
    };
    if !unix_directory_has_exact_names(
        &manifest_directory,
        &BTreeSet::from([OsString::from("plugin.json")]),
        &manifest_path,
        "managed plugin manifest directory",
    )? || !unix_directory_has_exact_names(
        &hooks_directory,
        &BTreeSet::from([OsString::from("hooks.json")]),
        &hooks_path,
        "managed hooks directory",
    )? || !unix_directory_has_exact_names(
        &scripts_directory,
        &BTreeSet::from([
            OsString::from("abtop-codex-hook.cmd"),
            OsString::from("abtop-codex-hook.sh"),
        ]),
        &scripts_path,
        "managed scripts directory",
    )? {
        return Ok(false);
    }

    let marketplace_manifest_path = agents_plugins_path.join("marketplace.json");
    let plugin_manifest_path = manifest_path.join("plugin.json");
    let hooks_file_path = hooks_path.join("hooks.json");
    let posix_path = scripts_path.join("abtop-codex-hook.sh");
    let windows_path = scripts_path.join("abtop-codex-hook.cmd");
    let Some(marketplace_manifest) = open_matching_managed_file_at(
        &agents_plugins,
        "marketplace.json",
        &marketplace_manifest_path,
        &bundle.marketplace_manifest,
        false,
        true,
    )?
    else {
        return Ok(false);
    };
    let Some(plugin_manifest) = open_matching_managed_file_at(
        &manifest_directory,
        "plugin.json",
        &plugin_manifest_path,
        &bundle.plugin_manifest,
        false,
        true,
    )?
    else {
        return Ok(false);
    };
    let Some(hooks_file) = open_matching_managed_file_at(
        &hooks_directory,
        "hooks.json",
        &hooks_file_path,
        &bundle.hooks_manifest,
        false,
        true,
    )?
    else {
        return Ok(false);
    };
    let Some(posix_launcher) = open_matching_managed_file_at(
        &scripts_directory,
        "abtop-codex-hook.sh",
        &posix_path,
        &bundle.posix_launcher,
        true,
        true,
    )?
    else {
        return Ok(false);
    };
    let Some(windows_launcher) = open_matching_managed_file_at(
        &scripts_directory,
        "abtop-codex-hook.cmd",
        &windows_path,
        &bundle.windows_launcher,
        false,
        true,
    )?
    else {
        return Ok(false);
    };

    let rebound_home = open_unix_directory(&paths.codex_home, false)?;
    if !same_file_metadata(&home_metadata, &rebound_home.metadata()?) {
        return Err(invalid_data("CODEX_HOME changed during source validation"));
    }
    let rebound_source_root = reopen_same_directory_at(
        &rebound_home,
        "abtop",
        &source_root,
        &source_root_path,
        true,
    )?;
    let rebound_marketplace = reopen_same_directory_at(
        &rebound_source_root,
        "marketplace",
        &marketplace,
        &marketplace_path,
        true,
    )?;
    let rebound_agents =
        reopen_same_directory_at(&rebound_marketplace, ".agents", &agents, &agents_path, true)?;
    let rebound_source_plugins = reopen_same_directory_at(
        &rebound_marketplace,
        "plugins",
        &source_plugins,
        &source_plugins_path,
        true,
    )?;
    let rebound_agents_plugins = reopen_same_directory_at(
        &rebound_agents,
        "plugins",
        &agents_plugins,
        &agents_plugins_path,
        true,
    )?;
    let rebound_plugin_root = reopen_same_directory_at(
        &rebound_source_plugins,
        PLUGIN_NAME,
        &plugin_root,
        &plugin_root_path,
        true,
    )?;
    let rebound_manifest = reopen_same_directory_at(
        &rebound_plugin_root,
        ".codex-plugin",
        &manifest_directory,
        &manifest_path,
        true,
    )?;
    let rebound_hooks = reopen_same_directory_at(
        &rebound_plugin_root,
        "hooks",
        &hooks_directory,
        &hooks_path,
        true,
    )?;
    let rebound_scripts = reopen_same_directory_at(
        &rebound_plugin_root,
        "scripts",
        &scripts_directory,
        &scripts_path,
        true,
    )?;
    reopen_same_file_at(
        &rebound_agents_plugins,
        "marketplace.json",
        &marketplace_manifest,
        &marketplace_manifest_path,
        &bundle.marketplace_manifest,
        false,
        true,
    )?;
    reopen_same_file_at(
        &rebound_manifest,
        "plugin.json",
        &plugin_manifest,
        &plugin_manifest_path,
        &bundle.plugin_manifest,
        false,
        true,
    )?;
    reopen_same_file_at(
        &rebound_hooks,
        "hooks.json",
        &hooks_file,
        &hooks_file_path,
        &bundle.hooks_manifest,
        false,
        true,
    )?;
    reopen_same_file_at(
        &rebound_scripts,
        "abtop-codex-hook.sh",
        &posix_launcher,
        &posix_path,
        &bundle.posix_launcher,
        true,
        true,
    )?;
    reopen_same_file_at(
        &rebound_scripts,
        "abtop-codex-hook.cmd",
        &windows_launcher,
        &windows_path,
        &bundle.windows_launcher,
        false,
        true,
    )?;
    Ok(unix_directory_has_required_and_optional_names(
        &rebound_source_root,
        &root_required,
        &root_optional,
        &source_root_path,
        "managed source root",
    )? && unix_directory_has_exact_names(
        &rebound_marketplace,
        &marketplace_names,
        &marketplace_path,
        "managed marketplace root",
    )? && unix_directory_has_exact_names(
        &rebound_plugin_root,
        &plugin_root_names,
        &plugin_root_path,
        "managed plugin source root",
    )? && private_runtime_state_tree_valid(paths)?)
}

#[cfg(not(unix))]
fn bundle_matches_disk_portable(paths: &PluginPaths, bundle: &RenderedBundle) -> io::Result<bool> {
    if !audit_owned_source_tree(paths, true)? {
        return Ok(false);
    }
    if !private_runtime_state_tree_valid(paths)? {
        return Ok(false);
    }
    let expected = [
        (
            &paths.marketplace_manifest,
            bundle.marketplace_manifest.as_slice(),
            false,
        ),
        (
            &paths.plugin_manifest,
            bundle.plugin_manifest.as_slice(),
            false,
        ),
        (
            &paths.hooks_manifest,
            bundle.hooks_manifest.as_slice(),
            false,
        ),
        (
            &paths.posix_launcher,
            bundle.posix_launcher.as_slice(),
            true,
        ),
        (
            &paths.windows_launcher,
            bundle.windows_launcher.as_slice(),
            false,
        ),
    ];
    for (path, bytes, executable) in expected {
        if !private_regular_file_matches(path, bytes, executable)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn private_runtime_state_tree_valid(paths: &PluginPaths) -> io::Result<bool> {
    #[cfg(unix)]
    {
        private_runtime_state_tree_valid_unix(paths)
    }
    #[cfg(not(unix))]
    {
        private_runtime_state_tree_valid_portable(paths)
    }
}

#[cfg(unix)]
fn private_runtime_state_tree_valid_unix(paths: &PluginPaths) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;

    let home = open_unix_directory(&paths.codex_home, false)?;
    let home_metadata = home.metadata()?;
    let plugins_path = paths.codex_home.join("plugins");
    let Some(plugins) = open_managed_directory_at(&home, "plugins", &plugins_path, false)? else {
        return Ok(false);
    };
    let data_path = plugins_path.join("data");
    let Some(data) = open_managed_directory_at(&plugins, "data", &data_path, false)? else {
        return Ok(false);
    };
    let Some(data_root) =
        open_managed_directory_at(&data, "abtop-abtop-local", &paths.plugin_data_root, true)?
    else {
        return Ok(false);
    };
    let states_path = paths.plugin_data_root.join(HOOK_STATE_DIR_NAME);
    let Some(states) =
        open_managed_directory_at(&data_root, HOOK_STATE_DIR_NAME, &states_path, true)?
    else {
        return Ok(false);
    };
    let faults_path = states_path.join(HOOK_FAULT_DIR_NAME);
    let Some(faults) = open_managed_directory_at(&states, HOOK_FAULT_DIR_NAME, &faults_path, true)?
    else {
        return Ok(false);
    };
    for directory in [&data_root, &states, &faults] {
        if directory.metadata()?.permissions().mode() & 0o777 != 0o700 {
            return Ok(false);
        }
    }

    let rebound_home = open_unix_directory(&paths.codex_home, false)?;
    if !same_file_metadata(&home_metadata, &rebound_home.metadata()?) {
        return Err(invalid_data(
            "CODEX_HOME changed during runtime-state validation",
        ));
    }
    let rebound_plugins =
        reopen_same_directory_at(&rebound_home, "plugins", &plugins, &plugins_path, false)?;
    let rebound_data =
        reopen_same_directory_at(&rebound_plugins, "data", &data, &data_path, false)?;
    let rebound_data_root = reopen_same_directory_at(
        &rebound_data,
        "abtop-abtop-local",
        &data_root,
        &paths.plugin_data_root,
        true,
    )?;
    let rebound_states = reopen_same_directory_at(
        &rebound_data_root,
        HOOK_STATE_DIR_NAME,
        &states,
        &states_path,
        true,
    )?;
    let rebound_faults = reopen_same_directory_at(
        &rebound_states,
        HOOK_FAULT_DIR_NAME,
        &faults,
        &faults_path,
        true,
    )?;
    let exact_modes = [&rebound_data_root, &rebound_states, &rebound_faults]
        .into_iter()
        .all(|directory| {
            directory
                .metadata()
                .is_ok_and(|metadata| metadata.permissions().mode() & 0o777 == 0o700)
        });
    Ok(exact_modes)
}

#[cfg(not(unix))]
fn private_runtime_state_tree_valid_portable(paths: &PluginPaths) -> io::Result<bool> {
    for directory in [
        paths.plugin_data_root.clone(),
        paths.plugin_data_root.join(HOOK_STATE_DIR_NAME),
        paths
            .plugin_data_root
            .join(HOOK_STATE_DIR_NAME)
            .join(HOOK_FAULT_DIR_NAME),
    ] {
        if !private_directory_valid(&directory)? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(not(unix))]
fn private_directory_valid(path: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(false);
    }
    validate_same_owner(&metadata, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(not(unix))]
fn plugin_data_hierarchy_valid(paths: &PluginPaths) -> io::Result<bool> {
    for directory in [
        paths.codex_home.join("plugins"),
        paths.codex_home.join("plugins/data"),
    ] {
        let metadata = match fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Ok(false);
        }
        validate_same_owner(&metadata, &directory)?;
        validate_not_other_writable(&metadata, &directory)?;
    }
    private_directory_valid(&paths.plugin_data_root)
}

fn read_installation_attestation_bytes(paths: &PluginPaths) -> io::Result<Option<Vec<u8>>> {
    #[cfg(unix)]
    {
        read_installation_attestation_bytes_unix(paths)
    }
    #[cfg(not(unix))]
    {
        if !plugin_data_hierarchy_valid(paths)? {
            return Ok(None);
        }
        if fs::symlink_metadata(&paths.install_attestation).is_ok()
            && !private_regular_mode_matches(&paths.install_attestation, false)?
        {
            return Err(invalid_data(
                "installation attestation does not have the exact private file mode",
            ));
        }
        read_private_regular_file(&paths.install_attestation)
    }
}

#[cfg(unix)]
fn read_installation_attestation_bytes_unix(paths: &PluginPaths) -> io::Result<Option<Vec<u8>>> {
    let home = open_unix_directory(&paths.codex_home, false)?;
    let home_metadata = home.metadata()?;
    let plugins_path = paths.codex_home.join("plugins");
    let Some(plugins) = open_managed_directory_at(&home, "plugins", &plugins_path, false)? else {
        return Ok(None);
    };
    let data_path = plugins_path.join("data");
    let Some(data) = open_managed_directory_at(&plugins, "data", &data_path, false)? else {
        return Ok(None);
    };
    let Some(data_root) =
        open_managed_directory_at(&data, "abtop-abtop-local", &paths.plugin_data_root, true)?
    else {
        return Ok(None);
    };
    let Some(bytes) = read_private_regular_file_at(
        &data_root,
        INSTALL_ATTESTATION_FILE,
        &paths.install_attestation,
    )?
    else {
        return Ok(None);
    };
    let pinned = open_matching_managed_file_at(
        &data_root,
        INSTALL_ATTESTATION_FILE,
        &paths.install_attestation,
        &bytes,
        false,
        true,
    )?
    .ok_or_else(|| invalid_data("installation attestation does not have the exact private mode"))?;

    let rebound_home = open_unix_directory(&paths.codex_home, false)?;
    if !same_file_metadata(&home_metadata, &rebound_home.metadata()?) {
        return Err(invalid_data(
            "CODEX_HOME changed during attestation validation",
        ));
    }
    let rebound_plugins =
        reopen_same_directory_at(&rebound_home, "plugins", &plugins, &plugins_path, false)?;
    let rebound_data =
        reopen_same_directory_at(&rebound_plugins, "data", &data, &data_path, false)?;
    let rebound_data_root = reopen_same_directory_at(
        &rebound_data,
        "abtop-abtop-local",
        &data_root,
        &paths.plugin_data_root,
        true,
    )?;
    reopen_same_file_at(
        &rebound_data_root,
        INSTALL_ATTESTATION_FILE,
        &pinned,
        &paths.install_attestation,
        &bytes,
        false,
        true,
    )?;
    Ok(Some(bytes))
}

fn attestation_matches(paths: &PluginPaths, bundle: &RenderedBundle) -> io::Result<bool> {
    let Some(bytes) = read_installation_attestation_bytes(paths)? else {
        return Ok(false);
    };
    let attestation: InstallationAttestation = serde_json::from_slice(&bytes)
        .map_err(|error| invalid_data(format!("invalid installation attestation: {error}")))?;
    Ok(valid_attestation_shape(&attestation) && attestation_identity_matches(&attestation, bundle))
}

fn attestation_identity_matches(
    attestation: &InstallationAttestation,
    bundle: &RenderedBundle,
) -> bool {
    attestation.schema_version == 1
        && attestation.hook_schema_revision == HOOK_SCHEMA_REVISION
        && attestation.helper_digest == bundle.helper_digest
        && attestation.plugin_id == PLUGIN_ID
        && attestation.plugin_version == bundle.plugin_version
        && attestation.hooks_digest == bundle.hooks_digest
        && attestation.hook_events
            == HOOK_EVENTS
                .iter()
                .map(|event| (*event).to_string())
                .collect::<Vec<_>>()
}

fn remove_marketplace_if_owned(
    paths: &PluginPaths,
    codex_binary: &Path,
    codex_binary_digest: &str,
    codex_home: &Path,
) -> io::Result<bool> {
    ensure_plugin_absent(paths)?;
    let current = inspect_config_state(paths, None)?;
    if let Some(conflict) = current.marketplace_conflict {
        return Err(invalid_data(format!(
            "refusing to remove marketplace `{MARKETPLACE_NAME}` because it now points at {}",
            conflict.display()
        )));
    }
    if current.marketplace_malformed {
        return Err(invalid_data(format!(
            "cannot prove ownership of malformed `marketplaces.{MARKETPLACE_NAME}`; the plugin was removed, but the marketplace entry and abtop source bundle were preserved for manual recovery"
        )));
    }
    if !current.marketplace_registered {
        return Ok(false);
    }

    remove_marketplace_cli(codex_binary, codex_binary_digest, codex_home)?;
    ensure_marketplace_absent(paths)?;
    Ok(true)
}

fn ensure_plugin_absent(paths: &PluginPaths) -> io::Result<()> {
    let current = inspect_config_state(paths, None)?;
    if current.plugin_configured || current.plugin_config_malformed {
        return Err(invalid_data(format!(
            "reserved plugin `{PLUGIN_ID}` remains configured after native removal; preserving its marketplace and source bundle"
        )));
    }
    let cache_root = paths
        .codex_home
        .join("plugins/cache")
        .join(MARKETPLACE_NAME)
        .join(PLUGIN_NAME);
    match fs::symlink_metadata(&cache_root) {
        Ok(_) => Err(invalid_data(format!(
            "reserved plugin cache {} remains after native removal; preserving its marketplace and source bundle",
            cache_root.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn ensure_marketplace_absent(paths: &PluginPaths) -> io::Result<()> {
    let current = inspect_config_state(paths, None)?;
    if let Some(conflict) = current.marketplace_conflict {
        return Err(invalid_data(format!(
            "marketplace `{MARKETPLACE_NAME}` was concurrently registered from {}; preserving the abtop source bundle",
            conflict.display()
        )));
    }
    if current.marketplace_malformed {
        return Err(invalid_data(format!(
            "marketplace `{MARKETPLACE_NAME}` became malformed during cleanup; preserving the abtop source bundle"
        )));
    }
    if current.marketplace_registered {
        return Err(invalid_data(format!(
            "marketplace `{MARKETPLACE_NAME}` was concurrently re-registered; preserving the abtop source bundle"
        )));
    }
    Ok(())
}

fn remove_marketplace_cli(
    codex_binary: &Path,
    codex_binary_digest: &str,
    codex_home: &Path,
) -> io::Result<()> {
    let output = run_mutating_codex(
        codex_binary,
        codex_binary_digest,
        codex_home,
        &[
            OsString::from("plugin"),
            OsString::from("marketplace"),
            OsString::from("remove"),
            OsString::from(MARKETPLACE_NAME),
            OsString::from("--json"),
        ],
    )?;
    require_success(&output, "removing the abtop local marketplace")?;
    require_json_object(&output.stdout, "marketplace remove")
}

fn remove_plugin_cli(
    codex_binary: &Path,
    codex_binary_digest: &str,
    codex_home: &Path,
) -> io::Result<()> {
    let output = run_mutating_codex(
        codex_binary,
        codex_binary_digest,
        codex_home,
        &[
            OsString::from("plugin"),
            OsString::from("remove"),
            OsString::from(PLUGIN_ID),
            OsString::from("--json"),
        ],
    )?;
    require_success(&output, "removing the abtop plugin")?;
    require_json_object(&output.stdout, "plugin remove")
}

fn remove_owned_bundle_files(paths: &PluginPaths) -> io::Result<Vec<PathBuf>> {
    #[cfg(unix)]
    {
        remove_owned_bundle_files_unix(paths)
    }
    #[cfg(not(unix))]
    {
        remove_owned_bundle_files_portable(paths)
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct OwnedSourceTreeGuard {
    codex_home_directory: File,
    root_directory: File,
    root_metadata: fs::Metadata,
    root_path: PathBuf,
}

#[cfg(unix)]
impl OwnedSourceTreeGuard {
    fn open(paths: &PluginPaths) -> io::Result<Self> {
        let codex_home_directory = open_unix_directory(&paths.codex_home, false)?;
        let root_path = paths.codex_home.join("abtop");
        let root_directory = openat_unix(
            &codex_home_directory,
            std::ffi::OsStr::new("abtop"),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )?
        .ok_or_else(|| invalid_data("managed abtop source root disappeared during uninstall"))?;
        let root_metadata = root_directory.metadata()?;
        validate_owned_directory(&root_path, &root_metadata, true)?;
        Ok(Self {
            codex_home_directory,
            root_directory,
            root_metadata,
            root_path,
        })
    }

    fn parent_and_leaf(
        &self,
        relative: &Path,
    ) -> io::Result<Option<(File, std::ffi::CString, PathBuf)>> {
        use std::os::unix::ffi::OsStrExt;

        let mut components = relative.components().collect::<Vec<_>>();
        let leaf = components
            .pop()
            .ok_or_else(|| invalid_data("managed source relative path is empty"))?;
        let Component::Normal(leaf) = leaf else {
            return Err(invalid_data("managed source relative path is unsafe"));
        };
        let leaf = std::ffi::CString::new(leaf.as_bytes())
            .map_err(|_| invalid_data("managed source path contains NUL"))?;
        let mut directory = self.root_directory.try_clone()?;
        let mut display = self.root_path.clone();
        for component in components {
            let Component::Normal(name) = component else {
                return Err(invalid_data("managed source relative path is unsafe"));
            };
            display.push(name);
            let Some(next) = openat_unix(
                &directory,
                name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )?
            else {
                return Ok(None);
            };
            validate_owned_directory(&display, &next.metadata()?, true)?;
            directory = next;
        }
        Ok(Some((directory, leaf, display)))
    }

    fn remove_file(&self, relative: &Path) -> io::Result<bool> {
        use std::os::fd::AsRawFd;

        let Some((parent, leaf, mut display)) = self.parent_and_leaf(relative)? else {
            return Ok(false);
        };
        display.push(
            relative
                .file_name()
                .ok_or_else(|| invalid_data("managed source relative path has no file name"))?,
        );
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        let Some(first) = openat_unix_cstr(&parent, &leaf, flags)? else {
            return Ok(false);
        };
        let first_metadata = first.metadata()?;
        validate_owned_regular_file(&display, &first_metadata, true)?;
        let second = openat_unix_cstr(&parent, &leaf, flags)?.ok_or_else(|| {
            invalid_data(format!(
                "managed source file {} disappeared before deletion",
                display.display()
            ))
        })?;
        if !same_file_content_snapshot(&first_metadata, &second.metadata()?) {
            return Err(invalid_data(format!(
                "managed source file {} changed before deletion",
                display.display()
            )));
        }
        if unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(true)
    }

    fn remove_directory(&self, relative: &Path) -> io::Result<bool> {
        use std::os::fd::AsRawFd;

        let Some((parent, leaf, mut display)) = self.parent_and_leaf(relative)? else {
            return Ok(false);
        };
        display.push(
            relative
                .file_name()
                .ok_or_else(|| invalid_data("managed source relative path has no file name"))?,
        );
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let Some(first) = openat_unix_cstr(&parent, &leaf, flags)? else {
            return Ok(false);
        };
        let first_metadata = first.metadata()?;
        validate_owned_directory(&display, &first_metadata, true)?;
        let second = openat_unix_cstr(&parent, &leaf, flags)?.ok_or_else(|| {
            invalid_data(format!(
                "managed source directory {} disappeared before deletion",
                display.display()
            ))
        })?;
        if !same_file_content_snapshot(&first_metadata, &second.metadata()?) {
            return Err(invalid_data(format!(
                "managed source directory {} changed before deletion",
                display.display()
            )));
        }
        if unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(true)
    }

    fn remove_root(self) -> io::Result<()> {
        use std::os::fd::AsRawFd;

        let name = std::ffi::CString::new("abtop").expect("static path has no NUL");
        let current = openat_unix_cstr(
            &self.codex_home_directory,
            &name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )?
        .ok_or_else(|| invalid_data("managed abtop source root disappeared before deletion"))?;
        let current_metadata = current.metadata()?;
        if !same_file_metadata(&self.root_metadata, &current_metadata) {
            return Err(invalid_data(
                "managed abtop source root changed before deletion",
            ));
        }
        validate_owned_directory(&self.root_path, &current_metadata, true)?;
        if unsafe {
            libc::unlinkat(
                self.codex_home_directory.as_raw_fd(),
                name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(unix)]
fn open_unix_directory(path: &Path, private: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    validate_owned_directory(path, &file.metadata()?, private)?;
    Ok(file)
}

#[cfg(unix)]
fn openat_unix(
    parent: &File,
    name: &std::ffi::OsStr,
    flags: libc::c_int,
) -> io::Result<Option<File>> {
    use std::os::unix::ffi::OsStrExt;

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| invalid_data("managed source path contains NUL"))?;
    openat_unix_cstr(parent, &name, flags)
}

#[cfg(unix)]
fn openat_unix_cstr(
    parent: &File,
    name: &std::ffi::CStr,
    flags: libc::c_int,
) -> io::Result<Option<File>> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error);
    }
    Ok(Some(unsafe { File::from_raw_fd(descriptor) }))
}

#[cfg(unix)]
fn openat_create_unix(
    parent: &File,
    name: &std::ffi::OsStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| invalid_data("managed source path contains NUL"))?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags,
            libc::c_uint::from(mode),
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn ensure_managed_directory_at(
    parent: &File,
    name: &str,
    path: &Path,
    private: bool,
    exact_mode: Option<libc::mode_t>,
) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    let name_os = std::ffi::OsStr::new(name);
    let mut directory = open_managed_directory_at(parent, name, path, private)?;
    if directory.is_none() {
        let name_c = std::ffi::CString::new(name_os.as_bytes())
            .map_err(|_| invalid_data("managed source path contains NUL"))?;
        let mode = exact_mode.unwrap_or(0o700);
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), mode) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        directory = open_managed_directory_at(parent, name, path, private)?;
    }
    let directory = directory.ok_or_else(|| {
        invalid_data(format!(
            "managed plugin directory {} could not be created",
            path.display()
        ))
    })?;
    if let Some(mode) = exact_mode {
        if unsafe { libc::fchmod(directory.as_raw_fd(), mode) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if u64::from(directory.metadata()?.permissions().mode() & 0o777) != u64::from(mode) {
            return Err(invalid_data(format!(
                "managed plugin directory {} does not have the exact private mode",
                path.display()
            )));
        }
    }
    validate_owned_directory(path, &directory.metadata()?, private)?;
    Ok(directory)
}

#[cfg(unix)]
fn remove_owned_bundle_files_unix(paths: &PluginPaths) -> io::Result<Vec<PathBuf>> {
    audit_owned_source_tree(paths, false)?;
    match fs::symlink_metadata(paths.codex_home.join("abtop")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let guard = OwnedSourceTreeGuard::open(paths)?;
    let files = [
        (
            Path::new("marketplace/plugins/abtop/scripts/abtop-codex-hook.cmd"),
            paths.windows_launcher.clone(),
        ),
        (
            Path::new("marketplace/plugins/abtop/scripts/abtop-codex-hook.sh"),
            paths.posix_launcher.clone(),
        ),
        (
            Path::new("marketplace/plugins/abtop/hooks/hooks.json"),
            paths.hooks_manifest.clone(),
        ),
        (
            Path::new("marketplace/plugins/abtop/.codex-plugin/plugin.json"),
            paths.plugin_manifest.clone(),
        ),
        (
            Path::new("marketplace/.agents/plugins/marketplace.json"),
            paths.marketplace_manifest.clone(),
        ),
    ];
    let mut removed = Vec::new();
    for (relative, reported) in files {
        if guard.remove_file(relative)? {
            removed.push(reported);
        }
    }
    // Pre-stable-lock installations left this source-local lock behind. It is
    // never used by the current process; the live retained lock is anchored
    // directly below CODEX_HOME.
    let _ = guard.remove_file(Path::new(".setup.lock"))?;
    for relative in [
        "marketplace/plugins/abtop/scripts",
        "marketplace/plugins/abtop/hooks",
        "marketplace/plugins/abtop/.codex-plugin",
        "marketplace/plugins/abtop",
        "marketplace/.agents/plugins",
        "marketplace/.agents",
        "marketplace/plugins",
        "marketplace",
    ] {
        let _ = guard.remove_directory(Path::new(relative))?;
    }
    guard.remove_root()?;
    Ok(removed)
}

#[cfg(not(unix))]
fn remove_owned_bundle_files_portable(paths: &PluginPaths) -> io::Result<Vec<PathBuf>> {
    audit_owned_source_tree(paths, false)?;
    match fs::symlink_metadata(paths.codex_home.join("abtop")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let mut removed = Vec::new();
    for path in [
        &paths.windows_launcher,
        &paths.posix_launcher,
        &paths.hooks_manifest,
        &paths.plugin_manifest,
        &paths.marketplace_manifest,
    ] {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(invalid_data(format!(
                    "refusing to remove unsafe managed plugin path {}",
                    path.display()
                )));
            }
            Ok(_) => {
                validate_source_ancestor_chain(paths, path)?;
                let metadata = fs::symlink_metadata(path)?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(invalid_data(format!(
                        "refusing to remove substituted managed plugin path {}",
                        path.display()
                    )));
                }
                validate_same_owner(&metadata, path)?;
                validate_single_link(&metadata, path)?;
                fs::remove_file(path)?;
                removed.push(path.clone());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let legacy_setup_lock = paths.codex_home.join("abtop/.setup.lock");
    match fs::symlink_metadata(&legacy_setup_lock) {
        Ok(metadata) => {
            validate_owned_regular_file(&legacy_setup_lock, &metadata, true)?;
            fs::remove_file(&legacy_setup_lock)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let directories = vec![
        paths.posix_launcher.parent().unwrap().to_path_buf(),
        paths.hooks_manifest.parent().unwrap().to_path_buf(),
        paths.plugin_manifest.parent().unwrap().to_path_buf(),
        paths.plugin_root.clone(),
        paths.marketplace_manifest.parent().unwrap().to_path_buf(),
        paths.marketplace_root.join(".agents"),
        paths.marketplace_root.join("plugins"),
        paths.marketplace_root.clone(),
    ];
    for directory in directories {
        match fs::symlink_metadata(&directory) {
            Ok(_) => {
                validate_source_ancestor_chain(paths, &directory)?;
                let metadata = fs::symlink_metadata(&directory)?;
                validate_owned_directory(&directory, &metadata, true)?;
                fs::remove_dir(directory)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(removed)
}

#[cfg(not(unix))]
fn validate_source_ancestor_chain(paths: &PluginPaths, leaf: &Path) -> io::Result<()> {
    let root = paths.codex_home.join("abtop");
    if !leaf.starts_with(&root) {
        return Err(invalid_data(format!(
            "managed plugin path {} is outside the abtop source root",
            leaf.display()
        )));
    }
    let mut current = root;
    let parent = leaf
        .parent()
        .ok_or_else(|| invalid_data("managed plugin path has no parent directory"))?;
    validate_owned_directory(&current, &fs::symlink_metadata(&current)?, true)?;
    let relative_parent = parent
        .strip_prefix(&current)
        .map_err(|_| invalid_data("managed plugin parent escaped its source root"))?;
    for component in relative_parent.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)?;
        validate_owned_directory(&current, &metadata, true)?;
    }
    Ok(())
}

fn current_codex_home() -> io::Result<PathBuf> {
    if let Some(value) = std::env::var_os("CODEX_HOME") {
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err(invalid_data("CODEX_HOME must be an absolute path"));
        }
        return Ok(path);
    }
    dirs::home_dir()
        .map(|home| home.join(".codex"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot determine CODEX_HOME"))
}

fn current_abtop_binary() -> io::Result<PathBuf> {
    let binary = std::env::current_exe()?;
    validate_abtop_binary(&binary)
}

fn resolve_codex_binary() -> io::Result<PathBuf> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PATH is not set"))?;
    let cwd = std::env::current_dir()?;
    #[cfg(windows)]
    let names = windows_codex_executable_names(std::env::var_os("PATHEXT").as_deref());
    #[cfg(not(windows))]
    let names = vec![OsString::from("codex")];
    resolve_codex_binary_in_path(&path, &cwd, &names)
}

fn resolve_codex_binary_in_path(
    path: &std::ffi::OsStr,
    cwd: &Path,
    names: &[OsString],
) -> io::Result<PathBuf> {
    for root in std::env::split_paths(&path) {
        let root = if root.as_os_str().is_empty() {
            cwd.to_path_buf()
        } else if root.is_absolute() {
            root
        } else {
            cwd.join(root)
        };
        for name in names {
            let candidate = root.join(name);
            if executable_target(&candidate) {
                return normalize_absolute(&candidate);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "cannot find a native Codex executable on PATH",
    ))
}

#[cfg(any(windows, test))]
fn windows_codex_executable_names(pathext: Option<&std::ffi::OsStr>) -> Vec<OsString> {
    let configured = pathext.and_then(std::ffi::OsStr::to_str);
    windows_codex_executable_names_from_text(configured)
}

#[cfg(any(windows, test))]
fn windows_codex_executable_names_from_text(pathext: Option<&str>) -> Vec<OsString> {
    const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

    let configured = pathext
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_PATHEXT);
    let mut seen = BTreeSet::new();
    let mut names = Vec::new();
    for extension in configured.split(';') {
        if extension.len() < 2
            || !extension.starts_with('.')
            || extension
                .bytes()
                .any(|byte| matches!(byte, b'/' | b'\\' | b':' | b'\0'))
        {
            continue;
        }
        let comparison = extension.to_ascii_uppercase();
        if seen.insert(comparison) {
            names.push(OsString::from(format!("codex{extension}")));
        }
    }
    if names.is_empty() && configured != DEFAULT_PATHEXT {
        return windows_codex_executable_names_from_text(Some(DEFAULT_PATHEXT));
    }
    names
}

fn prepare_codex_home(path: &Path) -> io::Result<PathBuf> {
    let path = normalize_absolute(path)?;
    if !path.exists() {
        let parent = path
            .parent()
            .ok_or_else(|| invalid_data("CODEX_HOME has no parent directory"))?;
        let parent = fs::canonicalize(parent)?;
        let parent_metadata = fs::metadata(&parent)?;
        if !parent_metadata.is_dir() {
            return Err(invalid_data("CODEX_HOME parent is not a directory"));
        }
        validate_same_owner(&parent_metadata, &parent)?;
        validate_not_other_writable(&parent_metadata, &parent)?;
        fs::create_dir(&path)?;
        #[cfg(unix)]
        fs::set_permissions(&path, unix_permissions(0o700))?;
    }
    let canonical = fs::canonicalize(&path)?;
    let metadata = fs::metadata(&canonical)?;
    if !metadata.is_dir() {
        return Err(invalid_data("CODEX_HOME is not a directory"));
    }
    validate_same_owner(&metadata, &canonical)?;
    Ok(canonical)
}

fn normalize_existing_or_lexical(path: &Path) -> io::Result<PathBuf> {
    let path = normalize_absolute(path)?;
    match fs::canonicalize(&path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path),
        Err(error) => Err(error),
    }
}

fn validate_codex_binary_compatibility(path: &Path, codex_home: &Path) -> io::Result<PathBuf> {
    ensure_hook_state_platform_supported()?;
    let (path, version) = inspect_codex_binary_identity(path, codex_home)?;
    validate_supported_codex_release(&version)?;

    let features = run_codex(
        &path,
        codex_home,
        &[OsString::from("features"), OsString::from("list")],
    )?;
    require_success(&features, "checking native Codex hook features")?;
    validate_required_feature_rows(&features.stdout)?;
    validate_generated_hook_schema(&path, codex_home)?;
    Ok(path)
}

fn ensure_hook_state_platform_supported() -> io::Result<()> {
    if super::state::hook_state_platform_supported() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native Codex hook state is supported only on macOS and Linux",
        ))
    }
}

fn validate_supported_codex_release(bytes: &[u8]) -> io::Result<()> {
    let (major, minor, patch) = parse_codex_cli_version(bytes)?;
    if (major, minor, patch) != (0, 146, 0) {
        return Err(invalid_data(format!(
            "Codex hook integration requires codex-cli {SUPPORTED_CODEX_VERSION}, but the selected executable reports {major}.{minor}.{patch}; every other release remains unsupported until its hook contract is audited"
        )));
    }
    Ok(())
}

fn validate_codex_binary_identity(path: &Path, codex_home: &Path) -> io::Result<PathBuf> {
    inspect_codex_binary_identity(path, codex_home).map(|(path, _version)| path)
}

fn capture_codex_binary_identity(path: &Path, codex_home: &Path) -> io::Result<(PathBuf, String)> {
    let path = validate_executable_path(path, "Codex executable")?;
    let before = executable_path_digest(&path)?;
    let validated = validate_codex_binary_identity(&path, codex_home)?;
    let after = executable_path_digest(&validated)?;
    if validated != path || before != after {
        return Err(invalid_data(
            "the native Codex executable changed during identity preflight",
        ));
    }
    Ok((validated, after))
}

fn capture_codex_binary_compatibility(
    path: &Path,
    codex_home: &Path,
) -> io::Result<(PathBuf, String)> {
    let path = validate_executable_path(path, "Codex executable")?;
    let before = executable_path_digest(&path)?;
    let validated = validate_codex_binary_compatibility(&path, codex_home)?;
    let after = executable_path_digest(&validated)?;
    if validated != path || before != after {
        return Err(invalid_data(
            "the native Codex executable changed during compatibility preflight",
        ));
    }
    Ok((validated, after))
}

fn inspect_codex_binary_identity(path: &Path, codex_home: &Path) -> io::Result<(PathBuf, Vec<u8>)> {
    let path = validate_executable_path(path, "Codex executable")?;
    let output = run_codex(&path, codex_home, &[OsString::from("--version")])?;
    require_success(&output, "checking the native Codex version")?;
    parse_codex_cli_version(&output.stdout)?;
    Ok((path, output.stdout))
}

fn parse_codex_cli_version(bytes: &[u8]) -> io::Result<(u64, u64, u64)> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid_data("native Codex returned a non-UTF-8 version"))?;
    let line = text.strip_suffix('\n').unwrap_or(text);
    let line = line.strip_suffix('\r').unwrap_or(line);
    let version = line.strip_prefix("codex-cli ").ok_or_else(|| {
        invalid_data("the selected executable did not report an exact native `codex-cli` version")
    })?;
    if version.is_empty()
        || version
            .bytes()
            .any(|byte| !byte.is_ascii_digit() && byte != b'.')
    {
        return Err(invalid_data(format!(
            "native Codex reported an unsupported semantic version `{version}`"
        )));
    }
    let components = version.split('.').collect::<Vec<_>>();
    if components.len() != 3
        || components.iter().any(|component| {
            component.is_empty() || component.len() > 1 && component.starts_with('0')
        })
    {
        return Err(invalid_data(format!(
            "native Codex reported an unsupported semantic version `{version}`"
        )));
    }
    let parsed = components
        .into_iter()
        .map(|component| {
            component.parse::<u64>().map_err(|_| {
                invalid_data(format!(
                    "native Codex reported an unsupported semantic version `{version}`"
                ))
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok((parsed[0], parsed[1], parsed[2]))
}

fn validate_required_feature_rows(bytes: &[u8]) -> io::Result<()> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid_data("native Codex returned a non-UTF-8 feature list"))?;
    let mut found = BTreeSet::new();
    for line in text.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let Some(name @ ("hooks" | "plugins")) = fields.first().copied() else {
            continue;
        };
        if fields.len() != 3 || fields[1] != "stable" || fields[2] != "true" {
            return Err(invalid_data(format!(
                "native Codex feature `{name}` must be reported exactly as stable and enabled"
            )));
        }
        if !found.insert(name) {
            return Err(invalid_data(format!(
                "native Codex reported duplicate `{name}` feature rows"
            )));
        }
    }
    if found != BTreeSet::from(["hooks", "plugins"]) {
        return Err(invalid_data(
            "native Codex did not report both stable enabled `hooks` and `plugins` features",
        ));
    }
    Ok(())
}

fn validate_generated_hook_schema(codex_binary: &Path, codex_home: &Path) -> io::Result<()> {
    let output_root = tempfile::Builder::new()
        .prefix("abtop-codex-schema-")
        .tempdir()?;
    let output = run_codex(
        codex_binary,
        codex_home,
        &[
            OsString::from("app-server"),
            OsString::from("generate-json-schema"),
            OsString::from("--out"),
            output_root.path().as_os_str().to_owned(),
        ],
    )?;
    require_success(&output, "generating the native Codex compatibility schema")?;
    let schema_path = output_root
        .path()
        .join("v2/ConfigRequirementsReadResponse.json");
    let schema = read_bounded_generated_schema(&schema_path)?;
    validate_hook_schema_bytes(&schema)
}

fn validate_hook_schema_bytes(schema: &[u8]) -> io::Result<()> {
    let schema: Value = serde_json::from_slice(schema).map_err(|error| {
        invalid_data(format!(
            "native Codex generated an invalid hook compatibility schema: {error}"
        ))
    })?;
    let properties = schema
        .get("definitions")
        .and_then(|definitions| definitions.get("ManagedHooksRequirements"))
        .and_then(|requirements| requirements.get("properties"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            invalid_data("native Codex compatibility schema omitted managed hook event properties")
        })?;
    let actual = properties
        .keys()
        .filter(|name| name.as_bytes().first().is_some_and(u8::is_ascii_uppercase))
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let expected = HOOK_EVENTS.into_iter().collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(invalid_data(format!(
            "native Codex managed-hook event set is incompatible: expected {}, found {}",
            expected.into_iter().collect::<Vec<_>>().join(", "),
            actual.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(())
}

fn read_bounded_generated_schema(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot inspect generated Codex compatibility schema {}: {error}",
                path.display()
            ),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_MANAGED_FILE
    {
        return Err(invalid_data(format!(
            "generated Codex compatibility schema {} is unsafe or oversized",
            path.display()
        )));
    }
    validate_same_owner(&metadata, path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    if !same_file_metadata(&metadata, &file.metadata()?) {
        return Err(invalid_data(
            "generated Codex compatibility schema changed while it was opened",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take(MAX_MANAGED_FILE + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANAGED_FILE
        || !same_file_metadata(&metadata, &fs::symlink_metadata(path)?)
    {
        return Err(invalid_data(
            "generated Codex compatibility schema changed or exceeded its safety bound",
        ));
    }
    Ok(bytes)
}

fn validate_abtop_binary(path: &Path) -> io::Result<PathBuf> {
    let path = validate_executable_path(path, "abtop executable")?;
    let canonical = fs::canonicalize(&path)?;
    validate_executable_path(&canonical, "abtop executable")
}

fn validate_executable_path(path: &Path, label: &str) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(invalid_data(format!("{label} path must be absolute")));
    }
    let path = normalize_absolute(path)?;
    let metadata = fs::metadata(&path).map_err(|error| {
        io::Error::new(error.kind(), format!("cannot inspect {label}: {error}"))
    })?;
    if !metadata.is_file() {
        return Err(invalid_data(format!("{label} is not a regular file")));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(invalid_data(format!("{label} is not executable")));
        }
    }
    Ok(path)
}

fn executable_target(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(not(unix))]
fn ensure_private_data_ancestry(paths: &PluginPaths) -> io::Result<()> {
    let plugins = paths.codex_home.join("plugins");
    let data = plugins.join("data");
    for directory in [&plugins, &data] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(invalid_data(format!(
                        "unsafe Codex plugin data ancestor {}",
                        directory.display()
                    )));
                }
                validate_same_owner(&metadata, directory)?;
                validate_not_other_writable(&metadata, directory)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(directory)?;
                #[cfg(unix)]
                fs::set_permissions(directory, unix_permissions(0o700))?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(any(not(unix), test))]
fn ensure_private_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(invalid_data(format!(
                    "managed plugin path {} is not a safe directory",
                    path.display()
                )));
            }
            validate_same_owner(&metadata, path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
        }
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    fs::set_permissions(path, unix_permissions(0o700))?;
    Ok(())
}

#[cfg(any(not(unix), test))]
fn atomic_write_private(path: &Path, bytes: &[u8], executable: bool) -> io::Result<()> {
    #[cfg(not(unix))]
    let _ = executable;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(invalid_data(format!(
                "managed plugin path {} is not a regular file",
                path.display()
            )));
        }
        validate_same_owner(&metadata, path)?;
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("managed plugin file has no parent directory"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    temporary
        .as_file_mut()
        .set_permissions(unix_permissions(if executable { 0o700 } else { 0o600 }))?;
    temporary.write_all(bytes)?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn atomic_write_private_at(
    parent: &File,
    name: &str,
    path: &Path,
    bytes: &[u8],
    executable: bool,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;

    let target_name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| invalid_data("managed plugin file name contains NUL"))?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    let prior = openat_unix_cstr(parent, &target_name, flags)?;
    if let Some(prior) = prior.as_ref() {
        validate_owned_regular_file(path, &prior.metadata()?, true)?;
    }

    let mode = if executable { 0o700 } else { 0o600 };
    let (temporary_name, mut temporary) = {
        let mut created = None;
        for _ in 0..32 {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(io::Error::other)?;
            let name = format!(".abtop-write-{}", hex(&random));
            match openat_create_unix(
                parent,
                std::ffi::OsStr::new(&name),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode,
            ) {
                Ok(file) => {
                    created = Some((name, file));
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        created.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate a private managed-plugin temporary file",
            )
        })?
    };
    let temporary_name_c = std::ffi::CString::new(temporary_name.as_bytes())
        .map_err(|_| invalid_data("managed plugin temporary file name contains NUL"))?;
    let mut renamed = false;
    let outcome = (|| {
        if unsafe { libc::fchmod(temporary.as_raw_fd(), mode) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let temporary_metadata = temporary.metadata()?;
        validate_owned_regular_file(path, &temporary_metadata, true)?;
        if u64::from(temporary_metadata.permissions().mode() & 0o777) != u64::from(mode) {
            return Err(invalid_data(
                "managed plugin temporary file has an unexpected mode",
            ));
        }
        temporary.write_all(bytes)?;
        temporary.sync_all()?;
        let prepared_metadata = temporary.metadata()?;
        if prepared_metadata.len() != bytes.len() as u64
            || !same_file_content_snapshot(&prepared_metadata, &temporary.metadata()?)
        {
            return Err(invalid_data(
                "managed plugin temporary file changed before installation",
            ));
        }

        let current = openat_unix_cstr(parent, &target_name, flags)?;
        match (prior.as_ref(), current.as_ref()) {
            (None, None) => {}
            (Some(prior), Some(current))
                if same_file_content_snapshot(&prior.metadata()?, &current.metadata()?) => {}
            _ => {
                return Err(invalid_data(format!(
                    "managed plugin file {} changed at the replacement boundary",
                    path.display()
                )));
            }
        }
        if unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                temporary_name_c.as_ptr(),
                parent.as_raw_fd(),
                target_name.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        renamed = true;
        let installed = open_matching_managed_file_at(parent, name, path, bytes, executable, true)?
            .ok_or_else(|| {
                invalid_data(format!(
                    "managed plugin file {} did not validate after installation",
                    path.display()
                ))
            })?;
        if !same_file_metadata(&prepared_metadata, &installed.metadata()?) {
            return Err(invalid_data(format!(
                "managed plugin file {} was replaced immediately after installation",
                path.display()
            )));
        }
        parent.sync_all()?;
        Ok(())
    })();
    if !renamed && unsafe { libc::unlinkat(parent.as_raw_fd(), temporary_name_c.as_ptr(), 0) } != 0
    {
        let cleanup_error = io::Error::last_os_error();
        if outcome.is_ok() {
            return Err(cleanup_error);
        }
    }
    outcome
}

#[cfg(not(unix))]
fn private_regular_file_matches(
    path: &Path,
    expected: &[u8],
    _executable: bool,
) -> io::Result<bool> {
    let Some(bytes) = read_private_regular_file(path)? else {
        return Ok(false);
    };
    if bytes != expected {
        return Ok(false);
    }
    Ok(true)
}

#[cfg(not(unix))]
fn private_regular_mode_matches(path: &Path, _executable: bool) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    validate_owned_regular_file(path, &metadata, true)?;
    Ok(true)
}

#[cfg(not(unix))]
fn read_private_regular_file(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid_data(format!(
            "managed plugin path {} is not a regular file",
            path.display()
        )));
    }
    if metadata.len() > MAX_MANAGED_FILE {
        return Err(invalid_data(format!(
            "managed plugin file {} is oversized",
            path.display()
        )));
    }
    validate_same_owner(&metadata, path)?;
    validate_single_link(&metadata, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(invalid_data(format!(
                "managed plugin file {} is accessible by another user",
                path.display()
            )));
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let opened = file.metadata()?;
    if !same_file_metadata(&metadata, &opened) {
        return Err(invalid_data(format!(
            "managed plugin file {} changed while it was opened",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take(MAX_MANAGED_FILE + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANAGED_FILE {
        return Err(invalid_data("managed plugin file is oversized"));
    }
    let after = fs::symlink_metadata(path)?;
    if !same_file_metadata(&metadata, &after) {
        return Err(invalid_data(format!(
            "managed plugin file {} changed while it was inspected",
            path.display()
        )));
    }
    Ok(Some(bytes))
}

fn executable_path_digest(path: &Path) -> io::Result<String> {
    let lexical_before = fs::symlink_metadata(path)?;
    let canonical = fs::canonicalize(path)?;
    validate_executable_path(&canonical, "native executable target")?;
    let target_digest = hash_file(&canonical)?;
    let lexical_after = fs::symlink_metadata(path)?;
    let canonical_after = fs::canonicalize(path)?;
    if !same_file_content_snapshot(&lexical_before, &lexical_after) || canonical_after != canonical
    {
        return Err(invalid_data(
            "native executable path changed while its identity was inspected",
        ));
    }
    let mut digest = Sha256::new();
    digest.update(path.as_os_str().as_encoded_bytes());
    digest.update([0]);
    digest.update(canonical.as_os_str().as_encoded_bytes());
    digest.update([0]);
    digest.update(target_digest.as_bytes());
    Ok(format!("sha256:{}", hex(&digest.finalize())))
}

fn hash_file(path: &Path) -> io::Result<String> {
    let path_metadata = fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(invalid_data(
            "helper or native executable path is not a direct regular file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || !same_file_content_snapshot(&path_metadata, &metadata) {
        return Err(invalid_data(
            "helper or native executable changed while it was opened",
        ));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let descriptor_after = file.metadata()?;
    let path_after = fs::symlink_metadata(path)?;
    if !same_file_content_snapshot(&metadata, &descriptor_after)
        || !same_file_content_snapshot(&metadata, &path_after)
    {
        return Err(invalid_data(
            "helper or native executable changed while it was hashed",
        ));
    }
    Ok(format!("sha256:{}", hex(&hasher.finalize())))
}

fn run_mutating_codex(
    binary: &Path,
    expected_digest: &str,
    codex_home: &Path,
    args: &[OsString],
) -> io::Result<CommandOutput> {
    ensure_codex_executable_identity(binary, expected_digest)?;
    let result = run_codex(binary, codex_home, args);
    let closing_identity = ensure_codex_executable_identity(binary, expected_digest);
    match (result, closing_identity) {
        (_, Err(identity_error)) => Err(invalid_data(format!(
            "the native Codex executable changed during a mutating plugin command: {identity_error}"
        ))),
        (result, Ok(())) => result,
    }
}

fn ensure_codex_executable_identity(binary: &Path, expected_digest: &str) -> io::Result<()> {
    let observed = executable_path_digest(binary)?;
    if observed == expected_digest {
        Ok(())
    } else {
        Err(invalid_data(
            "the selected lexical Codex executable path, target, or contents changed",
        ))
    }
}

fn run_codex(binary: &Path, codex_home: &Path, args: &[OsString]) -> io::Result<CommandOutput> {
    run_codex_with_timeout(binary, codex_home, args, COMMAND_TIMEOUT)
}

#[cfg(unix)]
fn run_codex_with_timeout(
    binary: &Path,
    codex_home: &Path,
    args: &[OsString],
    timeout: Duration,
) -> io::Result<CommandOutput> {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new(binary);
    command
        .args(args)
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing stderr pipe"))?;
    if let Err(error) = set_nonblocking(&stdout).and_then(|()| set_nonblocking(&stderr)) {
        terminate_unix_process_group(&mut child);
        reap_child_bounded(child, Instant::now() + INHERITED_PIPE_GRACE);
        return Err(error);
    }

    let mut stdout = stdout;
    let mut stderr = stderr;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_overflow = false;
    let mut stderr_overflow = false;
    let mut stdout_closed = false;
    let mut stderr_closed = false;
    let mut leader_exited = false;
    let mut leader_exited_at = None;
    let mut descendants_terminated_at = None;
    let deadline = Instant::now() + timeout;

    loop {
        if !stdout_closed {
            let drained =
                drain_nonblocking_pipe(&mut stdout, &mut stdout_bytes, &mut stdout_overflow);
            match drained {
                Ok(closed) => stdout_closed = closed,
                Err(error) => {
                    terminate_unix_process_group(&mut child);
                    reap_child_bounded(child, Instant::now() + INHERITED_PIPE_GRACE);
                    return Err(error);
                }
            }
        }
        if !stderr_closed {
            let drained =
                drain_nonblocking_pipe(&mut stderr, &mut stderr_bytes, &mut stderr_overflow);
            match drained {
                Ok(closed) => stderr_closed = closed,
                Err(error) => {
                    terminate_unix_process_group(&mut child);
                    reap_child_bounded(child, Instant::now() + INHERITED_PIPE_GRACE);
                    return Err(error);
                }
            }
        }
        if !leader_exited {
            match child_exited_without_reaping(child.id()) {
                Ok(exited) => {
                    leader_exited = exited;
                    if exited {
                        leader_exited_at = Some(Instant::now());
                    }
                }
                Err(error) => {
                    terminate_unix_process_group(&mut child);
                    reap_child_bounded(child, Instant::now() + INHERITED_PIPE_GRACE);
                    return Err(error);
                }
            }
        }
        if leader_exited && stdout_closed && stderr_closed {
            // `waitid(..., WNOWAIT)` deliberately kept the leader as a zombie
            // until all group cleanup was complete. Its PID/PGID therefore
            // could not be reused by an unrelated process before a group kill.
            let status = child.wait()?;
            return Ok(CommandOutput {
                status,
                stdout: stdout_bytes,
                stderr: stderr_bytes,
                overflowed: stdout_overflow || stderr_overflow,
            });
        }

        let now = Instant::now();
        if !leader_exited && now >= deadline {
            terminate_unix_process_group(&mut child);
            reap_child_bounded(child, now + INHERITED_PIPE_GRACE);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "native Codex plugin command timed out",
            ));
        }
        if leader_exited_at.is_some_and(|exited| now.duration_since(exited) >= INHERITED_PIPE_GRACE)
            && descendants_terminated_at.is_none()
        {
            // A descendant inherited one of the pipes after the native command
            // exited. Kill the isolated command process group so pipe readers
            // can never block setup or uninstall indefinitely.
            terminate_unix_process_group(&mut child);
            descendants_terminated_at = Some(now);
        }
        if descendants_terminated_at
            .is_some_and(|terminated| now.duration_since(terminated) >= INHERITED_PIPE_GRACE)
        {
            reap_child_bounded(child, now + INHERITED_PIPE_GRACE);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "native Codex plugin command left inherited output pipes open",
            ));
        }
        std::thread::sleep(COMMAND_POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn child_exited_without_reaping(pid: u32) -> io::Result<bool> {
    let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            information.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let information = unsafe { information.assume_init() };
    Ok(unsafe { information.si_pid() } != 0)
}

#[cfg(unix)]
fn set_nonblocking(pipe: &impl std::os::fd::AsRawFd) -> io::Result<()> {
    let descriptor = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn drain_nonblocking_pipe(
    pipe: &mut impl Read,
    output: &mut Vec<u8>,
    overflow: &mut bool,
) -> io::Result<bool> {
    const DRAIN_BUDGET: usize = 64 * 1024;

    let mut drained = 0;
    let mut buffer = [0_u8; 8192];
    while drained < DRAIN_BUDGET {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                drained += read;
                let remaining = MAX_COMMAND_OUTPUT.saturating_sub(output.len());
                let keep = remaining.min(read);
                output.extend_from_slice(&buffer[..keep]);
                *overflow |= keep != read;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

#[cfg(unix)]
fn terminate_unix_process_group(child: &mut Child) {
    let process_group = -(child.id() as libc::pid_t);
    let result = unsafe { libc::kill(process_group, libc::SIGKILL) };
    if result != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
        let _ = child.kill();
    } else {
        // `kill(-pgid, ...)` can report ESRCH when the leader is the only
        // process and has already exited. The direct kill is a harmless
        // fallback for the remaining leader incarnation.
        let _ = child.kill();
    }
}

#[cfg(unix)]
fn reap_child_bounded(mut child: Child, deadline: Instant) {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(COMMAND_POLL_INTERVAL);
            }
            Ok(None) | Err(_) => break,
        }
    }
    // Reaping is detached only after SIGKILL and a bounded grace period. The
    // administrative call remains bounded even on a broken platform wait.
    let _ = std::thread::Builder::new()
        .name("abtop-codex-reaper".to_string())
        .spawn(move || {
            let _ = child.wait();
        });
}

#[cfg(not(unix))]
fn run_codex_with_timeout(
    binary: &Path,
    codex_home: &Path,
    args: &[OsString],
    timeout: Duration,
) -> io::Result<CommandOutput> {
    let mut child = Command::new(binary)
        .args(args)
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    #[cfg(windows)]
    let job = WindowsCommandJob::assign(&child);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing stderr pipe"))?;
    let (stdout_sender, stdout_receiver) = mpsc::sync_channel(1);
    let (stderr_sender, stderr_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = stdout_sender.send(read_command_pipe(stdout));
    });
    thread::spawn(move || {
        let _ = stderr_sender.send(read_command_pipe(stderr));
    });

    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    let mut leader_exited_at = None;
    let deadline = Instant::now() + timeout;
    loop {
        if status.is_none() {
            status = child.try_wait()?;
            if status.is_some() {
                leader_exited_at = Some(Instant::now());
            }
        }
        if stdout.is_none() {
            match stdout_receiver.try_recv() {
                Ok(value) => stdout = Some(value?),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(io::Error::other("Codex stdout reader failed"));
                }
            }
        }
        if stderr.is_none() {
            match stderr_receiver.try_recv() {
                Ok(value) => stderr = Some(value?),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(io::Error::other("Codex stderr reader failed"));
                }
            }
        }
        if status.is_some() && stdout.is_some() && stderr.is_some() {
            return match (status, stdout, stderr) {
                (
                    Some(status),
                    Some((stdout, stdout_overflow)),
                    Some((stderr, stderr_overflow)),
                ) => Ok(CommandOutput {
                    status,
                    stdout,
                    stderr,
                    overflowed: stdout_overflow || stderr_overflow,
                }),
                _ => Err(io::Error::other(
                    "Codex command result changed while being collected",
                )),
            };
        }

        let now = Instant::now();
        let inherited_pipe_timeout = leader_exited_at
            .is_some_and(|exited| now.duration_since(exited) >= INHERITED_PIPE_GRACE);
        if now >= deadline || inherited_pipe_timeout {
            #[cfg(windows)]
            if let Some(job) = job.as_ref() {
                job.terminate();
            }
            let _ = child.kill();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                if inherited_pipe_timeout {
                    "native Codex plugin command left inherited output pipes open"
                } else {
                    "native Codex plugin command timed out"
                },
            ));
        }
        thread::sleep(COMMAND_POLL_INTERVAL);
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsCommandJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl WindowsCommandJob {
    fn assign(child: &Child) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return None;
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } != 0;
        let assigned = configured
            && unsafe { AssignProcessToJobObject(handle, child.as_raw_handle() as _) } != 0;
        if !assigned {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }
            return None;
        }
        Some(Self { handle })
    }

    fn terminate(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.handle, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsCommandJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(not(unix))]
fn read_command_pipe(mut pipe: impl Read) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut overflow = false;
    loop {
        let read = pipe.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = MAX_COMMAND_OUTPUT.saturating_sub(output.len());
        let keep = remaining.min(read);
        output.extend_from_slice(&buffer[..keep]);
        overflow |= keep != read;
    }
    Ok((output, overflow))
}

fn require_success(output: &CommandOutput, action: &str) -> io::Result<()> {
    if output.status.success() && !output.overflowed {
        Ok(())
    } else {
        Err(command_failure(action, output))
    }
}

fn command_failure(action: &str, output: &CommandOutput) -> io::Error {
    let detail = sanitize_output(if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    });
    let suffix = if output.overflowed {
        "output exceeded the 1 MiB safety limit".to_string()
    } else if detail.is_empty() {
        format!("exit status {}", output.status)
    } else {
        detail
    };
    io::Error::other(format!("failed while {action}: {suffix}"))
}

fn with_cleanup_errors(primary: io::Error, cleanup: Vec<io::Error>) -> io::Error {
    if cleanup.is_empty() {
        return primary;
    }
    let details = cleanup
        .into_iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    io::Error::new(
        primary.kind(),
        format!("{primary}; cleanup was incomplete: {details}"),
    )
}

fn sanitize_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .take(2048)
        .collect::<String>()
        .trim()
        .to_string()
}

fn parse_json(bytes: &[u8], label: &str) -> io::Result<Value> {
    serde_json::from_slice(bytes)
        .map_err(|error| invalid_data(format!("Codex {label} returned invalid JSON: {error}")))
}

fn require_json_object(bytes: &[u8], label: &str) -> io::Result<()> {
    if parse_json(bytes, label)?.is_object() {
        Ok(())
    } else {
        Err(invalid_data(format!(
            "Codex {label} did not return a JSON object"
        )))
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    let left = fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

#[cfg(unix)]
fn same_file_metadata(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(unix)]
fn same_file_content_snapshot(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    same_file_metadata(left, right)
        && left.len() == right.len()
        && left.mode() == right.mode()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(not(unix))]
fn same_file_metadata(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

#[cfg(not(unix))]
fn same_file_content_snapshot(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_file_metadata(left, right)
}

fn pretty_json(value: &Value) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn quote_posix(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn quote_cmd_path(value: &str) -> io::Result<String> {
    validate_embedded_text(value, "abtop executable path")?;
    if value.contains('"') {
        return Err(invalid_data(
            "the abtop executable path cannot contain a double quote on Windows",
        ));
    }
    Ok(format!("\"{}\"", value.replace('%', "%%")))
}

fn escape_cmd_set_value(value: &str) -> io::Result<String> {
    validate_embedded_text(value, "Codex hook fault directory")?;
    if value.contains('"') {
        return Err(invalid_data(
            "the Codex hook fault directory cannot contain a double quote on Windows",
        ));
    }
    Ok(value.replace('%', "%%"))
}

fn validate_embedded_text(value: &str, label: &str) -> io::Result<()> {
    if value
        .chars()
        .any(|character| character == '\0' || matches!(character, '\r' | '\n'))
    {
        return Err(invalid_data(format!(
            "{label} contains an unsafe character"
        )));
    }
    Ok(())
}

fn hook_event_key(event: &str) -> &'static str {
    match event {
        "PreToolUse" => "pre_tool_use",
        "PermissionRequest" => "permission_request",
        "PostToolUse" => "post_tool_use",
        "PreCompact" => "pre_compact",
        "PostCompact" => "post_compact",
        "SessionStart" => "session_start",
        "SessionEnd" => "session_end",
        "UserPromptSubmit" => "user_prompt_submit",
        "SubagentStart" => "subagent_start",
        "SubagentStop" => "subagent_stop",
        "Stop" => "stop",
        _ => unreachable!("HOOK_EVENTS contains only known events"),
    }
}

fn normalize_absolute(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(invalid_data("path must be absolute"));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
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

#[cfg(unix)]
fn unix_permissions(mode: u32) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(mode)
}

#[cfg(unix)]
fn validate_same_owner(metadata: &fs::Metadata, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(invalid_data(format!(
            "{} is not owned by the current user",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_same_owner(_metadata: &fs::Metadata, _path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn validate_single_link(metadata: &fs::Metadata, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() != 1 {
        return Err(invalid_data(format!(
            "managed plugin file {} has multiple hard links",
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
            "{} is writable by another user",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_not_other_writable(_metadata: &fs::Metadata, _path: &Path) -> io::Result<()> {
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn executable_file(path: &Path, contents: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    fn fixture_bundle() -> (tempfile::TempDir, PluginPaths, RenderedBundle) {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        fs::create_dir(&codex_home).unwrap();
        let codex_home = fs::canonicalize(codex_home).unwrap();
        let helper = temp.path().join("abtop");
        executable_file(&helper, b"helper bytes");
        let paths = PluginPaths::new(&codex_home).unwrap();
        let bundle = render_bundle(&helper, &paths.plugin_data_root).unwrap();
        (temp, paths, bundle)
    }

    #[cfg(unix)]
    fn rendered_test_bundle(helper: &Path) -> RenderedBundle {
        render_bundle(helper, &helper.parent().unwrap().join("abtop-abtop-local")).unwrap()
    }

    #[test]
    #[cfg(unix)]
    fn codex_runner_kills_a_descendant_that_inherits_output_pipes() {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex-home");
        fs::create_dir(&codex_home).unwrap();
        let binary = temp.path().join("codex-fake");
        executable_file(
            &binary,
            b"#!/bin/sh\nsleep 30 &\nprintf '%s\\n' \"$!\" > \"$CODEX_HOME/descendant.pid\"\nprintf '{}\\n'\nexit 0\n",
        );

        let started = Instant::now();
        let output =
            run_codex_with_timeout(&binary, &codex_home, &[], Duration::from_secs(15)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"{}\n");
        assert!(started.elapsed() < Duration::from_secs(20));

        let pid = fs::read_to_string(codex_home.join("descendant.pid"))
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
            std::thread::sleep(COMMAND_POLL_INTERVAL);
        }
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
    }

    #[test]
    #[cfg(unix)]
    fn codex_runner_timeout_kills_the_whole_process_group_without_blocking() {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex-home");
        fs::create_dir(&codex_home).unwrap();
        let binary = temp.path().join("codex-fake");
        executable_file(
            &binary,
            b"#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$CODEX_HOME/leader.pid\"\nsleep 30 &\nprintf '%s\\n' \"$!\" > \"$CODEX_HOME/descendant.pid\"\nwait\n",
        );

        let started = Instant::now();
        let error =
            run_codex_with_timeout(&binary, &codex_home, &[], Duration::from_secs(15)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(20));
        for name in ["leader.pid", "descendant.pid"] {
            let pid = fs::read_to_string(codex_home.join(name))
                .unwrap_or_else(|error| {
                    panic!("timeout fixture did not record {name} before termination: {error}")
                })
                .trim()
                .parse::<libc::pid_t>()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(1);
            while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
                std::thread::sleep(COMMAND_POLL_INTERVAL);
            }
            assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "{name} survived");
        }
    }

    #[test]
    fn windows_pathext_order_includes_com_and_deduplicates_case_insensitively() {
        let names = windows_codex_executable_names_from_text(Some(".COM;.EXE;.com;.CMD"));
        assert_eq!(
            names,
            vec![
                OsString::from("codex.COM"),
                OsString::from("codex.EXE"),
                OsString::from("codex.CMD")
            ]
        );
        assert_eq!(
            windows_codex_executable_names(None).first(),
            Some(&OsString::from("codex.COM"))
        );
    }

    #[test]
    #[cfg(unix)]
    fn path_resolution_preserves_pathext_precedence_and_the_lexical_entry() {
        let temp = tempfile::tempdir().unwrap();
        let path_root = temp.path().join("path-entry");
        fs::create_dir(&path_root).unwrap();
        executable_file(&path_root.join("codex.COM"), b"com");
        executable_file(&path_root.join("codex.EXE"), b"exe");
        let names = windows_codex_executable_names_from_text(Some(".COM;.EXE"));
        let path = std::env::join_paths([&path_root]).unwrap();
        let resolved = resolve_codex_binary_in_path(&path, temp.path(), &names).unwrap();
        assert_eq!(resolved, path_root.join("codex.COM"));
    }

    #[test]
    #[cfg(unix)]
    fn stable_setup_lock_is_private_and_reuses_one_inode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex-home");
        fs::create_dir(&codex_home).unwrap();
        let first = SetupLock::acquire(&codex_home).unwrap();
        first.revalidate().unwrap();
        let lock_path = codex_home.join(SETUP_LOCK_FILE);
        let first_metadata = fs::symlink_metadata(&lock_path).unwrap();
        assert_eq!(first_metadata.permissions().mode() & 0o777, 0o600);
        drop(first);
        let second = SetupLock::acquire(&codex_home).unwrap();
        second.revalidate().unwrap();
        let second_metadata = fs::symlink_metadata(lock_path).unwrap();
        assert_eq!(first_metadata.dev(), second_metadata.dev());
        assert_eq!(first_metadata.ino(), second_metadata.ino());
    }

    #[test]
    #[cfg(unix)]
    fn identity_preflight_rejects_an_atomic_executable_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex-home");
        fs::create_dir(&codex_home).unwrap();
        let binary = temp.path().join("codex-fake");
        let replacement = codex_home.join("replacement");
        executable_file(
            &binary,
            b"#!/bin/sh\nif [ \"$1\" = '--version' ]; then mv \"$CODEX_HOME/replacement\" \"$0\"; printf 'codex-cli 0.146.0\\n'; exit 0; fi\nexit 47\n",
        );
        executable_file(&replacement, b"#!/bin/sh\nprintf 'codex-cli 0.146.0\\n'\n");

        let error = capture_codex_binary_identity(&binary, &codex_home).unwrap_err();
        assert!(error
            .to_string()
            .contains("changed during identity preflight"));
    }

    #[test]
    #[cfg(unix)]
    fn mutating_command_rejects_an_atomic_executable_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex-home");
        fs::create_dir(&codex_home).unwrap();
        let binary = temp.path().join("codex-fake");
        let replacement = codex_home.join("replacement");
        executable_file(
            &binary,
            b"#!/bin/sh\nmv \"$CODEX_HOME/replacement\" \"$0\"\nprintf '{}\\n'\n",
        );
        executable_file(&replacement, b"#!/bin/sh\nprintf '{}\\n'\n");
        let digest = executable_path_digest(&binary).unwrap();

        let error = run_mutating_codex(&binary, &digest, &codex_home, &[]).unwrap_err();
        assert!(error
            .to_string()
            .contains("changed during a mutating plugin command"));
    }

    #[cfg(unix)]
    fn write_cached_bundle(paths: &PluginPaths, bundle: &RenderedBundle) {
        let version_root = cache_version_path(paths, &bundle.plugin_version);
        for directory in [
            version_root.clone(),
            version_root.join(".codex-plugin"),
            version_root.join("hooks"),
            version_root.join("scripts"),
        ] {
            fs::create_dir_all(directory).unwrap();
        }
        atomic_write_private(
            &version_root.join(".codex-plugin/plugin.json"),
            &bundle.plugin_manifest,
            false,
        )
        .unwrap();
        atomic_write_private(
            &version_root.join("hooks/hooks.json"),
            &bundle.hooks_manifest,
            false,
        )
        .unwrap();
        atomic_write_private(
            &version_root.join("scripts/abtop-codex-hook.sh"),
            &bundle.posix_launcher,
            true,
        )
        .unwrap();
        atomic_write_private(
            &version_root.join("scripts/abtop-codex-hook.cmd"),
            &bundle.windows_launcher,
            false,
        )
        .unwrap();
    }

    #[cfg(unix)]
    fn write_trusted_base_hook_config(paths: &PluginPaths, bundle: &RenderedBundle) {
        let mut states = toml::map::Map::new();
        for identity in &bundle.hook_commands {
            let key = format!("{PLUGIN_ID}:hooks/hooks.json:{}:0:0", identity.event_key);
            let mut state = toml::map::Map::new();
            state.insert("enabled".to_string(), toml::Value::Boolean(true));
            state.insert(
                "trusted_hash".to_string(),
                toml::Value::String(expected_trust_hash(identity)),
            );
            states.insert(key, toml::Value::Table(state));
        }
        let mut hooks = toml::map::Map::new();
        hooks.insert("state".to_string(), toml::Value::Table(states));
        let mut root = toml::map::Map::new();
        root.insert("hooks".to_string(), toml::Value::Table(hooks));
        let mut plugin = toml::map::Map::new();
        plugin.insert("enabled".to_string(), toml::Value::Boolean(true));
        let mut plugins = toml::map::Map::new();
        plugins.insert(PLUGIN_ID.to_string(), toml::Value::Table(plugin));
        root.insert("plugins".to_string(), toml::Value::Table(plugins));
        let config = toml::to_string(&toml::Value::Table(root)).unwrap();
        atomic_write_private(
            &paths.codex_home.join("config.toml"),
            config.as_bytes(),
            false,
        )
        .unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn bundle_declares_all_supported_hooks_without_matchers() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("abtop");
        executable_file(&helper, b"helper bytes");
        let bundle = rendered_test_bundle(&helper);
        let value: Value = serde_json::from_slice(&bundle.hooks_manifest).unwrap();
        let hooks = value["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), HOOK_EVENTS.len());
        for event in HOOK_EVENTS {
            let group = &hooks[event][0];
            assert!(group.get("matcher").is_none());
            let handler = &group["hooks"][0];
            assert_eq!(handler["timeout"], 1);
            assert!(handler.get("async").is_none());
            assert!(handler["command"]
                .as_str()
                .unwrap()
                .contains(HOOK_SCHEMA_REVISION));
            assert!(handler["command"]
                .as_str()
                .unwrap()
                .contains(&bundle.helper_digest));
            assert!(handler["command"].as_str().unwrap().starts_with("exec "));
        }
        assert!(hooks.get("PostToolUseFailure").is_none());
    }

    #[test]
    #[cfg(unix)]
    fn manifest_omits_the_unsupported_hooks_field() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("abtop");
        executable_file(&helper, b"helper bytes");
        let bundle = rendered_test_bundle(&helper);
        let manifest: Value = serde_json::from_slice(&bundle.plugin_manifest).unwrap();
        assert_eq!(manifest["name"], PLUGIN_NAME);
        assert!(manifest.get("hooks").is_none());
        assert!(manifest["version"].as_str().unwrap().contains("+codex."));
    }

    #[test]
    #[cfg(unix)]
    fn launcher_is_silent_and_invokes_the_exact_helper() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("abtop executable");
        executable_file(&helper, b"helper bytes");
        let bundle = rendered_test_bundle(&helper);
        let script = String::from_utf8(bundle.posix_launcher).unwrap();
        assert!(script.contains("'/"));
        assert!(!script.contains("\nexec '/"));
        assert!(script.contains("--codex-hook-ingest"));
        assert!(script.contains(">/dev/null 2>&1"));
        assert!(script.contains("set -C"));
        assert!(script.contains("mktemp \"$abtop_fault_dir/launch-$$-pending.XXXXXXXXXXXXXXXX\""));
        assert!(script.contains(HOOK_FAULT_TOKEN_ENV));
        assert!(script.contains("/states/faults"));
        assert!(script.contains("launch-$abtop_fault_slot-abtopv1.pending"));
        assert!(script.contains("overflow.json"));
        let expected_fault_directory = helper
            .parent()
            .unwrap()
            .join("abtop-abtop-local/states/faults");
        assert!(script.contains(expected_fault_directory.to_string_lossy().as_ref()));
        assert!(!script.contains("${PLUGIN_DATA"));
        assert!(!script.contains("echo "));
    }

    #[test]
    #[cfg(unix)]
    fn posix_launcher_absorbs_helper_failure_and_output() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("abtop-helper");
        executable_file(
            &helper,
            b"#!/bin/sh\nprintf 'sensitive stdout\\n'\nprintf 'sensitive stderr\\n' >&2\nexit 37\n",
        );
        let bundle = rendered_test_bundle(&helper);
        let launcher = temp.path().join("launcher.sh");
        executable_file(&launcher, &bundle.posix_launcher);
        fs::create_dir_all(temp.path().join("abtop-abtop-local/states/faults")).unwrap();

        let output = Command::new(&launcher)
            .args([
                "--schema-revision",
                HOOK_SCHEMA_REVISION,
                "--helper-digest",
                &bundle.helper_digest,
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn posix_launcher_marks_the_embedded_private_directory_without_provider_env() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("abtop-helper");
        executable_file(
            &helper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"${{{HOOK_FAULT_TOKEN_ENV}-}}\" > \"$ABTOP_TEST_TOKEN_OUTPUT\"\ntest -f \"$ABTOP_TEST_FAULT_DIR/${{{HOOK_FAULT_TOKEN_ENV}}}\"\n"
            )
            .as_bytes(),
        );
        let bundle = rendered_test_bundle(&helper);
        let launcher = temp.path().join("launcher.sh");
        fs::write(&launcher, &bundle.posix_launcher).unwrap();
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
        let plugin_data = temp.path().join("abtop-abtop-local");
        let faults = plugin_data
            .join(HOOK_STATE_DIR_NAME)
            .join(HOOK_FAULT_DIR_NAME);
        fs::create_dir_all(&faults).unwrap();
        fs::set_permissions(&plugin_data, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(
            plugin_data.join(HOOK_STATE_DIR_NAME),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::set_permissions(&faults, fs::Permissions::from_mode(0o700)).unwrap();
        let output = temp.path().join("token.txt");

        let status = Command::new(&launcher)
            .args([
                "--schema-revision",
                HOOK_SCHEMA_REVISION,
                "--helper-digest",
                &bundle.helper_digest,
            ])
            .env_remove("PLUGIN_DATA")
            .env_remove("CLAUDE_PLUGIN_DATA")
            .env("ABTOP_TEST_FAULT_DIR", &faults)
            .env("ABTOP_TEST_TOKEN_OUTPUT", &output)
            .status()
            .unwrap();
        assert!(status.success());
        let token = fs::read_to_string(&output).unwrap();
        let token = token.trim();
        let body = token.strip_prefix("launch-").unwrap();
        let (pid, nonce) = body.split_once("-pending.").unwrap();
        assert!(!pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()));
        assert_eq!(nonce.len(), 16);
        assert!(nonce.bytes().all(|byte| byte.is_ascii_alphanumeric()));
        let marker = faults.join(token);
        let metadata = fs::symlink_metadata(marker).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.len(), 0);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    #[cfg(unix)]
    fn posix_launcher_never_forwards_an_inherited_fault_token() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("abtop-helper");
        executable_file(
            &helper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"${{{HOOK_FAULT_TOKEN_ENV}-}}\" > \"$ABTOP_TEST_TOKEN_OUTPUT\"\n"
            )
            .as_bytes(),
        );
        let bundle = rendered_test_bundle(&helper);
        let launcher = temp.path().join("launcher.sh");
        executable_file(&launcher, &bundle.posix_launcher);
        let plugin_data = temp.path().join("abtop-abtop-local");
        fs::create_dir(&plugin_data).unwrap();
        let output = temp.path().join("token.txt");

        let status = Command::new(&launcher)
            .args([
                "--schema-revision",
                HOOK_SCHEMA_REVISION,
                "--helper-digest",
                &bundle.helper_digest,
            ])
            .env("PLUGIN_DATA", &plugin_data)
            .env(HOOK_FAULT_TOKEN_ENV, "launch-1-inherited.pending")
            .env("ABTOP_TEST_TOKEN_OUTPUT", &output)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(fs::read_to_string(output).unwrap(), "\n");
    }

    #[test]
    #[cfg(unix)]
    fn posix_launcher_never_truncates_colliding_pending_markers() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("abtop-helper");
        executable_file(
            &helper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"${{{HOOK_FAULT_TOKEN_ENV}-}}\" > \"$ABTOP_TEST_TOKEN_OUTPUT\"\n"
            )
            .as_bytes(),
        );
        let bundle = rendered_test_bundle(&helper);
        let launcher = temp.path().join("launcher.sh");
        executable_file(&launcher, &bundle.posix_launcher);
        let plugin_data = temp.path().join("abtop-abtop-local");
        let faults = plugin_data
            .join(HOOK_STATE_DIR_NAME)
            .join(HOOK_FAULT_DIR_NAME);
        fs::create_dir_all(&faults).unwrap();
        let output = temp.path().join("token.txt");
        let wrapper = format!(
            "mktemp() {{ return 1; }}\ni=0\nwhile [ \"$i\" -lt 16 ]; do\n  printf sentinel > \"$PLUGIN_DATA/{HOOK_STATE_DIR_NAME}/{HOOK_FAULT_DIR_NAME}/launch-$i-abtopv1.pending\"\n  i=$((i + 1))\ndone\nset -- --schema-revision '{HOOK_SCHEMA_REVISION}' --helper-digest '{}'\n. \"$ABTOP_TEST_LAUNCHER\"\n",
            bundle.helper_digest
        );

        let status = Command::new("sh")
            .args(["-c", &wrapper])
            .env("PLUGIN_DATA", &plugin_data)
            .env("ABTOP_TEST_LAUNCHER", &launcher)
            .env("ABTOP_TEST_TOKEN_OUTPUT", &output)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(fs::read_to_string(output).unwrap(), "\n");
        let entries = fs::read_dir(faults)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 17);
        for entry in entries {
            if entry.file_name() == "overflow.json" {
                assert!(fs::read(entry.path()).unwrap().is_empty());
            } else {
                assert_eq!(fs::read(entry.path()).unwrap(), b"sentinel");
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn setup_rejects_an_unaudited_codex_minor_before_writing_the_bundle() {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        fs::create_dir(&codex_home).unwrap();
        let codex = temp.path().join("codex-future");
        executable_file(&codex, b"#!/bin/sh\nprintf 'codex-cli 0.147.0\\n'\n");
        let helper = temp.path().join("abtop");
        executable_file(&helper, b"helper bytes");

        let error = install_after_legacy_cleanup(&codex_home, &codex, &helper).unwrap_err();
        assert!(error.to_string().contains("requires codex-cli 0.146.0"));
        assert!(!codex_home.join("abtop").exists());
    }

    #[test]
    #[cfg(unix)]
    fn setup_preflight_rejects_incompatible_codex_before_legacy_profile_edits() {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        fs::create_dir(&codex_home).unwrap();
        let legacy_home = temp.path().join("legacy-home");
        fs::create_dir(&legacy_home).unwrap();
        let profile = legacy_home.join(".zshrc");
        let original = format!(
            "before\n{}\nfunction codex() {{ :; }}\n{}\nafter\n",
            migration::LEGACY_START_MARKER,
            migration::LEGACY_END_MARKER
        );
        fs::write(&profile, &original).unwrap();
        let codex = temp.path().join("codex-future");
        executable_file(&codex, b"#!/bin/sh\nprintf 'codex-cli 0.147.0\\n'\n");
        let helper = temp.path().join("abtop");
        executable_file(&helper, b"helper bytes");

        let error = setup_with_home(&codex_home, &codex, &helper, &legacy_home).unwrap_err();
        assert!(error.to_string().contains("requires codex-cli 0.146.0"));
        assert_eq!(fs::read_to_string(profile).unwrap(), original);
        assert!(!legacy_home.join(".abtop-codex-migration.lock").exists());
    }

    #[test]
    #[cfg(unix)]
    fn setup_preflight_invokes_a_multicall_codex_symlink_by_its_lexical_path() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex-home");
        fs::create_dir(&codex_home).unwrap();
        let mut properties = serde_json::Map::new();
        for event in HOOK_EVENTS {
            properties.insert(event.to_string(), json!({"type": "array"}));
        }
        let schema = serde_json::to_string(&json!({
            "definitions": {
                "ManagedHooksRequirements": { "properties": properties }
            }
        }))
        .unwrap();
        let multicall_target = temp.path().join("mise");
        executable_file(
            &multicall_target,
            format!(
                r##"#!/bin/sh
if [ "$(basename "$0")" != "codex" ]; then
  exit 91
fi
if [ "$1" = "--version" ]; then
  printf 'codex-cli 0.146.0\n'
  exit 0
fi
if [ "$1" = "features" ] && [ "$2" = "list" ]; then
  printf 'hooks stable true\nplugins stable true\n'
  exit 0
fi
if [ "$1" = "app-server" ] && [ "$2" = "generate-json-schema" ] && [ "$3" = "--out" ]; then
  mkdir -p "$4/v2"
  printf '%s\n' {} > "$4/v2/ConfigRequirementsReadResponse.json"
  exit 0
fi
exit 92
"##,
                quote_posix(&schema)
            )
            .as_bytes(),
        );
        let lexical_codex = temp.path().join("codex");
        symlink(&multicall_target, &lexical_codex).unwrap();
        let helper = temp.path().join("abtop");
        executable_file(&helper, b"helper bytes");

        let prepared = prepare_install(&codex_home, &lexical_codex, &helper).unwrap();

        assert_eq!(prepared.codex_binary, lexical_codex);
        assert_ne!(
            prepared.codex_binary,
            fs::canonicalize(&prepared.codex_binary).unwrap()
        );
    }

    #[test]
    #[cfg(not(unix))]
    fn unsupported_platform_fails_before_setup_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        let error = prepare_install(
            &codex_home,
            &temp.path().join("codex.exe"),
            &temp.path().join("abtop.exe"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(!codex_home.exists());
    }

    #[test]
    fn expected_hook_keys_are_stable() {
        let keys = HOOK_EVENTS.map(hook_event_key);
        assert_eq!(keys[0], "pre_tool_use");
        assert_eq!(keys[10], "stop");
    }

    #[test]
    fn base_runtime_config_rejects_disabled_features_plugins_and_config_locks() {
        let safe: toml::Value =
            toml::from_str(&format!("[plugins.\"{PLUGIN_ID}\"]\nenabled = true\n")).unwrap();
        assert!(base_runtime_hook_config_safe(&safe));

        for unsafe_config in [
            format!(
                "[plugins.\"{PLUGIN_ID}\"]\nenabled = false\n"
            ),
            format!(
                "[plugins.\"{PLUGIN_ID}\"]\nenabled = true\n[features]\nhooks = false\n"
            ),
            format!(
                "[plugins.\"{PLUGIN_ID}\"]\nenabled = true\n[features]\nplugins = false\n"
            ),
            format!(
                "[plugins.\"{PLUGIN_ID}\"]\nenabled = true\n[debug.config_lockfile]\nload_path = \"/private/lock.toml\"\n"
            ),
        ] {
            let config: toml::Value = toml::from_str(&unsafe_config).unwrap();
            assert!(!base_runtime_hook_config_safe(&config));
        }
    }

    #[test]
    fn codex_version_gate_accepts_only_exact_stable_semver_shape() {
        assert_eq!(
            parse_codex_cli_version(b"codex-cli 0.146.0\n").unwrap(),
            (0, 146, 0)
        );
        assert_eq!(
            parse_codex_cli_version(b"codex-cli 0.146.27").unwrap(),
            (0, 146, 27)
        );
        validate_supported_codex_release(b"codex-cli 0.146.0\n").unwrap();
        assert!(validate_supported_codex_release(b"codex-cli 0.146.1\n").is_err());
        assert!(validate_supported_codex_release(b"codex-cli 0.146.27\n").is_err());
        assert!(validate_supported_codex_release(b"codex-cli 0.145.9\n").is_err());
        assert!(validate_supported_codex_release(b"codex-cli 0.147.0\n").is_err());
        for invalid in [
            b"codex 0.146.0\n".as_slice(),
            b"codex-cli 0.146.0-beta\n",
            b"codex-cli 0.146\n",
            b"codex-cli 0.146.00\n",
            b"codex-cli 0.146.0\ntrailing\n",
        ] {
            assert!(parse_codex_cli_version(invalid).is_err());
        }
    }

    #[test]
    fn feature_preflight_requires_unique_stable_enabled_rows() {
        validate_required_feature_rows(
            b"foo experimental false\nhooks stable true\nplugins stable true\n",
        )
        .unwrap();
        for invalid in [
            b"hooks stable true\n".as_slice(),
            b"hooks experimental true\nplugins stable true\n",
            b"hooks stable false\nplugins stable true\n",
            b"hooks stable true extra\nplugins stable true\n",
            b"hooks stable true\nhooks stable true\nplugins stable true\n",
        ] {
            assert!(validate_required_feature_rows(invalid).is_err());
        }
    }

    #[test]
    fn generated_schema_must_advertise_exactly_the_supported_event_set() {
        let mut properties = serde_json::Map::new();
        for event in HOOK_EVENTS {
            properties.insert(event.to_string(), json!({"type": "array"}));
        }
        properties.insert("managedDir".to_string(), json!({"type": "string"}));
        properties.insert("windowsManagedDir".to_string(), json!({"type": "string"}));
        let schema = json!({
            "definitions": {
                "ManagedHooksRequirements": { "properties": properties }
            }
        });
        let exact = serde_json::to_vec(&schema).unwrap();
        validate_hook_schema_bytes(&exact).unwrap();

        let mut extra = schema.clone();
        extra["definitions"]["ManagedHooksRequirements"]["properties"]["PostToolUseFailure"] =
            json!({"type": "array"});
        assert!(validate_hook_schema_bytes(&serde_json::to_vec(&extra).unwrap()).is_err());

        let mut missing = schema;
        missing["definitions"]["ManagedHooksRequirements"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("SessionEnd");
        assert!(validate_hook_schema_bytes(&serde_json::to_vec(&missing).unwrap()).is_err());
    }

    #[test]
    fn canonical_hash_does_not_depend_on_object_insertion_order() {
        let left = json!({"b": 1, "a": {"d": 2, "c": 3}});
        let right = json!({"a": {"c": 3, "d": 2}, "b": 1});
        assert_eq!(
            hash_bytes(&serde_json::to_vec(&canonical_json(&left)).unwrap()),
            hash_bytes(&serde_json::to_vec(&canonical_json(&right)).unwrap())
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn trust_hash_matches_the_codex_normalized_identity_fixture() {
        let identity = HookCommandIdentity {
            event: "PreToolUse",
            event_key: "pre_tool_use",
            command: "echo fixed".to_string(),
            command_windows: "cmd.exe /D /C echo fixed".to_string(),
        };
        assert_eq!(
            expected_trust_hash(&identity),
            "sha256:418976fc656aa7eb68bea8445e29221077b6687c785ed66fbdbcd4c83cb934d7"
        );
    }

    #[test]
    #[cfg(unix)]
    fn source_tree_is_closed_and_rejects_unexpected_capabilities() {
        use std::os::unix::fs::PermissionsExt;

        let (_temp, paths, bundle) = fixture_bundle();
        write_bundle(&paths, &bundle).unwrap();
        assert!(audit_owned_source_tree(&paths, true).unwrap());
        for directory in [
            paths.plugin_data_root.clone(),
            paths.plugin_data_root.join(HOOK_STATE_DIR_NAME),
            paths
                .plugin_data_root
                .join(HOOK_STATE_DIR_NAME)
                .join(HOOK_FAULT_DIR_NAME),
        ] {
            let metadata = fs::symlink_metadata(directory).unwrap();
            assert!(metadata.is_dir());
            assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        }
        assert!(private_runtime_state_tree_valid(&paths).unwrap());

        let unexpected = paths.plugin_root.join("commands");
        fs::create_dir(&unexpected).unwrap();
        assert!(audit_owned_source_tree(&paths, true).is_err());
        assert!(remove_owned_bundle_files(&paths).is_err());
        assert!(unexpected.exists());
    }

    #[test]
    #[cfg(unix)]
    fn source_tree_rejects_a_symlinked_ancestor_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let (_temp, paths, _bundle) = fixture_bundle();
        ensure_private_dir(&paths.codex_home.join("abtop")).unwrap();
        let outside = paths.codex_home.join("outside");
        fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, b"keep").unwrap();
        symlink(&outside, &paths.marketplace_root).unwrap();

        assert!(audit_owned_source_tree(&paths, false).is_err());
        assert!(remove_owned_bundle_files(&paths).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
    }

    #[test]
    #[cfg(unix)]
    fn handle_relative_unlink_rejects_an_ancestor_swapped_to_a_symlink() {
        use std::os::unix::fs::symlink;

        let (_temp, paths, bundle) = fixture_bundle();
        write_bundle(&paths, &bundle).unwrap();
        let guard = OwnedSourceTreeGuard::open(&paths).unwrap();
        let scripts = paths.plugin_root.join("scripts");
        let displaced = paths.plugin_root.join("scripts-displaced");
        fs::rename(&scripts, &displaced).unwrap();
        let outside = paths.codex_home.join("outside-scripts");
        fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("abtop-codex-hook.sh");
        fs::write(&sentinel, b"outside sentinel").unwrap();
        symlink(&outside, &scripts).unwrap();

        assert!(guard
            .remove_file(Path::new(
                "marketplace/plugins/abtop/scripts/abtop-codex-hook.sh"
            ))
            .is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"outside sentinel");
        assert!(displaced.join("abtop-codex-hook.sh").exists());
    }

    #[test]
    #[cfg(unix)]
    fn cached_payload_must_be_the_exact_current_closed_bundle() {
        let (_temp, paths, bundle) = fixture_bundle();
        write_cached_bundle(&paths, &bundle);
        assert!(cached_bundle_matches_disk(&paths, &bundle).unwrap());

        let version_root = cache_version_path(&paths, &bundle.plugin_version);
        let unexpected = version_root.join("commands.md");
        atomic_write_private(&unexpected, b"unexpected", false).unwrap();
        assert!(cached_bundle_matches_disk(&paths, &bundle).is_err());
        fs::remove_file(&unexpected).unwrap();

        use std::os::unix::fs::PermissionsExt;
        let hooks = version_root.join("hooks/hooks.json");
        fs::set_permissions(&hooks, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!cached_bundle_matches_disk(&paths, &bundle).unwrap());
        fs::set_permissions(&hooks, fs::Permissions::from_mode(0o600)).unwrap();

        atomic_write_private(&hooks, b"{}\n", false).unwrap();
        assert!(!cached_bundle_matches_disk(&paths, &bundle).unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn runtime_hook_config_requires_the_exact_cached_bundle() {
        let (temp, paths, bundle) = fixture_bundle();
        write_bundle(&paths, &bundle).unwrap();
        write_attestation(&paths, &bundle).unwrap();
        write_cached_bundle(&paths, &bundle);
        write_trusted_base_hook_config(&paths, &bundle);

        let helper = temp.path().join("abtop");
        assert!(bundle_matches_disk(&paths, &bundle).unwrap());
        assert!(cached_bundle_matches_disk(&paths, &bundle).unwrap());
        assert!(attestation_matches(&paths, &bundle).unwrap());
        let config = read_base_config(&paths).unwrap();
        assert!(base_runtime_hook_config_safe(&config));
        let base = inspect_base_hook_state_from_config(&config, &bundle);
        assert_eq!(base.trusted, HOOK_EVENTS.len());
        assert_eq!(base.enabled, HOOK_EVENTS.len());
        let valid = runtime_hook_config(&paths.codex_home, &helper).unwrap();
        assert!(valid.complete_hook_set);

        let cached_hooks =
            cache_version_path(&paths, &bundle.plugin_version).join("hooks/hooks.json");
        atomic_write_private(&cached_hooks, b"{}\n", false).unwrap();
        let tampered = runtime_hook_config(&paths.codex_home, &helper).unwrap();
        assert!(!tampered.complete_hook_set);
        assert_eq!(tampered.config_digest, valid.config_digest);
    }

    #[test]
    #[cfg(unix)]
    fn static_config_inspection_ignores_unrelated_marketplace_snapshots() {
        let (_temp, paths, bundle) = fixture_bundle();
        write_cached_bundle(&paths, &bundle);
        let unrelated = paths.codex_home.join(".tmp/marketplaces/broken.json");
        fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        fs::write(&unrelated, b"not json").unwrap();
        let config = format!(
            "[marketplaces.{MARKETPLACE_NAME}]\nsource_type = \"local\"\nsource = {:?}\n\n[plugins.\"{PLUGIN_ID}\"]\nenabled = true\n",
            paths.marketplace_root.to_string_lossy()
        );
        atomic_write_private(
            &paths.codex_home.join("config.toml"),
            config.as_bytes(),
            false,
        )
        .unwrap();

        let state = inspect_config_state(&paths, Some(&bundle)).unwrap();
        assert!(state.marketplace_registered);
        assert!(state.plugin_configured);
        assert!(state.plugin_enabled);
        assert!(state.plugin_installed);
        assert_eq!(state.installed_version, Some(bundle.plugin_version.clone()));

        let conflict = format!(
            "[marketplaces.{MARKETPLACE_NAME}]\nsource_type = \"local\"\nsource = \"/different/source\"\n"
        );
        atomic_write_private(
            &paths.codex_home.join("config.toml"),
            conflict.as_bytes(),
            false,
        )
        .unwrap();
        let state = inspect_config_state(&paths, None).unwrap();
        assert!(!state.marketplace_registered);
        assert_eq!(
            state.marketplace_conflict,
            Some(PathBuf::from("/different/source"))
        );
    }

    #[test]
    #[cfg(unix)]
    fn uninstall_removes_the_plugin_but_preserves_an_unowned_malformed_marketplace() {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex");
        fs::create_dir(&codex_home).unwrap();
        fs::create_dir(codex_home.join("abtop")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(codex_home.join("abtop"), fs::Permissions::from_mode(0o700)).unwrap();
        atomic_write_private(
            &codex_home.join("config.toml"),
            b"[marketplaces.abtop-local]\nsource_type = \"local\"\n",
            false,
        )
        .unwrap();
        let codex = temp.path().join("codex-fake");
        executable_file(
            &codex,
            br##"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf 'codex-cli 0.147.0\n'
  exit 0
fi
printf '%s\n' "$*" >> "$CODEX_HOME/calls.log"
if [ "$1" = "plugin" ] && [ "$2" = "remove" ] && [ "$3" = "abtop@abtop-local" ]; then
  printf '{}\n'
  exit 0
fi
exit 47
"##,
        );

        let error = uninstall_after_legacy_cleanup(&codex_home, &codex).unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot prove ownership of malformed"));
        let calls = fs::read_to_string(codex_home.join("calls.log")).unwrap();
        assert_eq!(calls, "plugin remove abtop@abtop-local --json\n");
        assert!(!calls.contains("list"));
        assert!(codex_home.join("abtop").exists());
    }

    #[test]
    #[cfg(unix)]
    fn uninstall_recovers_from_a_malformed_owned_marketplace_snapshot() {
        let (temp, paths, bundle) = fixture_bundle();
        write_bundle(&paths, &bundle).unwrap();
        atomic_write_private(&paths.marketplace_manifest, b"malformed\n", false).unwrap();
        let config = format!(
            "[marketplaces.{MARKETPLACE_NAME}]\nsource_type = \"local\"\nsource = {:?}\n\n[plugins.\"{PLUGIN_ID}\"]\nenabled = true\n",
            paths.marketplace_root.to_string_lossy()
        );
        atomic_write_private(
            &paths.codex_home.join("config.toml"),
            config.as_bytes(),
            false,
        )
        .unwrap();
        let codex = temp.path().join("codex-fake");
        executable_file(
            &codex,
            br##"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf 'codex-cli 0.146.0\n'
  exit 0
fi
printf '%s\n' "$*" >> "$CODEX_HOME/calls.log"
if [ "$1" = "plugin" ] && [ "$2" = "remove" ]; then
  sed '/^\[plugins\./,$d' "$CODEX_HOME/config.toml" > "$CODEX_HOME/config.toml.next"
  mv "$CODEX_HOME/config.toml.next" "$CODEX_HOME/config.toml"
  printf '{}\n'
  exit 0
fi
if [ "$1" = "plugin" ] && [ "$2" = "marketplace" ] && [ "$3" = "remove" ]; then
  : > "$CODEX_HOME/config.toml"
  printf '{}\n'
  exit 0
fi
exit 47
"##,
        );

        let report = uninstall_after_legacy_cleanup(&paths.codex_home, &codex).unwrap();
        assert!(report.plugin_removed);
        assert!(report.marketplace_removed);
        assert!(!paths.marketplace_root.exists());
        assert!(paths.plugin_data_root.exists());
        let calls = fs::read_to_string(paths.codex_home.join("calls.log")).unwrap();
        assert!(calls.contains("plugin remove abtop@abtop-local --json"));
        assert!(calls.contains("plugin marketplace remove abtop-local --json"));
        assert!(!calls.contains("list"));
    }

    #[test]
    #[cfg(unix)]
    fn successful_uninstall_retains_the_same_stable_lock_inode() {
        use std::os::unix::fs::MetadataExt;

        let (temp, paths, bundle) = fixture_bundle();
        write_bundle(&paths, &bundle).unwrap();
        let config = format!(
            "[marketplaces.{MARKETPLACE_NAME}]\nsource_type = \"local\"\nsource = {:?}\n\n[plugins.\"{PLUGIN_ID}\"]\nenabled = true\n",
            paths.marketplace_root.to_string_lossy()
        );
        atomic_write_private(
            &paths.codex_home.join("config.toml"),
            config.as_bytes(),
            false,
        )
        .unwrap();
        let codex = temp.path().join("codex-fake");
        executable_file(
            &codex,
            br##"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf 'codex-cli 0.146.0\n'
  exit 0
fi
if [ "$1" = "plugin" ] && [ "$2" = "remove" ]; then
  sed '/^\[plugins\./,$d' "$CODEX_HOME/config.toml" > "$CODEX_HOME/config.toml.next"
  mv "$CODEX_HOME/config.toml.next" "$CODEX_HOME/config.toml"
  printf '{}\n'
  exit 0
fi
if [ "$1" = "plugin" ] && [ "$2" = "marketplace" ] && [ "$3" = "remove" ]; then
  : > "$CODEX_HOME/config.toml"
  printf '{}\n'
  exit 0
fi
exit 47
"##,
        );

        uninstall_after_legacy_cleanup(&paths.codex_home, &codex).unwrap();
        let lock_path = paths.codex_home.join(SETUP_LOCK_FILE);
        let first = fs::symlink_metadata(&lock_path).unwrap();
        uninstall_after_legacy_cleanup(&paths.codex_home, &codex).unwrap();
        let second = fs::symlink_metadata(&lock_path).unwrap();
        assert_eq!(first.dev(), second.dev());
        assert_eq!(first.ino(), second.ino());
    }

    #[test]
    #[cfg(unix)]
    fn uninstall_preserves_source_when_native_plugin_remove_is_a_noop() {
        let (temp, paths, bundle) = fixture_bundle();
        write_bundle(&paths, &bundle).unwrap();
        atomic_write_private(
            &paths.codex_home.join("config.toml"),
            format!("[plugins.\"{PLUGIN_ID}\"]\nenabled = true\n").as_bytes(),
            false,
        )
        .unwrap();
        let codex = temp.path().join("codex-fake");
        executable_file(
            &codex,
            br##"#!/bin/sh
if [ "$1" = "--version" ]; then printf 'codex-cli 0.146.0\n'; exit 0; fi
printf '%s\n' "$*" >> "$CODEX_HOME/calls.log"
printf '{}\n'
"##,
        );

        let error = uninstall_after_legacy_cleanup(&paths.codex_home, &codex).unwrap_err();
        assert!(error.to_string().contains("remains configured"));
        assert!(paths.marketplace_root.exists());
        assert_eq!(
            fs::read_to_string(paths.codex_home.join("calls.log")).unwrap(),
            "plugin remove abtop@abtop-local --json\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn uninstall_removes_reserved_plugin_before_rejecting_unsafe_source() {
        use std::os::unix::fs::symlink;

        let (temp, paths, bundle) = fixture_bundle();
        write_bundle(&paths, &bundle).unwrap();
        symlink(temp.path(), paths.codex_home.join("abtop/unexpected-link")).unwrap();
        atomic_write_private(
            &paths.codex_home.join("config.toml"),
            format!("[plugins.\"{PLUGIN_ID}\"]\nenabled = true\n").as_bytes(),
            false,
        )
        .unwrap();
        let codex = temp.path().join("codex-fake");
        executable_file(
            &codex,
            br##"#!/bin/sh
if [ "$1" = "--version" ]; then printf 'codex-cli 0.146.0\n'; exit 0; fi
printf '%s\n' "$*" >> "$CODEX_HOME/calls.log"
: > "$CODEX_HOME/config.toml"
printf '{}\n'
"##,
        );

        let error = uninstall_after_legacy_cleanup(&paths.codex_home, &codex).unwrap_err();
        assert!(error.to_string().contains("unexpected file or capability"));
        assert!(paths.marketplace_root.exists());
        assert_eq!(
            fs::read_to_string(paths.codex_home.join("calls.log")).unwrap(),
            "plugin remove abtop@abtop-local --json\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn uninstall_preserves_source_when_native_cache_remains() {
        let (temp, paths, bundle) = fixture_bundle();
        write_bundle(&paths, &bundle).unwrap();
        atomic_write_private(
            &paths.codex_home.join("config.toml"),
            format!("[plugins.\"{PLUGIN_ID}\"]\nenabled = true\n").as_bytes(),
            false,
        )
        .unwrap();
        fs::create_dir_all(
            paths
                .codex_home
                .join("plugins/cache")
                .join(MARKETPLACE_NAME)
                .join(PLUGIN_NAME)
                .join("retained"),
        )
        .unwrap();
        let codex = temp.path().join("codex-fake");
        executable_file(
            &codex,
            br##"#!/bin/sh
if [ "$1" = "--version" ]; then printf 'codex-cli 0.146.0\n'; exit 0; fi
: > "$CODEX_HOME/config.toml"
printf '{}\n'
"##,
        );

        let error = uninstall_after_legacy_cleanup(&paths.codex_home, &codex).unwrap_err();
        assert!(error.to_string().contains("plugin cache"));
        assert!(paths.marketplace_root.exists());
    }
}
