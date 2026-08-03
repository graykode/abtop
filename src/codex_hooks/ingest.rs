//! Silent Codex hook ingestion.
//!
//! The parser materializes only allowlisted lifecycle fields. Prompt text,
//! tool inputs/outputs, transcript paths, assistant messages, and every
//! unknown value are drained with `IgnoredAny` and never enter persisted state.

use super::plugin::{self, PluginPaths, HOOK_SCHEMA_REVISION};
use super::state::{
    unix_now_ms, HookEvent, HookEventKind, HookProcessIdentity, HookStateIngress, HookStateStore,
    HookToolClass, IntegrationIdentity, SessionStartSource,
};
use crate::collector::process;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const MAX_HOOK_DECLARATION_BYTES: usize = 256 * 1024;
const MAX_HOOK_STREAM_BYTES: usize = 4 * 1024 * 1024;
const MAX_LIFECYCLE_ID_BYTES: usize = 512;
const MAX_CWD_BYTES: usize = 16 * 1024;
const MAX_EVENT_NAME_BYTES: usize = 64;
const MAX_SOURCE_BYTES: usize = 16;
const MAX_ROOT_FIELDS: usize = 256;
const MAX_ANCESTORS: usize = 64;

pub(crate) fn run_from_environment(args: Vec<OsString>) -> io::Result<()> {
    let plugin_data = plugin_data_from_environment()?;
    let observed_at_ms = unix_now_ms();
    let ingress = HookStateStore::prepare(&plugin_data)?;
    let ingest_guard = match std::env::var_os("ABTOP_CODEX_HOOK_FAULT_TOKEN") {
        Some(token) => match ingress.adopt_launcher_marker(token.as_os_str(), observed_at_ms) {
            Ok(guard) => guard,
            Err(error) => {
                // Keep the launcher's untrusted/unusable marker in place and
                // independently establish a generic fail-closed marker.
                let _ = ingress.begin_ingest(observed_at_ms);
                return Err(error);
            }
        },
        None => ingress.begin_ingest(observed_at_ms)?,
    };
    let ingest_marker_id = ingest_guard.marker_id()?.to_owned();

    let helper_digest = parse_private_args(&args)?;
    let parsed = parse_and_drain_hook_input_outcome(io::stdin().lock())?;
    ingress.reclaim_stale_artifacts_after_drain(observed_at_ms, Some(&ingest_marker_id))?;
    let parsed = parsed?;
    let (store, process, integration) =
        attest_hook_environment(&plugin_data, &ingress, &helper_digest)?;
    let event = parsed.into_event(process, integration, observed_at_ms, ingest_marker_id)?;
    store.fold(event)?;
    ingest_guard.succeed()?;
    Ok(())
}

fn attest_hook_environment(
    plugin_data: &Path,
    ingress: &HookStateIngress,
    helper_digest: &str,
) -> io::Result<(HookStateStore, HookProcessIdentity, IntegrationIdentity)> {
    let codex_home = codex_home_from_plugin_data(plugin_data)?;
    let paths = PluginPaths::new(&codex_home)?;
    let canonical_data = fs::canonicalize(plugin_data)?;
    if canonical_data != fs::canonicalize(&paths.plugin_data_root)? {
        return Err(invalid_data(
            "PLUGIN_DATA does not identify abtop's plugin data",
        ));
    }

    let attestation = plugin::read_installation_attestation(&codex_home)?
        .ok_or_else(|| invalid_data("missing installation attestation"))?;
    if attestation.hook_schema_revision != HOOK_SCHEMA_REVISION
        || attestation.helper_digest != helper_digest
    {
        return Err(invalid_data("hook helper identity changed"));
    }
    let current_exe = std::env::current_exe()?;
    if plugin::helper_digest(&current_exe)? != helper_digest {
        return Err(invalid_data(
            "running helper does not match its installation",
        ));
    }
    validate_cached_plugin(
        &codex_home,
        &attestation.plugin_version,
        &attestation.hooks_digest,
    )?;
    let runtime = plugin::runtime_hook_config(&codex_home, &current_exe)?;
    let integration = IntegrationIdentity {
        hook_schema_revision: attestation.hook_schema_revision,
        helper_digest: attestation.helper_digest,
        installation_id: attestation.installation_id,
        config_digest: runtime.config_digest,
        complete_hook_set: runtime.complete_hook_set,
    };
    let store = ingress.bind(integration.clone())?;
    let process = resolve_nearest_codex_ancestor(&codex_home)?;
    Ok((store, process, integration))
}

fn parse_private_args(args: &[OsString]) -> io::Result<String> {
    if args.len() != 4
        || args[0] != OsStr::new("--schema-revision")
        || args[1] != OsStr::new(HOOK_SCHEMA_REVISION)
        || args[2] != OsStr::new("--helper-digest")
    {
        return Err(invalid_data("invalid private hook arguments"));
    }
    let digest = args[3]
        .to_str()
        .ok_or_else(|| invalid_data("invalid helper digest encoding"))?;
    if digest.len() != 71
        || !digest.starts_with("sha256:")
        || !digest[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid_data("invalid helper digest"));
    }
    Ok(digest.to_string())
}

fn plugin_data_from_environment() -> io::Result<PathBuf> {
    let value = std::env::var_os("PLUGIN_DATA")
        .or_else(|| std::env::var_os("CLAUDE_PLUGIN_DATA"))
        .ok_or_else(|| invalid_data("missing PLUGIN_DATA"))?;
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || path.file_name().and_then(|name| name.to_str()) != Some("abtop-abtop-local")
    {
        return Err(invalid_data("invalid PLUGIN_DATA"));
    }
    Ok(path)
}

