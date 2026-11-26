//! Where sessions live in tmux: the pane whose process tree holds a session's
//! agent process, found by walking up its parents to a pane's process (the
//! same descent `jump::tmux` uses to switch to it). One `tmux list-panes -a`
//! call lists every pane of the default server (or of the one in `$TMUX`),
//! whether or not abtop itself runs inside tmux.

use super::process::ProcInfo;
use crate::model::TmuxLocation;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long `tmux list-panes` may take: a server that does not answer must
/// not freeze the tick, and with it the UI.
const LIST_PANES_TIMEOUT: Duration = Duration::from_secs(2);

/// How often `run_bounded` checks whether its command has exited; tmux
/// usually answers within a few milliseconds.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// The fields listed for each pane, in the order `parse_panes` reads them.
const PANE_FIELDS: [&str; 5] = [
    "pane_pid",
    "session_name",
    "window_index",
    "pane_index",
    "window_name",
];

/// Every pane of the tmux server, keyed by the pid of the pane's process.
/// Empty when tmux is not installed, no server is running, or the command
/// fails: those simply mean "no location". `None` when the server did not
/// answer within `LIST_PANES_TIMEOUT` (the `tmux` client is then killed): no
/// news, so the caller keeps the panes it had.
pub fn list_panes() -> Option<HashMap<u32, TmuxLocation>> {
    let nonce = nonce();
    let format = pane_format(&nonce);
    match run_bounded(
        Command::new("tmux").args(["list-panes", "-a", "-F", &format]),
        LIST_PANES_TIMEOUT,
    ) {
        Bounded::Done(stdout) => Some(parse_panes(&String::from_utf8_lossy(&stdout), &nonce)),
        Bounded::Failed => Some(HashMap::new()),
        Bounded::TimedOut => None,
    }
}

/// 16 hex digits that cannot be guessed before the call: what the std hasher
/// gives for no input under a new `RandomState`, whose keys come from the
/// OS's random source and change with every instance.
fn nonce() -> String {
    use std::hash::{BuildHasher, Hasher};
    let hasher = std::collections::hash_map::RandomState::new().build_hasher();
    format!("{:016x}", hasher.finish())
}

/// The `list-panes -F` format: every field preceded by `nonce`. tmux passes
/// window names through raw, tabs and newlines included, so any separator
/// character could appear in a name and fake a field or a whole record; a
/// nonce made for this call cannot. It also survives tmux's sanitizing of
/// its output for a client whose locale is not UTF-8, which turns control
/// characters such as tabs into `_`.
fn pane_format(nonce: &str) -> String {
    PANE_FIELDS
        .iter()
        .map(|field| format!("{nonce}#{{{field}}}"))
        .collect()
}

/// Parse `list-panes -F pane_format(nonce)` output: split on the nonce, it
/// gives back the fields tmux printed, five per pane, whatever the names
/// contain. Session and window names are user text, so they are sanitized
/// for terminal display. A pane listed more than once (a window linked into
/// several sessions) keeps its first entry.
fn parse_panes(output: &str, nonce: &str) -> HashMap<u32, TmuxLocation> {
    let mut panes = HashMap::new();
    // Whatever comes before the first nonce is not a field.
    let mut fields = output.split(nonce).skip(1);
    while let (Some(pid), Some(session), Some(window), Some(pane), Some(name)) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) {
        let (Ok(pid), Ok(window_index), Ok(pane_index)) =
            (pid.parse(), window.parse(), pane.parse())
        else {
            continue;
        };
        // The last field ends with the newline that ends the record.
        let name = name.strip_suffix('\n').unwrap_or(name);
        panes.entry(pid).or_insert_with(|| TmuxLocation {
            session_name: super::sanitize_terminal_text(session),
            window_index,
            window_name: super::sanitize_terminal_text(name),
            pane_index,
        });
    }
    panes
}

/// What became of a command run by `run_bounded`.
#[derive(Debug, PartialEq, Eq)]
enum Bounded {
    /// Exited successfully, with this output.
    Done(Vec<u8>),
    /// Could not be started, or exited with an error.
    Failed,
    /// Still running when its time ran out: killed and reaped.
    TimedOut,
}

