use std::collections::HashMap;
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub rss_kb: u64,
    pub cpu_pct: f64,
    pub command: String,
}

/// Return the OS-reported executable for one exact live PID.
///
/// This does not inspect or reinterpret argv. Callers that bind a security
/// decision to the result must separately verify the process incarnation
/// before and after this lookup.
#[cfg(target_os = "linux")]
pub fn get_process_executable(pid: u32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(target_vendor = "apple")]
pub fn get_process_executable(pid: u32) -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::os::raw::{c_int, c_void};
    use std::os::unix::ffi::OsStrExt;

    const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffer_size: u32) -> c_int;
    }

    let pid = c_int::try_from(pid).ok()?;
    let mut buffer = vec![0_u8; PROC_PIDPATHINFO_MAXSIZE];
    // SAFETY: the buffer is writable for its full reported capacity and the
    // PID conversion above is exact.
    let length = unsafe {
        proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast(),
            u32::try_from(buffer.len()).ok()?,
        )
    };
    if length <= 0 || usize::try_from(length).ok()? >= buffer.len() {
        return None;
    }
    let path = CStr::from_bytes_until_nul(&buffer).ok()?;
    if path.to_bytes().is_empty() {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())))
}

#[cfg(target_os = "windows")]
pub fn get_process_executable(pid: u32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: the requested access is read-only and `pid` is passed by value.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut buffer = vec![0_u16; 32_768];
    let mut length = 32_768_u32;
    // SAFETY: `buffer` is writable for `length` UTF-16 code units and the
    // process handle remains live until the matching CloseHandle below.
    let succeeded = unsafe {
        QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length as *mut u32)
    };
    // SAFETY: this function owns the non-null process handle.
    unsafe { CloseHandle(process) };
    if succeeded == 0 || length == 0 {
        return None;
    }
    buffer.truncate(usize::try_from(length).ok()?);
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer)))
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple", target_os = "windows")))]
pub fn get_process_executable(_pid: u32) -> Option<PathBuf> {
    None
}

/// Return the exact OS argv vector for one live PID, preserving empty and
/// non-UTF-8 arguments where the platform exposes them.
#[cfg(target_os = "linux")]
pub fn get_process_argv(pid: u32) -> Option<Vec<OsString>> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    parse_linux_cmdline(&bytes)
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn parse_linux_cmdline(bytes: &[u8]) -> Option<Vec<OsString>> {
    use std::os::unix::ffi::OsStringExt;

    if bytes.is_empty() || *bytes.last()? != 0 {
        return None;
    }
    let mut parts = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let terminator = parts.pop()?;
    if !terminator.is_empty() {
        return None;
    }
    let argv = parts
        .into_iter()
        .map(|argument| OsString::from_vec(argument.to_vec()))
        .collect::<Vec<_>>();
    (!argv.is_empty()).then_some(argv)
}