fn codex_home_from_plugin_data(plugin_data: &Path) -> io::Result<PathBuf> {
    let data = plugin_data
        .parent()
        .ok_or_else(|| invalid_data("PLUGIN_DATA has no data parent"))?;
    if data.file_name().and_then(|name| name.to_str()) != Some("data") {
        return Err(invalid_data("PLUGIN_DATA is outside plugins/data"));
    }
    let plugins = data
        .parent()
        .ok_or_else(|| invalid_data("PLUGIN_DATA has no plugins parent"))?;
    if plugins.file_name().and_then(|name| name.to_str()) != Some("plugins") {
        return Err(invalid_data("PLUGIN_DATA is outside the plugins root"));
    }
    plugins
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| invalid_data("PLUGIN_DATA has no CODEX_HOME parent"))
}

fn validate_cached_plugin(
    codex_home: &Path,
    version: &str,
    expected_hooks_digest: &str,
) -> io::Result<()> {
    if version.is_empty() || version == "." || version == ".." || version.contains(['/', '\\']) {
        return Err(invalid_data("invalid installed plugin version"));
    }
    let expected_root = codex_home
        .join("plugins/cache/abtop-local/abtop")
        .join(version);
    let plugin_root = std::env::var_os("PLUGIN_ROOT")
        .or_else(|| std::env::var_os("CLAUDE_PLUGIN_ROOT"))
        .map(PathBuf::from)
        .ok_or_else(|| invalid_data("missing PLUGIN_ROOT"))?;
    if !plugin_root.is_absolute()
        || fs::canonicalize(&plugin_root)? != fs::canonicalize(&expected_root)?
    {
        return Err(invalid_data(
            "hook did not originate from the installed abtop cache",
        ));
    }
    let hooks = read_regular_bounded(
        &plugin_root.join("hooks/hooks.json"),
        MAX_HOOK_DECLARATION_BYTES,
    )?;
    if sha256(&hooks) != expected_hooks_digest {
        return Err(invalid_data("installed hook declaration changed"));
    }
    Ok(())
}

fn read_regular_bounded(path: &Path, maximum: usize) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > maximum as u64 {
        return Err(invalid_data("unsafe installed hook declaration"));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(invalid_data("oversized installed hook declaration"));
    }
    Ok(bytes)
}

#[derive(Debug, Default)]
struct ParsedHookInput {
    session_id: Option<String>,
    cwd: Option<String>,
    event: Option<String>,
    turn_id: Option<String>,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    agent_id: Option<String>,
    source: Option<String>,
    stop_hook_active: Option<bool>,
}

impl ParsedHookInput {
    fn into_event(
        self,
        process: HookProcessIdentity,
        integration: IntegrationIdentity,
        observed_at_ms: u64,
        ingest_marker_id: String,
    ) -> io::Result<HookEvent> {
        let session_id = self
            .session_id
            .ok_or_else(|| invalid_data("hook input has no session_id"))?;
        let cwd = self
            .cwd
            .ok_or_else(|| invalid_data("hook input has no cwd"))?;
        let event_name = self
            .event
            .ok_or_else(|| invalid_data("hook input has no hook_event_name"))?;
        let kind = parse_event_kind(&event_name)?;
        let tool_class = match kind {
            HookEventKind::PreToolUse | HookEventKind::PostToolUse => {
                Some(if self.tool_name.as_deref() == Some("request_user_input") {
                    HookToolClass::RequestUserInput
                } else {
                    HookToolClass::Ordinary
                })
            }
            _ => None,
        };
        let session_start_source = match (kind, self.source.as_deref()) {
            (HookEventKind::SessionStart, Some("startup")) => Some(SessionStartSource::Startup),
            (HookEventKind::SessionStart, Some("resume")) => Some(SessionStartSource::Resume),
            (HookEventKind::SessionStart, Some("clear")) => Some(SessionStartSource::Clear),
            (HookEventKind::SessionStart, Some("compact")) => Some(SessionStartSource::Compact),
            (HookEventKind::SessionStart, _) => {
                return Err(invalid_data("SessionStart has an invalid source"));
            }
            (_, Some(_)) => return Err(invalid_data("unexpected hook source field")),
            _ => None,
        };
        if self
            .agent_id
            .as_deref()
            .is_some_and(|agent_id| agent_id == session_id.as_str())
        {
            return Err(invalid_data(
                "hook agent_id aliases the shared root session",
            ));
        }
        require_shape(
            kind,
            self.turn_id.as_deref(),
            self.tool_name.as_deref(),
            self.tool_use_id.as_deref(),
            self.agent_id.as_deref(),
            self.stop_hook_active,
        )?;
        Ok(HookEvent {
            kind,
            session_id,
            cwd,
            turn_id: self.turn_id,
            tool_use_id: self.tool_use_id,
            tool_class,
            agent_id: self.agent_id,
            session_start_source,
            observed_at_ms,
            process,
            integration,
            stop_hook_active: self.stop_hook_active,
            ingest_marker_id,
        })
    }
}