/// Run `cmd`, giving it at most `timeout` to exit. Its output goes to a
/// temporary file rather than to a pipe, which a long listing could fill,
/// stalling the command until it is killed.
fn run_bounded(cmd: &mut Command, timeout: Duration) -> Bounded {
    let Ok(mut output) = tempfile::tempfile() else {
        return Bounded::Failed;
    };
    let spawned = output.try_clone().and_then(|stdout| {
        cmd.stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::null())
            .spawn()
    });
    let Ok(mut child) = spawned else {
        return Bounded::Failed;
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                let read = output
                    .rewind()
                    .and_then(|()| output.read_to_end(&mut stdout));
                return if status.success() && read.is_ok() {
                    Bounded::Done(stdout)
                } else {
                    Bounded::Failed
                };
            }
            Ok(None) if started.elapsed() < timeout => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Bounded::TimedOut;
            }
            // In practice only when something else reaped the child (ECHILD):
            // its pid may already be reused, so it is not killed.
            Err(_) => return Bounded::Failed,
        }
    }
}

/// The pane whose process is `pid` itself or one of its ancestors.
pub fn pane_of(
    pid: u32,
    panes: &HashMap<u32, TmuxLocation>,
    process_info: &HashMap<u32, ProcInfo>,
) -> Option<TmuxLocation> {
    let mut current = pid;
    let mut visited = HashSet::new();
    while current > 1 && visited.insert(current) {
        if let Some(location) = panes.get(&current) {
            return Some(location.clone());
        }
        current = process_info.get(&current)?.ppid;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "5f3a9c0e12b47d86";

    /// One pane as tmux prints it for `pane_format(NONCE)`.
    fn record(pid: &str, session: &str, window: &str, pane: &str, name: &str) -> String {
        record_with(NONCE, pid, session, window, pane, name)
    }

    fn record_with(
        nonce: &str,
        pid: &str,
        session: &str,
        window: &str,
        pane: &str,
        name: &str,
    ) -> String {
        [pid, session, window, pane, name]
            .iter()
            .map(|value| format!("{nonce}{value}"))
            .collect::<String>()
            + "\n"
    }

    fn proc(pid: u32, ppid: u32) -> (u32, ProcInfo) {
        let info = ProcInfo {
            pid,
            ppid,
            rss_kb: 0,
            cpu_pct: 0.0,
            command: "x".to_string(),
        };
        (pid, info)
    }

    fn location(
        session: &str,
        window_index: u32,
        window_name: &str,
        pane_index: u32,
    ) -> TmuxLocation {
        TmuxLocation {
            session_name: session.to_string(),
            window_index,
            window_name: window_name.to_string(),
            pane_index,
        }
    }

    #[test]
    fn pane_format_puts_the_nonce_before_every_field() {
        assert_eq!(
            pane_format("N"),
            "N#{pane_pid}N#{session_name}N#{window_index}N#{pane_index}N#{window_name}"
        );
    }

    #[test]
    fn nonce_is_new_for_each_call_and_safe_in_a_format() {
        let (a, b) = (nonce(), nonce());
        assert_ne!(a, b);
        for n in [a, b] {
            assert_eq!(n.len(), 16);
            assert!(n.chars().all(|c| c.is_ascii_hexdigit()), "{n}");
        }
    }

    #[test]
    fn parse_panes_reads_every_field() {
        let out = [
            record("41285", "dev", "1", "1", "bash"),
            record("15394", "dev", "31", "2", "build"),
            record("42391", "work", "4", "1", "mail client"),
        ]
        .concat();
        let panes = parse_panes(&out, NONCE);
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[&15394], location("dev", 31, "build", 2));
        assert_eq!(panes[&42391], location("work", 4, "mail client", 1));
        assert_eq!(panes[&15394].label(), "dev:build");
    }

    #[test]
    fn parse_panes_handles_tabs_newlines_and_empty_window_names() {
        let out = [
            record("7", "work", "0", "3", "logs\tand\ntests\n"),
            record("8", "work", "2", "0", ""),
            record("9", "work", "5", "1", "\n"),
        ]
        .concat();
        let panes = parse_panes(&out, NONCE);
        // Tabs and newlines don't shift any field; sanitizing drops them.
        assert_eq!(panes[&7], location("work", 0, "logsandtests", 3));
        assert_eq!(panes[&8].window_name, "");
        assert_eq!(panes[&8].label(), "work:2");
        assert_eq!(panes[&9], location("work", 5, "", 1));
    }

    #[test]
    fn parse_panes_ignores_records_forged_in_a_window_name() {
        let forged_names = [
            // A whole record of the old tab-separated format.
            "nl\n200\tforged\t9\t9\tevil\t200\ntail".to_string(),
            // A whole record of this format with a wrong nonce, even a near
            // miss.
            format!(
                "nl\n{}tail",
                record_with("0123456789abcdef", "200", "forged", "9", "9", "evil")
            ),
            format!(
                "nl\n{}tail",
                record_with("5f3a9c0e12b47d87", "200", "forged", "9", "9", "evil")
            ),
        ];
        for name in forged_names {
            let forger = record("100", "dev", "1", "0", &name);
            let real = record("200", "dev", "2", "0", "real");
            for out in [forger.clone() + &real, real.clone() + &forger] {
                let panes = parse_panes(&out, NONCE);
                assert_eq!(panes.len(), 2, "{out:?}");
                assert_eq!(panes[&200], location("dev", 2, "real", 0), "{out:?}");
                let shown = crate::collector::sanitize_terminal_text(&name);
                assert_eq!(panes[&100], location("dev", 1, &shown, 0), "{out:?}");
            }
        }
    }

    #[test]
    fn parse_panes_sanitizes_names_and_skips_bad_records() {
        let out = [
            "garbage\n\n".to_string(),
            record("x", "work", "0", "0", "name"),
            record("9", "work", "one", "0", "name"),
            record("10", "w\u{202E}ork", "1", "0", "bu\u{1b}[31mild"),
            // Cut short.
            format!("{NONCE}11{NONCE}work{NONCE}2"),
        ]
        .concat();
        let panes = parse_panes(&out, NONCE);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[&10], location("work", 1, "bu[31mild", 0));
    }

    #[test]
    fn parse_panes_keeps_the_first_entry_of_a_linked_pane() {
        let out =
            record("5", "alpha", "1", "0", "shared") + &record("5", "beta", "4", "0", "shared");
        assert_eq!(parse_panes(&out, NONCE)[&5].session_name, "alpha");
    }

    #[cfg(unix)]
    #[test]
    fn run_bounded_kills_a_command_that_does_not_end() {
        let started = Instant::now();
        let run = run_bounded(Command::new("sleep").arg("30"), Duration::from_millis(200));
        assert_eq!(run, Bounded::TimedOut);
        // Killed, then reaped: waiting for it to end by itself takes 30 s.
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn run_bounded_returns_the_output_or_a_failure() {
        let timeout = Duration::from_secs(10);
        assert_eq!(
            run_bounded(Command::new("printf").arg("a\tb\n"), timeout),
            Bounded::Done(b"a\tb\n".to_vec())
        );
        assert_eq!(
            run_bounded(&mut Command::new("false"), timeout),
            Bounded::Failed
        );
        assert_eq!(
            run_bounded(&mut Command::new("/nonexistent/tmux"), timeout),
            Bounded::Failed
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_bounded_takes_more_output_than_a_pipe_holds() {
        let run = run_bounded(
            Command::new("head").args(["-c", "300000", "/dev/zero"]),
            Duration::from_secs(10),
        );
        assert_eq!(run, Bounded::Done(vec![0; 300_000]));
    }

    #[test]
    fn pane_of_walks_up_to_the_pane_process() {
        // tmux pane shell 100 → wrapper 200 → claude 300 → bash tool 400 → codex 500
        let procs: HashMap<u32, ProcInfo> = [
            proc(100, 50),
            proc(200, 100),
            proc(300, 200),
            proc(400, 300),
            proc(500, 400),
            proc(600, 1),
        ]
        .into_iter()
        .collect();
        let panes = HashMap::from([(100, location("dev", 31, "build", 2))]);

        let expected = Some(location("dev", 31, "build", 2));
        assert_eq!(pane_of(300, &panes, &procs), expected);
        // A session nested in another one lives in the same pane.
        assert_eq!(pane_of(500, &panes, &procs), expected);
        // The pane's own process can be the agent.
        assert_eq!(pane_of(100, &panes, &procs), expected);
        // Not under any pane, or gone.
        assert_eq!(pane_of(600, &panes, &procs), None);
        assert_eq!(pane_of(999, &panes, &procs), None);
        assert_eq!(pane_of(0, &panes, &procs), None);
    }

    #[test]
    fn pane_of_stops_on_a_ppid_cycle() {
        let procs: HashMap<u32, ProcInfo> = [proc(10, 20), proc(20, 10)].into_iter().collect();
        assert_eq!(pane_of(10, &HashMap::new(), &procs), None);
    }
}