#[cfg(target_vendor = "apple")]
pub fn get_process_argv(pid: u32) -> Option<Vec<OsString>> {
    use std::os::raw::{c_int, c_void};
    use std::os::unix::ffi::OsStringExt;

    const CTL_KERN: c_int = 1;
    const KERN_ARGMAX: c_int = 8;
    const KERN_PROCARGS2: c_int = 49;

    let pid = c_int::try_from(pid).ok()?;
    let mut argmax: c_int = 0;
    let mut argmax_size = std::mem::size_of::<c_int>();
    let mut argmax_mib = [CTL_KERN, KERN_ARGMAX];
    // SAFETY: the MIB and output pointers describe initialized writable
    // storage of the exact lengths supplied to sysctl.
    if unsafe {
        libc::sysctl(
            argmax_mib.as_mut_ptr(),
            u32::try_from(argmax_mib.len()).ok()?,
            (&mut argmax as *mut c_int).cast::<c_void>(),
            &mut argmax_size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || argmax <= 0
    {
        return None;
    }
    let mut buffer = vec![0_u8; usize::try_from(argmax).ok()?];
    let mut size = buffer.len();
    let mut args_mib = [CTL_KERN, KERN_PROCARGS2, pid];
    // SAFETY: the MIB is valid for KERN_PROCARGS2 and `buffer` is writable for
    // the byte count carried in `size`.
    if unsafe {
        libc::sysctl(
            args_mib.as_mut_ptr(),
            u32::try_from(args_mib.len()).ok()?,
            buffer.as_mut_ptr().cast::<c_void>(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size < std::mem::size_of::<c_int>()
    {
        return None;
    }
    buffer.truncate(size);
    let argc = c_int::from_ne_bytes(buffer.get(..4)?.try_into().ok()?);
    let argc = usize::try_from(argc).ok()?;
    if argc == 0 || argc > 65_536 {
        return None;
    }
    let mut cursor = std::mem::size_of::<c_int>();
    cursor += buffer.get(cursor..)?.iter().position(|byte| *byte == 0)? + 1;
    while buffer.get(cursor) == Some(&0) {
        cursor += 1;
    }
    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        let remaining = buffer.get(cursor..)?;
        let length = remaining.iter().position(|byte| *byte == 0)?;
        argv.push(OsString::from_vec(remaining[..length].to_vec()));
        cursor = cursor.checked_add(length + 1)?;
    }
    Some(argv)
}

#[cfg(target_os = "windows")]
pub fn get_process_argv(pid: u32) -> Option<Vec<OsString>> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::new().with_cmd(UpdateKind::Always),
    );
    let argv = system.process(pid)?.cmd().to_vec();
    (!argv.is_empty()).then_some(argv)
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple", target_os = "windows")))]
pub fn get_process_argv(_pid: u32) -> Option<Vec<OsString>> {
    None
}

/// Return one live process's argv as losslessly separated UTF-8 tokens.
///
/// Non-UTF-8 argv fails closed because provider/session classification must not
/// reinterpret a lossy executable or session identifier. Callers making an
/// ownership decision must bracket this lookup with exact incarnation reads.
pub(crate) fn get_process_tokens(pid: u32) -> Option<Vec<String>> {
    get_process_argv(pid)?
        .into_iter()
        .map(|argument| argument.into_string().ok())
        .collect()
}

/// Check executable positions in an already separated argv observation.
pub(crate) fn tokens_have_binary(tokens: &[String], name: &str) -> bool {
    tokens
        .iter()
        .take(2)
        .any(|token| token_has_binary(token, name))
}

/// Split the command representation returned by the local process scanner.
/// This is intentionally small and shell-independent: it preserves simple
/// single/double-quoted executable paths without evaluating escapes or shell
/// expansions.
pub(crate) fn command_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut token_started = false;
    for ch in command.chars() {
        match (quote, ch) {
            (Some(active), value) if value == active => quote = None,
            (None, '\'' | '"') if !token_started => {
                quote = Some(ch);
                token_started = true;
            }
            (None, '\'' | '"') => {
                current.push(ch);
                token_started = true;
            }
            (None, value) if value.is_whitespace() => {
                if token_started {
                    tokens.push(std::mem::take(&mut current));
                    token_started = false;
                }
            }
            _ => {
                current.push(ch);
                token_started = true;
            }
        }
    }
    if token_started {
        tokens.push(current);
    }
    tokens
}

#[cfg(any(test, target_os = "linux", target_os = "windows"))]
fn join_command_args(args: impl IntoIterator<Item = String>) -> String {
    args.into_iter()
        .map(|arg| {
            if arg.is_empty() {
                "\"\"".to_string()
            } else if !arg
                .chars()
                .any(|character| character.is_whitespace() || matches!(character, '\'' | '"'))
            {
                arg
            } else if !arg.contains('"') {
                format!("\"{arg}\"")
            } else if !arg.contains('\'') {
                format!("'{arg}'")
            } else {
                // This representation is used for process classification, not
                // shell execution. Preserve the most useful argv boundaries
                // even for the exceedingly rare argument containing both
                // quote styles.
                arg.chars()
                    .map(|ch| if ch.is_whitespace() { '_' } else { ch })
                    .collect()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Return the current working directory of a process.
///
/// Linux exposes this directly through procfs, Windows is handled by
/// `sysinfo`, and other Unix platforms use the process's `cwd` descriptor
/// reported by `lsof`.
#[cfg(target_os = "linux")]
pub fn get_process_cwd(pid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(target_os = "windows")]
pub fn get_process_cwd(pid: u32) -> Option<String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::new().with_cwd(UpdateKind::Always),
    );
    system
        .process(pid)
        .and_then(|process| process.cwd())
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
pub fn get_process_cwd(pid: u32) -> Option<String> {
    // `-a` ANDs the selectors. Without it, lsof can return cwd descriptors
    // for unrelated processes as well as descriptors belonging to `pid`.
    let pid = pid.to_string();
    let output = Command::new("lsof")
        .args(["-a", "-p", &pid, "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix('n').filter(|path| !path.is_empty()))
        .map(str::to_string)
}

/// Return the process start time as milliseconds since the Unix epoch.
#[cfg(target_os = "linux")]
pub fn get_process_started_at_ms(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field is parenthesized and may itself contain spaces or `)`.
    let fields: Vec<&str> = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect();
    // After comm: state is field 3, while starttime is field 22.
    let start_ticks = fields.get(19)?.parse::<u64>().ok()?;
    let boot_secs = fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse::<u64>()
        .ok()?;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        return None;
    }

    let start_offset_ms = (u128::from(start_ticks) * 1_000) / ticks_per_second as u128;
    let started_at = u128::from(boot_secs) * 1_000 + start_offset_ms;
    u64::try_from(started_at).ok()
}

#[cfg(target_os = "windows")]
pub fn get_process_started_at_ms(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::new(),
    );
    let started_at = system.process(pid)?.start_time();
    (started_at != 0).then(|| started_at.saturating_mul(1_000))
}

#[cfg(target_os = "macos")]
pub fn get_process_started_at_ms(pid: u32) -> Option<u64> {
    use proc_pidinfo::{proc_pidinfo, Pid, ProcBSDInfo};

    let info = proc_pidinfo::<ProcBSDInfo>(Pid(pid)).ok().flatten()?;
    if info.pbi_start_tvsec == 0 || info.pbi_start_tvusec >= 1_000_000 {
        return None;
    }

    info.pbi_start_tvsec
        .checked_mul(1_000)?
        .checked_add(info.pbi_start_tvusec / 1_000)
}

#[cfg(all(
    not(target_os = "linux"),
    not(target_os = "windows"),
    not(target_os = "macos")
))]
pub fn get_process_started_at_ms(pid: u32) -> Option<u64> {
    use chrono::{Local, LocalResult, NaiveDateTime, TimeZone};

    let pid = pid.to_string();
    let output = Command::new("ps")
        .args(["-p", &pid, "-o", "lstart="])
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let naive = NaiveDateTime::parse_from_str(raw.trim(), "%a %b %e %H:%M:%S %Y").ok()?;
    let started_at = match Local.from_local_datetime(&naive) {
        LocalResult::Single(value) => value,
        // A process can start during the repeated hour at the end of DST.
        // Choosing the earlier occurrence is conservative for PID-reuse checks.
        LocalResult::Ambiguous(earlier, later) => earlier.min(later),
        LocalResult::None => return None,
    };
    u64::try_from(started_at.timestamp_millis()).ok()
}

/// Return an opaque, exact process-incarnation marker for `pid`.
///
/// Unlike [`get_process_started_at_ms`], this value is never rounded or
/// reconstructed from a wall-clock estimate. Callers must treat it as opaque
/// and compare it only for exact equality while also matching the PID.
#[cfg(target_os = "linux")]
pub fn get_process_incarnation(pid: u32) -> Option<String> {
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let boot_id = validated_linux_boot_id(&boot_id)?;
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let start_ticks = parse_linux_proc_start_ticks(&stat)?;
    Some(format!("linux:{boot_id}:{start_ticks}"))
}

#[cfg(any(test, target_os = "linux"))]
fn validated_linux_boot_id(raw: &str) -> Option<String> {
    let boot_id = raw.trim();
    if boot_id.len() != 36 {
        return None;
    }
    for (index, byte) in boot_id.bytes().enumerate() {
        let expected_hyphen = matches!(index, 8 | 13 | 18 | 23);
        if (expected_hyphen && byte != b'-') || (!expected_hyphen && !byte.is_ascii_hexdigit()) {
            return None;
        }
    }
    Some(boot_id.to_ascii_lowercase())
}

#[cfg(any(test, target_os = "linux"))]
fn parse_linux_proc_start_ticks(stat: &str) -> Option<u64> {
    // `comm` is parenthesized and may contain spaces and `)`, so the final
    // closing parenthesis is the only safe boundary before the fixed fields.
    let fields: Vec<&str> = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect();
    // After `comm`, state is field 3 and starttime is field 22.
    fields.get(19)?.parse().ok()
}

#[cfg(target_os = "macos")]
pub fn get_process_incarnation(pid: u32) -> Option<String> {
    use proc_pidinfo::{proc_pidinfo, Pid, ProcBSDInfo};

    let info = proc_pidinfo::<ProcBSDInfo>(Pid(pid)).ok().flatten()?;
    if info.pbi_pid != Pid(pid) {
        return None;
    }
    format_macos_process_incarnation(info.pbi_start_tvsec, info.pbi_start_tvusec)
}

#[cfg(any(test, target_os = "macos"))]
fn format_macos_process_incarnation(start_sec: u64, start_usec: u64) -> Option<String> {
    if start_sec == 0 || start_usec >= 1_000_000 {
        return None;
    }
    Some(format!("macos:{start_sec}:{start_usec}"))
}

#[cfg(target_os = "windows")]
pub fn get_process_incarnation(pid: u32) -> Option<String> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: `pid` is passed by value and no pointers cross this call.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }

    let mut creation_time = MaybeUninit::<FILETIME>::uninit();
    let mut exit_time = MaybeUninit::<FILETIME>::uninit();
    let mut kernel_time = MaybeUninit::<FILETIME>::uninit();
    let mut user_time = MaybeUninit::<FILETIME>::uninit();
    // SAFETY: the handle is live, each output points to writable storage, and
    // all four values are read only after GetProcessTimes reports success.
    let read_succeeded = unsafe {
        GetProcessTimes(
            handle,
            creation_time.as_mut_ptr(),
            exit_time.as_mut_ptr(),
            kernel_time.as_mut_ptr(),
            user_time.as_mut_ptr(),
        )
    } != 0;
    // SAFETY: OpenProcess returned this non-null owned handle exactly once.
    let close_succeeded = unsafe { CloseHandle(handle) } != 0;
    if !read_succeeded || !close_succeeded {
        return None;
    }

    // SAFETY: GetProcessTimes initialized every output on success.
    let creation_time = unsafe { creation_time.assume_init() };
    Some(format_windows_process_incarnation(
        creation_time.dwHighDateTime,
        creation_time.dwLowDateTime,
    ))
}