/// Parse one hook object without retaining its raw JSON, then drain the input.
///
/// Draining happens even after malformed JSON so Codex never blocks while its
/// hook writer is still holding the other end of the pipe. The hard byte cap
/// bounds adversarial or broken producers; ordinary large ignored fields do
/// not allocate an aggregate input buffer and are accepted up to that cap.
#[cfg(all(test, unix))]
fn parse_before_preflight<R, F, T>(reader: R, preflight: F) -> io::Result<(ParsedHookInput, T)>
where
    R: Read,
    F: FnOnce() -> io::Result<T>,
{
    // This ordering is part of the hook protocol. Codex writes the complete
    // JSON object before it can wait for us to exit; hashing executables or
    // probing process ancestry first can leave both processes blocked on a
    // full stdin pipe until Codex's one-second hook timeout fires.
    let parsed = parse_and_drain_hook_input_outcome(reader)??;
    let verified = preflight()?;
    Ok((parsed, verified))
}

#[cfg(test)]
fn parse_and_drain_hook_input<R: Read>(reader: R) -> io::Result<ParsedHookInput> {
    parse_and_drain_hook_input_outcome(reader)?
}

/// Return the parse result only after the bounded input reached EOF.
///
/// Keeping the parse error nested lets the ingest path perform content-free
/// maintenance after a malformed but fully drained payload, without running
/// maintenance when an oversized or broken producer prevented a complete
/// drain.
fn parse_and_drain_hook_input_outcome<R: Read>(
    reader: R,
) -> io::Result<io::Result<ParsedHookInput>> {
    let mut reader = BoundedHookReader::new(reader, MAX_HOOK_STREAM_BYTES);
    let parsed = {
        let mut deserializer = serde_json::Deserializer::from_reader(&mut reader);
        ParsedHookInput::deserialize(&mut deserializer)
            .and_then(|parsed| deserializer.end().map(|()| parsed))
            .map_err(|error| invalid_data(format!("invalid hook JSON: {error}")))
    };

    let drain_result = reader.drain_to_eof();
    if reader.exceeded {
        return Err(invalid_data("hook input exceeds its streaming bound"));
    }
    drain_result?;
    Ok(parsed)
}

struct BoundedHookReader<R> {
    inner: R,
    consumed: usize,
    maximum: usize,
    exceeded: bool,
}

impl<R> BoundedHookReader<R> {
    fn new(inner: R, maximum: usize) -> Self {
        Self {
            inner,
            consumed: 0,
            maximum,
            exceeded: false,
        }
    }
}

impl<R: Read> BoundedHookReader<R> {
    fn read_bounded(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.exceeded {
            return Err(invalid_data("hook input exceeds its streaming bound"));
        }
        if buffer.is_empty() {
            return Ok(0);
        }

        // Permit one probe byte beyond the bound so an input of exactly the
        // maximum size can still be distinguished from an oversized stream.
        let remaining_with_probe = self.maximum.saturating_add(1).saturating_sub(self.consumed);
        if remaining_with_probe == 0 {
            self.exceeded = true;
            return Err(invalid_data("hook input exceeds its streaming bound"));
        }
        let maximum_read = buffer.len().min(remaining_with_probe);
        let read = loop {
            match self.inner.read(&mut buffer[..maximum_read]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        self.consumed = self.consumed.saturating_add(read);
        if self.consumed > self.maximum {
            self.exceeded = true;
            return Err(invalid_data("hook input exceeds its streaming bound"));
        }
        Ok(read)
    }

    fn drain_to_eof(&mut self) -> io::Result<()> {
        if self.exceeded {
            return Err(invalid_data("hook input exceeds its streaming bound"));
        }
        let mut buffer = [0_u8; 8192];
        loop {
            let read = self.read_bounded(&mut buffer)?;
            if read == 0 {
                return Ok(());
            }
        }
    }
}

impl<R: Read> Read for BoundedHookReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_bounded(buffer)
    }
}

impl<'de> serde::Deserialize<'de> for ParsedHookInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ParsedHookVisitor)
    }
}

struct ParsedHookVisitor;

#[derive(Clone, Copy)]
enum HookField {
    SessionId,
    Cwd,
    Event,
    TurnId,
    ToolName,
    ToolUseId,
    AgentId,
    Source,
    StopHookActive,
    Other,
}

impl HookField {
    fn bit(self) -> Option<u16> {
        match self {
            Self::SessionId => Some(1 << 0),
            Self::Cwd => Some(1 << 1),
            Self::Event => Some(1 << 2),
            Self::TurnId => Some(1 << 3),
            Self::ToolName => Some(1 << 4),
            Self::ToolUseId => Some(1 << 5),
            Self::AgentId => Some(1 << 6),
            Self::Source => Some(1 << 7),
            Self::StopHookActive => Some(1 << 8),
            Self::Other => None,
        }
    }
}

impl<'de> Deserialize<'de> for HookField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_identifier(HookFieldVisitor)
    }
}

struct HookFieldVisitor;

impl Visitor<'_> for HookFieldVisitor {
    type Value = HookField;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a hook object field")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(match value {
            "session_id" => HookField::SessionId,
            "cwd" => HookField::Cwd,
            "hook_event_name" => HookField::Event,
            "turn_id" => HookField::TurnId,
            "tool_name" => HookField::ToolName,
            "tool_use_id" => HookField::ToolUseId,
            "agent_id" => HookField::AgentId,
            "source" => HookField::Source,
            "stop_hook_active" => HookField::StopHookActive,
            _ => HookField::Other,
        })
    }
}

struct BoundedString<const MAXIMUM: usize>(String);