#[cfg(any(test, target_os = "windows"))]
fn format_windows_process_incarnation(high: u32, low: u32) -> String {
    let creation_filetime = (u64::from(high) << 32) | u64::from(low);
    format!("windows:{creation_filetime}")
}

#[cfg(all(
    not(target_os = "linux"),
    not(target_os = "macos"),
    not(target_os = "windows")
))]
pub fn get_process_incarnation(_pid: u32) -> Option<String> {
    None
}

/// Read one environment variable from a running process.
///
/// Access can legitimately fail because of OS privacy controls or because the
/// process exits while it is being inspected; callers should treat `None` as
/// an unavailable value rather than as an error.
#[cfg(target_os = "linux")]
pub fn read_process_env_var(pid: u32, name: &str) -> Option<String> {
    let data = fs::read(format!("/proc/{pid}/environ")).ok()?;
    parse_nul_env_var(&data, name)
}

#[cfg(target_os = "windows")]
pub fn read_process_env_var(pid: u32, name: &str) -> Option<String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    if !valid_env_name(name) {
        return None;
    }
    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::new().with_environ(UpdateKind::Always),
    );
    system.process(pid)?.environ().iter().find_map(|entry| {
        let entry = entry.to_string_lossy();
        let (key, value) = entry.split_once('=')?;
        key.eq_ignore_ascii_case(name).then(|| value.to_string())
    })
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
pub fn read_process_env_var(pid: u32, name: &str) -> Option<String> {
    if !valid_env_name(name) {
        return None;
    }
    // This is best-effort: BSD ps can withhold or truncate another process's
    // environment, and whitespace in values cannot be represented reliably.
    let pid = pid.to_string();
    let output = Command::new("ps").args(["eww", "-p", &pid]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_ps_env_var(&String::from_utf8_lossy(&output.stdout), name)
}

fn valid_env_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('=') && !name.bytes().any(|byte| byte == 0)
}

#[cfg(any(test, target_os = "linux"))]
fn parse_nul_env_var(data: &[u8], name: &str) -> Option<String> {
    if !valid_env_name(name) {
        return None;
    }
    let name = name.as_bytes();
    data.split(|byte| *byte == 0).find_map(|entry| {
        (entry.len() > name.len() && entry.starts_with(name) && entry[name.len()] == b'=')
            .then(|| String::from_utf8(entry[name.len() + 1..].to_vec()).ok())
            .flatten()
    })
}

#[cfg(any(test, all(not(target_os = "linux"), not(target_os = "windows"))))]
fn parse_ps_env_var(output: &str, name: &str) -> Option<String> {
    if !valid_env_name(name) {
        return None;
    }
    let prefix = format!("{name}=");
    output
        .split_whitespace()
        .find_map(|token| token.strip_prefix(&prefix).map(str::to_string))
}

/// Resolve all symlinks in /proc/{pid}/fd, returning their targets.
/// Used by both port discovery (socket inodes) and Codex JSONL discovery.
#[cfg(target_os = "linux")]
pub fn scan_proc_fds(pid: u32) -> Vec<std::path::PathBuf> {
    let fd_dir = format!("/proc/{}/fd", pid);
    let entries = match fs::read_dir(&fd_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    entries
        .flatten()
        .filter_map(|e| fs::read_link(e.path()).ok())
        .collect()
}

#[cfg(target_os = "linux")]
pub fn get_process_info() -> HashMap<u32, ProcInfo> {
    let mut map = HashMap::new();

    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;

    let uptime_secs: f64 = fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0);

    let entries = match fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return map,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid: u32 = match name.to_str().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };

        // /proc/{pid}/stat - parse fields after (comm)
        let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(s) => s,
            Err(_) => continue,
        };
        // comm can contain spaces/parens, so find last ')'
        let after_comm = match stat.rfind(')') {
            Some(pos) if pos + 2 < stat.len() => &stat[pos + 2..],
            _ => continue,
        };
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        // fields[0]=state, [1]=ppid, [11]=utime, [12]=stime, [19]=starttime, [21]=rss
        if fields.len() < 22 {
            continue;
        }
        let ppid: u32 = fields[1].parse().unwrap_or(0);
        let utime: u64 = fields[11].parse().unwrap_or(0);
        let stime: u64 = fields[12].parse().unwrap_or(0);
        let starttime: u64 = fields[19].parse().unwrap_or(0);
        let rss_pages: u64 = fields[21].parse().unwrap_or(0);

        let rss_kb = rss_pages * page_size / 1024;

        // CPU%: lifetime average (total CPU time / wall time).
        // This differs from ps's instantaneous %CPU and is used only as a
        // best-effort signal that a validated descendant is active. Lifecycle
        // collectors must never infer Waiting or Idle from low CPU usage.
        let uptime_ticks = (uptime_secs * clk_tck) as u64;
        let elapsed_ticks = uptime_ticks.saturating_sub(starttime);
        let cpu_pct = if elapsed_ticks > 0 {
            ((utime + stime) as f64 / elapsed_ticks as f64) * 100.0
        } else {
            0.0
        };

        // /proc/{pid}/cmdline is NUL-separated. Re-quote arguments containing
        // whitespace so downstream provider parsers do not mistake prompt
        // words for CLI subcommands.
        let command = fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .map(|bytes| {
                join_command_args(
                    bytes
                        .split(|byte| *byte == 0)
                        .filter(|arg| !arg.is_empty())
                        .map(|arg| String::from_utf8_lossy(arg).into_owned()),
                )
            })
            .unwrap_or_default();
        if command.is_empty() {
            continue; // kernel thread, skip
        }

        map.insert(
            pid,
            ProcInfo {
                pid,
                ppid,
                rss_kb,
                cpu_pct,
                command,
            },
        );
    }
    map
}

#[cfg(target_os = "windows")]
pub fn get_process_info() -> HashMap<u32, ProcInfo> {
    use std::sync::{Mutex, OnceLock};

    // sysinfo's `cpu_usage()` is a delta between two refreshes — a freshly
    // constructed `System` always reports 0. Hold one across calls so the
    // second tick onward returns real CPU%, instead of every Windows process
    // looking inactive to `has_active_descendant`. Low CPU remains
    // insufficient evidence for Waiting or Idle.
    static SYS: OnceLock<Mutex<sysinfo::System>> = OnceLock::new();
    let sys_mutex = SYS.get_or_init(|| Mutex::new(sysinfo::System::new()));
    let mut sys = sys_mutex
        .lock()
        .expect("process-info system mutex poisoned");

    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::new()
            .with_cpu()
            .with_memory()
            .with_cmd(sysinfo::UpdateKind::Always),
    );

    let mut map = HashMap::new();
    for (pid, proc_) in sys.processes() {
        let pid_u32 = pid.as_u32();
        // cmd() can be empty on Windows (cmdline retrieval failed for this
        // process); fall back to the executable name so cmd_has_binary still
        // matches `claude` / `codex` for those processes.
        let command = if proc_.cmd().is_empty() {
            proc_.name().to_string_lossy().into_owned()
        } else {
            join_command_args(proc_.cmd().iter().map(|s| s.to_string_lossy().into_owned()))
        };
        if command.is_empty() {
            continue;
        }
        map.insert(
            pid_u32,
            ProcInfo {
                pid: pid_u32,
                ppid: proc_.parent().map(|p| p.as_u32()).unwrap_or(0),
                rss_kb: proc_.memory() / 1024,
                cpu_pct: proc_.cpu_usage() as f64,
                command,
            },
        );
    }
    map
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
pub fn get_process_info() -> HashMap<u32, ProcInfo> {
    let mut map = HashMap::new();
    let output = Command::new("ps")
        .args(["-ww", "-eo", "pid,ppid,rss,%cpu,command"])
        .output()
        .ok();

    if let Some(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 5 {
                if let (Ok(pid), Ok(ppid), Ok(rss)) = (
                    parts[0].parse::<u32>(),
                    parts[1].parse::<u32>(),
                    parts[2].parse::<u64>(),
                ) {
                    let cpu = parts[3].parse::<f64>().unwrap_or(0.0);
                    let command = parts[4..].join(" ");
                    map.insert(
                        pid,
                        ProcInfo {
                            pid,
                            ppid,
                            rss_kb: rss,
                            cpu_pct: cpu,
                            command,
                        },
                    );
                }
            }
        }
    }
    map
}