impl<'de, const MAXIMUM: usize> Deserialize<'de> for BoundedString<MAXIMUM> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(BoundedStringVisitor::<MAXIMUM>)
    }
}

struct BoundedStringVisitor<const MAXIMUM: usize>;

impl<const MAXIMUM: usize> Visitor<'_> for BoundedStringVisitor<MAXIMUM> {
    type Value = BoundedString<MAXIMUM>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "a string no longer than {MAXIMUM} bytes")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value.len() > MAXIMUM {
            return Err(E::custom("allowlisted hook field is oversized"));
        }
        Ok(BoundedString(value.to_owned()))
    }
}

impl<'de> Visitor<'de> for ParsedHookVisitor {
    type Value = ParsedHookInput;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("one Codex hook object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut parsed = ParsedHookInput::default();
        let mut seen = 0_u16;
        let mut field_count = 0_usize;
        while let Some(field) = map.next_key::<HookField>()? {
            field_count = field_count.saturating_add(1);
            if field_count > MAX_ROOT_FIELDS {
                return Err(de::Error::custom("hook object has too many fields"));
            }
            if let Some(bit) = field.bit() {
                if seen & bit != 0 {
                    return Err(de::Error::custom("duplicate critical hook field"));
                }
                seen |= bit;
            }
            match field {
                HookField::SessionId => {
                    parsed.session_id =
                        Some(map.next_value::<BoundedString<MAX_LIFECYCLE_ID_BYTES>>()?.0);
                }
                HookField::Cwd => {
                    parsed.cwd = Some(map.next_value::<BoundedString<MAX_CWD_BYTES>>()?.0);
                }
                HookField::Event => {
                    parsed.event = Some(map.next_value::<BoundedString<MAX_EVENT_NAME_BYTES>>()?.0);
                }
                HookField::TurnId => {
                    parsed.turn_id =
                        Some(map.next_value::<BoundedString<MAX_LIFECYCLE_ID_BYTES>>()?.0);
                }
                HookField::ToolName => {
                    parsed.tool_name =
                        Some(map.next_value::<BoundedString<MAX_LIFECYCLE_ID_BYTES>>()?.0);
                }
                HookField::ToolUseId => {
                    parsed.tool_use_id =
                        Some(map.next_value::<BoundedString<MAX_LIFECYCLE_ID_BYTES>>()?.0);
                }
                HookField::AgentId => {
                    parsed.agent_id =
                        Some(map.next_value::<BoundedString<MAX_LIFECYCLE_ID_BYTES>>()?.0);
                }
                HookField::Source => {
                    parsed.source = Some(map.next_value::<BoundedString<MAX_SOURCE_BYTES>>()?.0);
                }
                HookField::StopHookActive => {
                    parsed.stop_hook_active = Some(map.next_value::<bool>()?);
                }
                HookField::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(parsed)
    }
}

fn parse_event_kind(value: &str) -> io::Result<HookEventKind> {
    match value {
        "PreToolUse" => Ok(HookEventKind::PreToolUse),
        "PermissionRequest" => Ok(HookEventKind::PermissionRequest),
        "PostToolUse" => Ok(HookEventKind::PostToolUse),
        "PreCompact" => Ok(HookEventKind::PreCompact),
        "PostCompact" => Ok(HookEventKind::PostCompact),
        "SessionStart" => Ok(HookEventKind::SessionStart),
        "SessionEnd" => Ok(HookEventKind::SessionEnd),
        "UserPromptSubmit" => Ok(HookEventKind::UserPromptSubmit),
        "SubagentStart" => Ok(HookEventKind::SubagentStart),
        "SubagentStop" => Ok(HookEventKind::SubagentStop),
        "Stop" => Ok(HookEventKind::Stop),
        _ => Err(invalid_data("unsupported hook event")),
    }
}

fn require_shape(
    kind: HookEventKind,
    turn_id: Option<&str>,
    tool_name: Option<&str>,
    tool_use_id: Option<&str>,
    agent_id: Option<&str>,
    stop_hook_active: Option<bool>,
) -> io::Result<()> {
    let needs_turn = matches!(
        kind,
        HookEventKind::PreToolUse
            | HookEventKind::PermissionRequest
            | HookEventKind::PostToolUse
            | HookEventKind::PreCompact
            | HookEventKind::PostCompact
            | HookEventKind::UserPromptSubmit
            | HookEventKind::SubagentStart
            | HookEventKind::SubagentStop
            | HookEventKind::Stop
    );
    if needs_turn && turn_id.is_none_or(str::is_empty) {
        return Err(invalid_data("hook event has no turn_id"));
    }
    if matches!(kind, HookEventKind::PreToolUse | HookEventKind::PostToolUse)
        && tool_use_id.is_none_or(str::is_empty)
    {
        return Err(invalid_data("tool hook has no tool_use_id"));
    }
    if matches!(
        kind,
        HookEventKind::PreToolUse | HookEventKind::PermissionRequest | HookEventKind::PostToolUse
    ) && tool_name.is_none_or(str::is_empty)
    {
        return Err(invalid_data("tool hook has no tool_name"));
    }
    if matches!(
        kind,
        HookEventKind::SubagentStart | HookEventKind::SubagentStop
    ) && agent_id.is_none_or(str::is_empty)
    {
        return Err(invalid_data("subagent hook has no agent_id"));
    }
    if matches!(
        kind,
        HookEventKind::SessionStart | HookEventKind::SessionEnd | HookEventKind::Stop
    ) && agent_id.is_some()
    {
        return Err(invalid_data("root hook unexpectedly names a subagent"));
    }
    if matches!(kind, HookEventKind::Stop | HookEventKind::SubagentStop) {
        if stop_hook_active.is_none() {
            return Err(invalid_data("stop hook has no stop_hook_active flag"));
        }
    } else if stop_hook_active.is_some() {
        return Err(invalid_data("unexpected stop_hook_active field"));
    }
    Ok(())
}

fn resolve_nearest_codex_ancestor(codex_home: &Path) -> io::Result<HookProcessIdentity> {
    let processes = process::get_process_info();
    let mut current = std::process::id();
    let mut visited = std::collections::BTreeSet::new();
    for _ in 0..MAX_ANCESTORS {
        if !visited.insert(current) {
            break;
        }
        let parent = processes
            .get(&current)
            .map(|info| info.ppid)
            .or_else(|| (current == std::process::id()).then(current_parent_pid))
            .filter(|pid| *pid > 1)
            .ok_or_else(|| invalid_data("cannot resolve Codex hook ancestry"))?;
        current = parent;
        let before = process::get_process_incarnation(current)
            .ok_or_else(|| invalid_data("cannot identify hook ancestor incarnation"))?;
        let executable = process::get_process_executable(current)
            .ok_or_else(|| invalid_data("cannot identify hook ancestor executable"))?;
        let argv = process::get_process_argv(current)
            .ok_or_else(|| invalid_data("cannot identify hook ancestor arguments"))?;
        let after = process::get_process_incarnation(current)
            .ok_or_else(|| invalid_data("hook ancestor disappeared during inspection"))?;
        if before != after {
            return Err(invalid_data("hook ancestor incarnation changed"));
        }
        if !is_native_codex_executable(&executable) {
            continue;
        }
        if excluded_codex_host(&executable, &argv) {
            return Err(invalid_data("hook belongs to an excluded Codex host"));
        }
        if let Some(root) = process::read_process_env_var(current, "CODEX_HOME") {
            let root = PathBuf::from(root);
            if !root.is_absolute()
                || fs::canonicalize(root).ok().as_deref()
                    != fs::canonicalize(codex_home).ok().as_deref()
            {
                return Err(invalid_data("hook process uses another CODEX_HOME"));
            }
        }
        let started_at_ms = process::get_process_started_at_ms(current)
            .ok_or_else(|| invalid_data("cannot identify hook ancestor start time"))?;
        return Ok(HookProcessIdentity {
            pid: current,
            started_at_ms,
            incarnation: before,
            shared_host: false,
            launch_config_ambiguous: launch_configuration_ambiguous(&argv),
        });
    }
    Err(invalid_data("no eligible native Codex ancestor"))
}

#[cfg(unix)]
fn current_parent_pid() -> u32 {
    // SAFETY: getppid has no preconditions or side effects.
    u32::try_from(unsafe { libc::getppid() }).unwrap_or(0)
}

#[cfg(not(unix))]
fn current_parent_pid() -> u32 {
    0
}

fn is_native_codex_executable(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    (name == "codex" || name == "codex.exe" || name.starts_with("codex-"))
        && name != "codex-app-server"
        && name != "codex-app-server.exe"
        && name != "codex-code-mode-host"
        && name != "codex-code-mode-host.exe"
}

fn excluded_codex_host(executable: &Path, argv: &[OsString]) -> bool {
    let name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.contains("app-server") || name.contains("code-mode-host") {
        return true;
    }
    argv.iter().skip(1).any(|argument| {
        matches!(
            argument.to_str(),
            Some(
                "app-server"
                    | "daemon"
                    | "mcp-server"
                    | "remote-control"
                    | "exec-server"
                    | "codex-code-mode-host"
            )
        )
    })
}

fn launch_configuration_ambiguous(argv: &[OsString]) -> bool {
    let mut iter = argv.iter().skip(1).peekable();
    while let Some(argument) = iter.next() {
        let Some(argument) = argument.to_str() else {
            return true;
        };
        if argument == "--dangerously-bypass-hook-trust"
            || argument == "--profile"
            || argument.starts_with("--profile=")
            || argument == "-p"
            || (argument.starts_with("-p") && argument.len() > 2)
        {
            return true;
        }

        if matches!(argument, "--enable" | "--disable") {
            let Some(feature) = iter.next().and_then(|value| value.to_str()) else {
                return true;
            };
            if feature_override_affects_hooks(feature) {
                return true;
            }
            continue;
        }
        if let Some(feature) = argument
            .strip_prefix("--enable=")
            .or_else(|| argument.strip_prefix("--disable="))
        {
            if feature_override_affects_hooks(feature) {
                return true;
            }
            continue;
        }

        if matches!(argument, "-c" | "--config") {
            let Some(value) = iter.next().and_then(|value| value.to_str()) else {
                return true;
            };
            if config_override_affects_hooks(value) {
                return true;
            }
            continue;
        }
        if let Some(value) = argument.strip_prefix("--config=") {
            if config_override_affects_hooks(value) {
                return true;
            }
            continue;
        }
        if let Some(value) = argument
            .strip_prefix("-c=")
            .or_else(|| argument.strip_prefix("-c"))
        {
            if config_override_affects_hooks(value) {
                return true;
            }
        }
    }
    false
}

fn feature_override_affects_hooks(value: &str) -> bool {
    value
        .split(',')
        .map(str::trim)
        .any(|feature| matches!(feature, "hooks" | "plugins" | "codex_hooks"))
}

fn config_override_affects_hooks(value: &str) -> bool {
    let key = value.split_once('=').map_or(value, |(key, _)| key).trim();
    matches!(
        key,
        "hooks"
            | "plugins"
            | "features.hooks"
            | "features.plugins"
            | "features.codex_hooks"
            | "debug.config_lockfile"
            | "requirements"
    ) || key.starts_with("hooks.")
        || key.starts_with("plugins.")
        || key.starts_with("debug.config_lockfile.")
        || key.starts_with("requirements.")
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut value = String::with_capacity(71);
    value.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::{Cursor, Repeat, Take};
    use std::rc::Rc;

    #[cfg(unix)]
    #[test]
    fn input_larger_than_a_pipe_is_drained_before_slow_preflight() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration;

        let (reader, mut writer) = UnixStream::pair().expect("create hook input socket");
        let writer_finished = Arc::new(AtomicBool::new(false));
        let writer_finished_in_thread = Arc::clone(&writer_finished);
        let body = "x".repeat(3 * 1024 * 1024);
        let input = format!(
            "{{\"prompt\":\"{body}\",\"session_id\":\"session-a\",\"cwd\":\"/tmp/project\",\"hook_event_name\":\"UserPromptSubmit\",\"turn_id\":\"turn-a\"}}"
        );
        assert!(input.len() < MAX_HOOK_STREAM_BYTES);
        let writer_thread = thread::spawn(move || {
            writer
                .write_all(input.as_bytes())
                .expect("write complete oversized-pipe hook input");
            writer_finished_in_thread.store(true, Ordering::Release);
            writer
                .shutdown(std::net::Shutdown::Write)
                .expect("close hook input writer");
        });

        let (parsed, preflight_value) = parse_before_preflight(reader, || {
            // A preflight-first implementation reaches this closure while the
            // producer is still blocked on the full socket buffer.
            if !writer_finished.load(Ordering::Acquire) {
                return Err(invalid_data("preflight ran before stdin was drained"));
            }
            thread::sleep(Duration::from_millis(25));
            Ok(42)
        })
        .expect("drain hook input before expensive preflight");
        writer_thread.join().expect("hook input writer completes");

        assert_eq!(preflight_value, 42);
        assert_eq!(parsed.session_id.as_deref(), Some("session-a"));
        assert_eq!(parsed.event.as_deref(), Some("UserPromptSubmit"));
    }

    #[test]
    fn parser_discards_every_sensitive_field() {
        let input = br#"{
            "session_id":"session-a",
            "cwd":"/tmp/project",
            "hook_event_name":"PreToolUse",
            "turn_id":"turn-a",
            "tool_use_id":"call-a",
            "tool_name":"Bash",
            "prompt":"ABTOP_SECRET_PROMPT",
            "tool_input":{"command":"ABTOP_SECRET_COMMAND"},
            "tool_response":{"output":"ABTOP_SECRET_OUTPUT"},
            "transcript_path":"/tmp/ABTOP_SECRET_TRANSCRIPT",
            "last_assistant_message":"ABTOP_SECRET_MESSAGE"
        }"#;
        let parsed = parse_and_drain_hook_input(input.as_slice()).expect("parse hook");
        let debug = format!(
            "{} {:?} {:?}",
            parsed.session_id.unwrap(),
            parsed.turn_id,
            parsed.tool_use_id
        );
        assert!(!debug.contains("ABTOP_SECRET"));
    }