pub fn get_children_map(procs: &HashMap<u32, ProcInfo>) -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for proc in procs.values() {
        children.entry(proc.ppid).or_default().push(proc.pid);
    }
    children
}

/// Walk the ppid chain from `pid` and return true if `ancestor` is reached.
/// Used to identify processes spawned by abtop itself (e.g. `claude --print`
/// summary children) so they can be filtered without dropping unrelated
/// non-interactive sessions started by the user.
pub fn is_descendant_of(pid: u32, ancestor: u32, process_info: &HashMap<u32, ProcInfo>) -> bool {
    if pid == 0 || ancestor == 0 || pid == ancestor {
        return false;
    }
    let mut current = pid;
    let mut visited = std::collections::HashSet::new();
    while visited.insert(current) {
        let Some(info) = process_info.get(&current) else {
            return false;
        };
        if info.ppid == ancestor {
            return true;
        }
        if info.ppid == 0 || info.ppid == 1 {
            return false;
        }
        current = info.ppid;
    }
    false
}

pub fn has_active_descendant(
    pid: u32,
    children_map: &HashMap<u32, Vec<u32>>,
    process_info: &HashMap<u32, ProcInfo>,
    cpu_threshold: f64,
) -> bool {
    let mut stack = vec![pid];
    let mut visited = std::collections::HashSet::new();
    while let Some(p) = stack.pop() {
        if !visited.insert(p) {
            continue;
        }
        if let Some(kids) = children_map.get(&p) {
            for &kid in kids {
                if process_info
                    .get(&kid)
                    .is_some_and(|p| p.cpu_pct > cpu_threshold)
                {
                    return true;
                }
                stack.push(kid);
            }
        }
    }
    false
}

/// On Linux, parse /proc/net/tcp[6] for LISTEN sockets, then match inodes
/// via scan_proc_fds. Only scans FDs for PIDs in `known_pids` (from
/// get_process_info) to avoid scanning all 500+ /proc entries.
#[cfg(target_os = "linux")]
pub fn get_listening_ports() -> HashMap<u32, Vec<u16>> {
    // Step 1: Parse /proc/net/tcp + tcp6 for LISTEN sockets -> inode -> port
    let mut inode_to_port: HashMap<u64, u16> = HashMap::new();
    for path in &["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines().skip(1) {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() < 10 || fields[3] != "0A" {
                    continue;
                }
                if let Some(port_hex) = fields[1].rsplit(':').next() {
                    if let Ok(port) = u16::from_str_radix(port_hex, 16) {
                        if let Ok(inode) = fields[9].parse::<u64>() {
                            inode_to_port.insert(inode, port);
                        }
                    }
                }
            }
        }
    }

    if inode_to_port.is_empty() {
        return HashMap::new();
    }

    // Step 2: Scan FDs of all PIDs for matching socket inodes.
    // We scan all /proc PIDs rather than just known agent PIDs because
    // child processes (servers, databases) that own ports may not be in
    // the agent PID set but are still relevant for orphan port detection.
    let mut map: HashMap<u32, Vec<u16>> = HashMap::new();
    let proc_entries = match fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return map,
    };

    for entry in proc_entries.flatten() {
        let pid: u32 = match entry.file_name().to_str().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        for target in scan_proc_fds(pid) {
            let target_str = target.to_string_lossy();
            if let Some(inode_str) = target_str
                .strip_prefix("socket:[")
                .and_then(|s| s.strip_suffix(']'))
            {
                if let Ok(inode) = inode_str.parse::<u64>() {
                    if let Some(&port) = inode_to_port.get(&inode) {
                        map.entry(pid).or_default().push(port);
                    }
                }
            }
        }
    }
    map
}

#[cfg(target_os = "windows")]
pub fn get_listening_ports() -> HashMap<u32, Vec<u16>> {
    let output = Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .output()
        .ok();

    output
        .map(|output| parse_windows_netstat(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default()
}

#[cfg(any(test, target_os = "windows"))]
fn parse_windows_netstat(output: &str) -> HashMap<u32, Vec<u16>> {
    let mut map: HashMap<u32, Vec<u16>> = HashMap::new();

    for line in output.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 5
            || !parts[0].eq_ignore_ascii_case("TCP")
            || !parts[parts.len() - 2].eq_ignore_ascii_case("LISTENING")
        {
            continue;
        }

        // Standard netstat rows start with the protocol, then the local
        // address: `TCP  127.0.0.1:3000  ...  LISTENING  42`.
        let Some(port) = parts[1]
            .rsplit(':')
            .next()
            .and_then(|port| port.parse::<u16>().ok())
        else {
            continue;
        };
        let Some(pid) = parts.last().and_then(|pid| pid.parse::<u32>().ok()) else {
            continue;
        };
        map.entry(pid).or_default().push(port);
    }

    map
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
pub fn get_listening_ports() -> HashMap<u32, Vec<u16>> {
    let mut map: HashMap<u32, Vec<u16>> = HashMap::new();
    let output = Command::new("lsof")
        .args(["-i", "-P", "-n", "-sTCP:LISTEN"])
        .output()
        .ok();

    if let Some(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let is_tcp_listen = parts.len() >= 9 && parts[7] == "TCP" && line.contains("(LISTEN)");
            if is_tcp_listen {
                if let Ok(pid) = parts[1].parse::<u32>() {
                    if let Some(addr) = parts.get(8) {
                        if let Some(port_str) = addr.rsplit(':').next() {
                            if let Ok(port) = port_str.parse::<u16>() {
                                map.entry(pid).or_default().push(port);
                            }
                        }
                    }
                }
            }
        }
    }
    map
}

/// Return the last segment of a path-like string. Splits on `/` everywhere
/// plus `\` on Windows, so non-Windows callers don't accidentally treat
/// backslash as a separator (it's a legal filename character on unix).
pub fn last_path_segment(s: &str) -> Option<&str> {
    #[cfg(windows)]
    let segment = s.rsplit(['/', '\\']).next();
    #[cfg(not(windows))]
    let segment = s.rsplit('/').next();
    segment
}

/// Check if a command string has a given binary name in executable position.
/// Checks the first two argv tokens only (covers direct invocation and
/// interpreter-wrapped scripts like `node /path/to/codex ...`).
///
/// Also matches the autoupdater layout used by Claude Code 2.x where the
/// running binary is named after its version (e.g.
/// `~/.local/share/claude/versions/2.1.121`) — basename equality alone would
/// miss this, so we also accept any path of the form `<...>/<name>/versions/<filename>`.
#[cfg(not(windows))]
pub fn cmd_has_binary(cmd: &str, name: &str) -> bool {
    command_tokens(cmd)
        .into_iter()
        .take(2)
        .any(|token| unix_token_has_binary(&token, name))
}

#[cfg(not(windows))]
pub fn cmd_first_token_has_binary(cmd: &str, name: &str) -> bool {
    command_tokens(cmd)
        .first()
        .is_some_and(|token| unix_token_has_binary(token, name))
}

#[cfg(not(windows))]
fn unix_token_has_binary(tok: &str, name: &str) -> bool {
    let mut iter = tok.rsplit('/');
    let base = iter.next().unwrap_or(tok);
    if base == name {
        return true;
    }
    // Strip .exe suffix for compatibility with claude.exe on non-Windows (e.g. @anthropic-ai/claude-code npm package)
    if let Some(stripped) = base.strip_suffix(".exe") {
        if stripped == name {
            return true;
        }
    }
    matches!((iter.next(), iter.next()), (Some("versions"), Some(parent)) if parent == name)
}

#[cfg(not(windows))]
pub(crate) fn token_has_binary(token: &str, name: &str) -> bool {
    unix_token_has_binary(token, name)
}

/// Windows variant: checks executable-position tokens, splits on `\`, strips a
/// trailing `.exe` and common script extensions (`.js`, `.sh`, `.py`), and
/// matches case-insensitively.
/// Kept separate from the unix impl so non-Windows matching stays exact
/// (`Claude` must not match `claude` on linux/macOS).
#[cfg(windows)]
pub fn cmd_has_binary(cmd: &str, name: &str) -> bool {
    windows_cmd_has_binary(cmd, name)
}

#[cfg(any(test, windows))]
fn windows_cmd_has_binary(cmd: &str, name: &str) -> bool {
    windows_command_tokens(cmd)
        .into_iter()
        .take(2)
        .any(|tok| windows_token_has_binary(&tok, name))
}

#[cfg(windows)]
pub fn cmd_first_token_has_binary(cmd: &str, name: &str) -> bool {
    windows_command_tokens(cmd)
        .first()
        .is_some_and(|tok| windows_token_has_binary(tok, name))
}

#[cfg(any(test, windows))]
fn windows_token_has_binary(tok: &str, name: &str) -> bool {
    let mut iter = tok.rsplit(['/', '\\']);
    let base = iter.next().unwrap_or(tok);
    let normalized = base.to_ascii_lowercase();
    let base = normalized
        .strip_suffix(".exe")
        .or_else(|| normalized.strip_suffix(".js"))
        .or_else(|| normalized.strip_suffix(".sh"))
        .or_else(|| normalized.strip_suffix(".py"))
        .unwrap_or(&normalized);
    if base.eq_ignore_ascii_case(name) {
        return true;
    }
    matches!(
        (iter.next(), iter.next()),
        (Some(versions), Some(parent))
            if versions.eq_ignore_ascii_case("versions") && parent.eq_ignore_ascii_case(name)
    )
}

#[cfg(windows)]
pub(crate) fn token_has_binary(token: &str, name: &str) -> bool {
    windows_token_has_binary(token, name)
}

#[cfg(any(test, windows))]
fn windows_command_tokens(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in cmd.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

pub fn collect_git_stats(cwd: &str) -> (u32, u32) {
    // Validate cwd is an existing directory before running git
    if !std::path::Path::new(cwd).is_dir() {
        return (0, 0);
    }
    let output = Command::new("git")
        .args(["-C", cwd, "status", "--porcelain"])
        .output()
        .ok();

    let mut added = 0u32;
    let mut modified = 0u32;

    if let Some(output) = output {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if line.len() < 2 {
                    continue;
                }
                let status_code = &line[..2];
                if status_code.contains('?') || status_code.contains('A') {
                    added += 1;
                } else if status_code.contains('M') {
                    modified += 1;
                }
            }
        }
    }

    (added, modified)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_tokens_preserve_quoted_executable_paths() {
        assert_eq!(
            command_tokens("\"/Applications/Grok Build/grok\" --resume abc"),
            vec!["/Applications/Grok Build/grok", "--resume", "abc"]
        );
    }

    #[test]
    fn command_arg_join_preserves_whitespace_boundaries() {
        let joined = join_command_args([
            "C:\\Program Files\\nodejs\\node.exe".to_string(),
            "C:\\pkg\\main.mjs".to_string(),
            "prompt with spaces".to_string(),
        ]);
        assert_eq!(
            command_tokens(&joined),
            vec![
                "C:\\Program Files\\nodejs\\node.exe",
                "C:\\pkg\\main.mjs",
                "prompt with spaces"
            ]
        );
    }

    #[test]
    fn windows_classifier_handles_serialized_sysinfo_argv_with_spaces() {
        let command = join_command_args([
            "C:\\Program Files\\nodejs\\node.exe".to_string(),
            "C:\\Users\\GK\\App Data\\Roaming\\npm\\node_modules\\@openai\\codex\\bin\\CODEX.JS"
                .to_string(),
            "--resume".to_string(),
        ]);

        assert!(windows_cmd_has_binary(&command, "codex"));
        assert!(!windows_cmd_has_binary(&command, "claude"));
    }

    #[test]
    fn windows_classifier_strips_extensions_case_insensitively() {
        assert!(windows_cmd_has_binary(
            r#"C:\Tools\CLAUDE.EXE --resume abc"#,
            "claude"
        ));
        assert!(windows_cmd_has_binary(
            r#"node.exe C:\Tools\CODEX.JS --resume abc"#,
            "codex"
        ));
    }

    #[test]
    fn windows_netstat_parser_reads_local_address_column() {
        let output = r#"
  Proto  Local Address          Foreign Address        State           PID
  TCP    0.0.0.0:3000           0.0.0.0:0              LISTENING       4242
  TCP    [::1]:8080             [::]:0                 LISTENING       4242
  TCP    127.0.0.1:9000         127.0.0.1:50000        ESTABLISHED     99
  UDP    0.0.0.0:5353           *:*                                    100
"#;

        let parsed = parse_windows_netstat(output);
        assert_eq!(
            parsed.get(&4242).map(Vec::as_slice),
            Some(&[3000, 8080][..])
        );
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn parse_nul_env_var_matches_exact_name_and_preserves_equals() {
        let data = b"GROK_HOME=/tmp/grok\0TOKEN=a=b=c\0GROK_HOME_EXTRA=nope\0";
        assert_eq!(
            parse_nul_env_var(data, "GROK_HOME").as_deref(),
            Some("/tmp/grok")
        );
        assert_eq!(parse_nul_env_var(data, "TOKEN").as_deref(), Some("a=b=c"));
        assert_eq!(parse_nul_env_var(data, "GROK"), None);
        assert_eq!(parse_nul_env_var(data, ""), None);
    }

    #[test]
    fn parse_ps_env_var_matches_exact_whitespace_delimited_entry() {
        let output = "123 ?? S command GROK_HOME=/tmp/grok OTHER=value";
        assert_eq!(
            parse_ps_env_var(output, "GROK_HOME").as_deref(),
            Some("/tmp/grok")
        );
        assert_eq!(parse_ps_env_var(output, "GROK"), None);
        assert_eq!(parse_ps_env_var(output, "BAD=NAME"), None);
    }

    #[test]
    fn process_helpers_inspect_current_process() {
        let pid = std::process::id();
        let expected_cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let actual_cwd = std::path::PathBuf::from(get_process_cwd(pid).unwrap())
            .canonicalize()
            .unwrap();
        assert_eq!(actual_cwd, expected_cwd);

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let started_at_ms = get_process_started_at_ms(pid).unwrap();
        assert!(started_at_ms > 0);
        assert!(started_at_ms <= now_ms.saturating_add(5_000));
    }

    #[test]
    fn linux_incarnation_parsers_validate_exact_fields() {
        let boot_id = "01234567-89AB-cdef-0123-456789abcdef\n";
        assert_eq!(
            validated_linux_boot_id(boot_id).as_deref(),
            Some("01234567-89ab-cdef-0123-456789abcdef")
        );
        assert_eq!(validated_linux_boot_id("not-a-boot-id"), None);
        assert_eq!(
            validated_linux_boot_id("01234567-89ab-cdef-0123-456789abcdeg"),
            None
        );

        let stat = "4242 (worker ) name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 987654 20";
        assert_eq!(parse_linux_proc_start_ticks(stat), Some(987_654));
        assert_eq!(parse_linux_proc_start_ticks("4242 (short) S 1 2"), None);
        assert_eq!(
            parse_linux_proc_start_ticks(
                "4242 (bad start) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 invalid"
            ),
            None
        );
    }

    #[test]
    fn platform_incarnation_format_preserves_native_precision() {
        assert_eq!(
            format_macos_process_incarnation(1_700_000_000, 42).as_deref(),
            Some("macos:1700000000:42")
        );
        assert_eq!(format_macos_process_incarnation(0, 42), None);
        assert_eq!(format_macos_process_incarnation(1, 1_000_000), None);
        assert_eq!(
            format_windows_process_incarnation(1, 2),
            "windows:4294967298"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn exact_incarnation_is_stable_for_current_process() {
        let pid = std::process::id();
        let first = get_process_incarnation(pid).expect("current process must have an identity");
        let second = get_process_incarnation(pid).expect("current process must remain queryable");
        assert_eq!(first, second);

        #[cfg(target_os = "linux")]
        assert!(first.starts_with("linux:"));
        #[cfg(target_os = "macos")]
        assert!(first.starts_with("macos:"));
        #[cfg(target_os = "windows")]
        assert!(first.starts_with("windows:"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn exact_incarnation_disappears_after_process_is_reaped() {
        #[cfg(windows)]
        let mut child = Command::new("cmd")
            .args(["/C", "exit", "0"])
            .spawn()
            .expect("spawn short-lived child");
        #[cfg(not(windows))]
        let mut child = Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn short-lived child");

        let pid = child.id();
        child.wait().expect("wait for child");
        drop(child);

        for _ in 0..50 {
            if get_process_incarnation(pid).is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("reaped process {pid} remained queryable");
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn process_env_helper_reads_inherited_path() {
        let expected = std::env::var("PATH").unwrap();
        assert_eq!(
            read_process_env_var(std::process::id(), "PATH").as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn cmd_has_binary_basename_match() {
        assert!(cmd_has_binary("/usr/local/bin/claude --foo", "claude"));
        assert!(cmd_has_binary("claude", "claude"));
        assert!(!cmd_has_binary("/usr/local/bin/claude-launch", "claude"));
    }

    #[cfg(not(windows))]
    #[test]
    fn cmd_has_binary_exe_suffix_on_unix() {
        // The @anthropic-ai/claude-code npm package ships a binary named
        // `claude.exe` even on macOS/Linux. Ensure we still detect it.
        assert!(cmd_has_binary(
            "/usr/local/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe --session-id abc",
            "claude",
        ));
        assert!(cmd_has_binary("claude.exe", "claude"));
        // Must not match unrelated .exe binaries
        assert!(!cmd_has_binary("/usr/bin/notclaude.exe", "claude"));
    }

    #[test]
    fn cmd_has_binary_autoupdater_layout() {
        // Claude Code 2.x: actual binary is named after its version, but the
        // path has `<name>/versions/<file>` structure we can match on.
        assert!(cmd_has_binary(
            "/Users/a/.local/share/claude/versions/2.1.121 --allow-dangerously-skip-permissions",
            "claude",
        ));
        assert!(cmd_has_binary("/opt/codex/versions/0.42.0 --foo", "codex",));
    }

    #[test]
    fn cmd_has_binary_does_not_overmatch() {
        // A sibling dir under `claude/` but not under `versions/` shouldn't match.
        assert!(!cmd_has_binary(
            "/Users/a/.local/share/claude/foo",
            "claude"
        ));
        // A `versions/` dir not under `<name>/` shouldn't match either.
        assert!(!cmd_has_binary("/some/versions/2.1.121", "claude"));
    }

    #[cfg(windows)]
    #[test]
    fn cmd_has_binary_windows_detects_node_wrapped_codex() {
        assert!(cmd_has_binary(
            r#""C:\Program Files\nodejs\node.exe" C:\Users\GK\AppData\Roaming\npm\node_modules\@openai\codex\bin\codex.js -m gpt-5.5"#,
            "codex",
        ));
    }

    #[cfg(windows)]
    #[test]
    fn cmd_has_binary_windows_ignores_codex_in_later_args() {
        assert!(!cmd_has_binary(
            r#""C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile "C:\Users\GK\AppData\Roaming\npm\node_modules\@openai\codex\bin\codex.js""#,
            "codex",
        ));
    }

    fn proc(pid: u32, ppid: u32) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            rss_kb: 0,
            cpu_pct: 0.0,
            command: "x".to_string(),
        }
    }

    #[test]
    fn is_descendant_of_direct_child() {
        let mut m = HashMap::new();
        m.insert(10, proc(10, 1));
        m.insert(20, proc(20, 10));
        assert!(is_descendant_of(20, 10, &m));
    }

    #[test]
    fn is_descendant_of_walks_chain() {
        let mut m = HashMap::new();
        m.insert(10, proc(10, 1));
        m.insert(20, proc(20, 10));
        m.insert(30, proc(30, 20));
        assert!(is_descendant_of(30, 10, &m));
    }

    #[test]
    fn is_descendant_of_unrelated_returns_false() {
        let mut m = HashMap::new();
        m.insert(10, proc(10, 1));
        m.insert(20, proc(20, 1));
        assert!(!is_descendant_of(20, 10, &m));
    }

    #[test]
    fn is_descendant_of_self_returns_false() {
        let mut m = HashMap::new();
        m.insert(10, proc(10, 1));
        assert!(!is_descendant_of(10, 10, &m));
    }

    #[test]
    fn is_descendant_of_zero_ancestor_or_pid_returns_false() {
        let m: HashMap<u32, ProcInfo> = HashMap::new();
        assert!(!is_descendant_of(0, 10, &m));
        assert!(!is_descendant_of(10, 0, &m));
    }

    #[test]
    fn is_descendant_of_handles_cycle() {
        // Synthetic cycle (real /proc shouldn't produce one, but be safe).
        let mut m = HashMap::new();
        m.insert(10, proc(10, 20));
        m.insert(20, proc(20, 10));
        assert!(!is_descendant_of(10, 99, &m));
    }

    #[test]
    fn is_descendant_of_missing_ancestor_in_chain() {
        // ppid points at a PID that no longer exists (parent already exited).
        let mut m = HashMap::new();
        m.insert(20, proc(20, 99));
        assert!(!is_descendant_of(20, 7, &m));
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple", target_os = "windows"))]
    #[test]
    fn current_process_executable_is_absolute_and_stable() {
        let first = get_process_executable(std::process::id())
            .expect("current process executable must be queryable");
        let second = get_process_executable(std::process::id())
            .expect("current process executable must remain queryable");
        assert!(first.is_absolute());
        assert_eq!(first, second);
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple", target_os = "windows"))]
    #[test]
    fn current_process_argv_is_exact_and_stable() {
        let expected = std::env::args_os().collect::<Vec<_>>();
        let first =
            get_process_argv(std::process::id()).expect("current process argv must be queryable");
        let second = get_process_argv(std::process::id())
            .expect("current process argv must remain queryable");
        assert_eq!(first, expected);
        assert_eq!(second, expected);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn live_process_argv_preserves_an_empty_argument() {
        #[cfg(unix)]
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "read _", "abtop-argv-probe", ""])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn argv probe");
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/D", "/Q", "/K", "rem", ""])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn argv probe");

        let mut observed = None;
        for _ in 0..100 {
            observed = get_process_argv(child.id())
                .filter(|argv| argv.iter().any(|argument| argument.is_empty()));
            if observed.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();

        let argv = observed.expect("live OS argv must preserve the empty argument");
        assert!(argv.iter().any(|argument| argument.is_empty()));
    }

    #[cfg(unix)]
    #[test]
    fn exact_cmdline_parser_preserves_empty_arguments() {
        use std::os::unix::ffi::OsStrExt;

        let argv = parse_linux_cmdline(b"/native/codex\0--remote\0\0frontend\0\0")
            .expect("terminated cmdline must parse");
        assert_eq!(argv.len(), 5);
        assert_eq!(argv[2].as_os_str().as_bytes(), b"");
        assert_eq!(argv[4].as_os_str().as_bytes(), b"");
        assert!(parse_linux_cmdline(b"/native/codex\0unterminated").is_none());
    }
}