    #[test]
    fn large_sensitive_value_is_streamed_and_not_retained() {
        const LARGE_IGNORED_BYTES: u64 = 1024 * 1024;
        let prefix = Cursor::new(br#"{"prompt":"ABTOP_SECRET_BEGIN_"#.as_slice());
        let body: Take<Repeat> = io::repeat(b'x').take(LARGE_IGNORED_BYTES);
        let suffix = Cursor::new(
            br#"_ABTOP_SECRET_END","session_id":"session-a","cwd":"/tmp/project","hook_event_name":"UserPromptSubmit","turn_id":"turn-a"}"#.as_slice(),
        );
        let consumed = Rc::new(Cell::new(0));
        let reader = CountingReader::new(prefix.chain(body).chain(suffix), consumed.clone());

        let parsed = parse_and_drain_hook_input(reader).expect("stream large ignored value");
        let retained_bytes = parsed.session_id.as_deref().map_or(0, str::len)
            + parsed.cwd.as_deref().map_or(0, str::len)
            + parsed.event.as_deref().map_or(0, str::len)
            + parsed.turn_id.as_deref().map_or(0, str::len)
            + parsed.tool_name.as_deref().map_or(0, str::len)
            + parsed.tool_use_id.as_deref().map_or(0, str::len)
            + parsed.agent_id.as_deref().map_or(0, str::len)
            + parsed.source.as_deref().map_or(0, str::len);
        assert!(retained_bytes < 128);
        assert!(consumed.get() > LARGE_IGNORED_BYTES as usize);
        assert_eq!(parsed.session_id.as_deref(), Some("session-a"));
        assert_eq!(parsed.event.as_deref(), Some("UserPromptSubmit"));

        let temp = tempfile::tempdir().expect("temporary plugin root");
        // macOS exposes `/var` through a symlink. Production passes the
        // canonical plugin-data path, and the secure store correctly rejects
        // every symlink component, so make the fixture mirror production.
        let temp_root = fs::canonicalize(temp.path()).expect("canonical temporary root");
        let plugin_data = temp_root.join("plugins/data").join("abtop-abtop-local");
        fs::create_dir_all(&plugin_data).expect("create plugin data");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&plugin_data, fs::Permissions::from_mode(0o700))
                .expect("private plugin data");
        }
        let ingress = HookStateStore::prepare(&plugin_data).expect("prepare hook state");
        let guard = ingress.begin_ingest(10).expect("create ingest marker");
        let marker_id = guard.marker_id().expect("marker ID").to_owned();
        let event = parsed
            .into_event(process_identity(), identity(), 10, marker_id)
            .expect("reduce allowlisted lifecycle fields");
        ingress
            .bind(identity())
            .expect("bind hook state")
            .fold(event)
            .expect("persist content-free state");
        guard.succeed().expect("commit ingest marker");
        assert_tree_excludes(&plugin_data, "ABTOP_SECRET");
    }

    #[test]
    fn malformed_input_is_drained_before_it_fails_closed() {
        let input = br#"{"session_id":false THIS_IS_MALFORMED_AND_MUST_BE_DRAINED"#;
        let mut reader = Cursor::new(input.as_slice());
        assert!(parse_and_drain_hook_input(&mut reader).is_err());
        assert_eq!(reader.position(), input.len() as u64);
    }

    #[test]
    fn oversized_stream_fails_closed_without_an_aggregate_buffer() {
        let prefix = Cursor::new(br#"{"ignored":""#.as_slice());
        let body = io::repeat(b'x').take(MAX_HOOK_STREAM_BYTES as u64 + 1);
        let consumed = Rc::new(Cell::new(0));
        let reader = CountingReader::new(prefix.chain(body), consumed.clone());

        let error = parse_and_drain_hook_input(reader).expect_err("oversized stream");
        assert!(error.to_string().contains("streaming bound"));
        assert_eq!(consumed.get(), MAX_HOOK_STREAM_BYTES + 1);
    }

    #[test]
    fn allowlisted_scalars_are_bounded_and_input_is_still_drained() {
        let oversized_id = "x".repeat(MAX_LIFECYCLE_ID_BYTES + 1);
        let input = format!(
            "{{\"session_id\":\"{oversized_id}\",\"ignored\":\"ABTOP_SECRET_AFTER_ERROR\"}}"
        );
        let mut reader = Cursor::new(input.as_bytes());

        assert!(parse_and_drain_hook_input(&mut reader).is_err());
        assert_eq!(reader.position(), input.len() as u64);
    }

    #[test]
    fn excessive_root_fields_fail_closed_and_are_drained() {
        let mut input = String::from("{");
        for index in 0..=MAX_ROOT_FIELDS {
            if index != 0 {
                input.push(',');
            }
            use std::fmt::Write as _;
            write!(input, "\"ignored_{index}\":null").expect("build hook object");
        }
        input.push_str(",\"tail\":\"ABTOP_SECRET_AFTER_FIELD_LIMIT\"}");
        let mut reader = Cursor::new(input.as_bytes());

        assert!(parse_and_drain_hook_input(&mut reader).is_err());
        assert_eq!(reader.position(), input.len() as u64);
    }

    #[test]
    fn duplicate_and_trailing_objects_fail_closed() {
        assert!(
            parse_and_drain_hook_input(br#"{"session_id":"a","session_id":"b"}"#.as_slice())
                .is_err()
        );
        assert!(parse_and_drain_hook_input(br#"{} {}"#.as_slice()).is_err());
    }

    #[test]
    fn all_eleven_event_names_are_exact() {
        for name in plugin::HOOK_EVENTS {
            assert!(parse_event_kind(name).is_ok(), "{name}");
        }
        assert!(parse_event_kind("PostToolUseFailure").is_err());
        assert!(parse_event_kind("pre_tool_use").is_err());
    }

    #[test]
    fn session_start_source_and_tool_shape_are_exact() {
        let start = parse_and_drain_hook_input(
            br#"{"session_id":"session-a","cwd":"/tmp/project","hook_event_name":"SessionStart","source":"compact"}"#.as_slice(),
        )
        .unwrap()
        .into_event(process_identity(), identity(), 10, test_marker_id())
        .unwrap();
        assert_eq!(
            start.session_start_source,
            Some(SessionStartSource::Compact)
        );

        let bad_source = parse_and_drain_hook_input(
            br#"{"session_id":"session-a","cwd":"/tmp/project","hook_event_name":"SessionEnd","source":"startup"}"#.as_slice(),
        )
        .unwrap();
        assert!(bad_source
            .into_event(process_identity(), identity(), 10, test_marker_id())
            .is_err());

        let missing_tool_name = parse_and_drain_hook_input(
            br#"{"session_id":"session-a","cwd":"/tmp/project","hook_event_name":"PreToolUse","turn_id":"turn-a","tool_use_id":"call-a"}"#.as_slice(),
        )
        .unwrap();
        assert!(missing_tool_name
            .into_event(process_identity(), identity(), 10, test_marker_id())
            .is_err());
    }

    #[test]
    fn stop_hook_active_is_exact_and_limited_to_stop_events() {
        let stop = parse_and_drain_hook_input(
            br#"{"session_id":"session-a","cwd":"/tmp/project","hook_event_name":"Stop","turn_id":"turn-a","stop_hook_active":true}"#.as_slice(),
        )
        .unwrap()
        .into_event(process_identity(), identity(), 10, test_marker_id())
        .unwrap();
        assert_eq!(stop.stop_hook_active, Some(true));

        let subagent_stop = parse_and_drain_hook_input(
            br#"{"session_id":"session-a","cwd":"/tmp/project","hook_event_name":"SubagentStop","turn_id":"turn-a","agent_id":"agent-a","stop_hook_active":false}"#.as_slice(),
        )
        .unwrap()
        .into_event(process_identity(), identity(), 10, test_marker_id())
        .unwrap();
        assert_eq!(subagent_stop.stop_hook_active, Some(false));

        for invalid in [
            br#"{"session_id":"session-a","cwd":"/tmp/project","hook_event_name":"Stop","turn_id":"turn-a"}"#.as_slice(),
            br#"{"session_id":"session-a","cwd":"/tmp/project","hook_event_name":"UserPromptSubmit","turn_id":"turn-a","stop_hook_active":false}"#.as_slice(),
            br#"{"session_id":"session-a","cwd":"/tmp/project","hook_event_name":"Stop","turn_id":"turn-a","stop_hook_active":"true"}"#.as_slice(),
        ] {
            let result = parse_and_drain_hook_input(invalid).and_then(|parsed| {
                parsed.into_event(process_identity(), identity(), 10, test_marker_id())
            });
            assert!(result.is_err());
        }
    }

    #[test]
    fn child_agent_identity_must_not_alias_the_shared_session() {
        let parsed = parse_and_drain_hook_input(
            br#"{"session_id":"root-a","cwd":"/tmp/project","hook_event_name":"SubagentStart","turn_id":"turn-a","agent_id":"root-a"}"#.as_slice(),
        )
        .unwrap();
        assert!(parsed
            .into_event(process_identity(), identity(), 10, test_marker_id())
            .is_err());
    }

    #[test]
    fn native_binary_and_host_filters_are_conservative() {
        assert!(is_native_codex_executable(Path::new("/opt/codex")));
        assert!(is_native_codex_executable(Path::new(
            "/opt/codex-aarch64-apple-darwin"
        )));
        assert!(!is_native_codex_executable(Path::new(
            "/opt/codex-code-mode-host"
        )));
        assert!(excluded_codex_host(
            Path::new("/opt/codex"),
            &[OsString::from("codex"), OsString::from("app-server")]
        ));
    }

    #[test]
    fn trust_bypass_and_hook_overrides_are_ambiguous() {
        assert!(launch_configuration_ambiguous(&[
            OsString::from("codex"),
            OsString::from("--dangerously-bypass-hook-trust")
        ]));
        assert!(launch_configuration_ambiguous(&[
            OsString::from("codex"),
            OsString::from("-c"),
            OsString::from("hooks.state={}")
        ]));
        for arguments in [
            vec!["codex", "-p", "profile-a"],
            vec!["codex", "-pprofile-a"],
            vec!["codex", "-p=profile-a"],
            vec!["codex", "--enable", "hooks"],
            vec!["codex", "--disable=plugins"],
            vec!["codex", "--enable=codex_hooks"],
            vec!["codex", "-cfeatures.hooks=false"],
            vec!["codex", "-c=features.plugins=true"],
            vec![
                "codex",
                "--config=debug.config_lockfile.load_path=/tmp/lock.toml",
            ],
            vec!["codex", "-c", "requirements.allowed_hooks=[]"],
        ] {
            let arguments = arguments
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>();
            assert!(launch_configuration_ambiguous(&arguments), "{arguments:?}");
        }
        assert!(!launch_configuration_ambiguous(&[
            OsString::from("codex"),
            OsString::from("-c"),
            OsString::from("model=plugins-v2")
        ]));
        assert!(!launch_configuration_ambiguous(&[
            OsString::from("codex"),
            OsString::from("--yolo")
        ]));
    }

    fn identity() -> IntegrationIdentity {
        IntegrationIdentity {
            hook_schema_revision: "1".into(),
            helper_digest: format!("sha256:{}", "1".repeat(64)),
            installation_id: "a".repeat(32),
            config_digest: format!("sha256:{}", "2".repeat(64)),
            complete_hook_set: true,
        }
    }

    fn process_identity() -> HookProcessIdentity {
        HookProcessIdentity {
            pid: std::process::id(),
            started_at_ms: 1,
            incarnation: "test-incarnation".into(),
            shared_host: false,
            launch_config_ambiguous: false,
        }
    }

    fn test_marker_id() -> String {
        format!("hook-{}.json", "a".repeat(32))
    }

    struct CountingReader<R> {
        inner: R,
        consumed: Rc<Cell<usize>>,
    }

    impl<R> CountingReader<R> {
        fn new(inner: R, consumed: Rc<Cell<usize>>) -> Self {
            Self { inner, consumed }
        }
    }

    impl<R: Read> Read for CountingReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(buffer)?;
            self.consumed.set(self.consumed.get().saturating_add(read));
            Ok(read)
        }
    }

    fn assert_tree_excludes(path: &Path, needle: &str) {
        for entry in fs::read_dir(path).expect("read persisted state tree") {
            let entry = entry.expect("read state entry");
            let file_type = entry.file_type().expect("state entry type");
            if file_type.is_dir() {
                assert_tree_excludes(&entry.path(), needle);
            } else if file_type.is_file() {
                let bytes = fs::read(entry.path()).expect("read persisted state file");
                assert!(!String::from_utf8_lossy(&bytes).contains(needle));
            }
        }
    }
}
