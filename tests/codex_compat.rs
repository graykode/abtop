#![cfg(unix)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const LEGACY_BINARY_ENV: &str = "ABTOP_MANAGED_CODEX_BINARY";

fn write_executable(directory: &Path, name: &str, source: &[u8]) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, source).expect("write native Codex test executable");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
        .expect("make native Codex test executable private");
    path
}

fn compatibility_command(root: &Path, native_binary: Option<&OsStr>) -> Command {
    let home = root.join("home");
    let codex_home = root.join("codex-home");
    let config_home = root.join("config");
    let cache_home = root.join("cache");
    for directory in [&home, &codex_home, &config_home, &cache_home] {
        fs::create_dir(directory).expect("create isolated test directory");
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .expect("keep isolated test directory private");
    }

    let mut command = Command::new(env!("CARGO_BIN_EXE_abtop"));
    command
        .arg("codex")
        .arg("--")
        .env("HOME", home)
        .env("CODEX_HOME", codex_home)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_CACHE_HOME", cache_home)
        .env_remove(LEGACY_BINARY_ENV);
    if let Some(binary) = native_binary {
        command.env(LEGACY_BINARY_ENV, binary);
    }
    command
}

fn run_with_input(mut command: Command, input: &[u8]) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch abtop compatibility trampoline");
    child
        .stdin
        .take()
        .expect("open child stdin")
        .write_all(input)
        .expect("write child stdin");
    child
        .wait_with_output()
        .expect("wait for compatibility trampoline")
}

#[test]
fn preserves_exact_os_arguments_without_shell_interpretation() {
    let temp = tempfile::tempdir().expect("create private temporary directory");
    let native = write_executable(
        temp.path(),
        "native-codex",
        b"#!/bin/sh\nfor argument do\n    printf '%s\\0' \"$argument\"\ndone\n",
    );
    let arguments = vec![
        OsString::from("--yolo"),
        OsString::from(""),
        OsString::from("space separated"),
        OsString::from("*?[literal]"),
        OsString::from("single'quote"),
        OsString::from("double\"quote"),
        OsString::from_vec(b"non-utf8-\xff-value".to_vec()),
    ];

    let mut command = compatibility_command(temp.path(), Some(native.as_os_str()));
    command.args(&arguments);
    let output = command.output().expect("run compatibility trampoline");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = arguments
        .iter()
        .flat_map(|argument| {
            let mut bytes = argument.clone().into_vec();
            bytes.push(0);
            bytes
        })
        .collect::<Vec<_>>();
    assert_eq!(output.stdout, expected);
    assert!(output.stderr.is_empty());
}

#[test]
fn preserves_argv_zero_for_a_multicall_codex_symlink() {
    let temp = tempfile::tempdir().expect("create private temporary directory");
    let multicall = write_executable(
        temp.path(),
        "multicall",
        b"#!/bin/sh\ncase \"$0\" in\n    */codex) printf 'codex applet' ;;\n    *) printf 'wrong applet: %s' \"$0\" >&2; exit 43 ;;\nesac\n",
    );
    let codex = temp.path().join("codex");
    symlink(&multicall, &codex).expect("create argv0-sensitive Codex shim");

    let output = compatibility_command(temp.path(), Some(codex.as_os_str()))
        .output()
        .expect("run compatibility trampoline through multicall shim");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"codex applet");
    assert!(output.stderr.is_empty());
}

#[test]
fn inherits_standard_streams_byte_for_byte() {
    let temp = tempfile::tempdir().expect("create private temporary directory");
    let native = write_executable(
        temp.path(),
        "native-codex",
        b"#!/bin/sh\n/bin/cat\nprintf 'native stderr' >&2\n",
    );
    let input = b"stdin with spaces, quotes '\" and binary: \x00\xff\n";

    let command = compatibility_command(temp.path(), Some(native.as_os_str()));
    let output = run_with_input(command, input);

    assert!(output.status.success());
    assert_eq!(output.stdout, input);
    assert_eq!(output.stderr, b"native stderr");
}

#[test]
fn preserves_native_exit_code() {
    let temp = tempfile::tempdir().expect("create private temporary directory");
    let native = write_executable(temp.path(), "native-codex", b"#!/bin/sh\nexit 37\n");

    let status = compatibility_command(temp.path(), Some(native.as_os_str()))
        .status()
        .expect("run compatibility trampoline");

    assert_eq!(status.code(), Some(37));
}

#[test]
fn preserves_native_signal_termination() {
    let temp = tempfile::tempdir().expect("create private temporary directory");
    let native = write_executable(
        temp.path(),
        "native-codex",
        b"#!/bin/sh\nkill -TERM \"$$\"\nexit 99\n",
    );

    let status = compatibility_command(temp.path(), Some(native.as_os_str()))
        .status()
        .expect("run compatibility trampoline");

    assert_eq!(status.signal(), Some(libc::SIGTERM));
    assert_eq!(status.code(), None);
}

#[test]
fn removes_the_private_binary_variable_from_the_native_environment() {
    let temp = tempfile::tempdir().expect("create private temporary directory");
    let native = write_executable(
        temp.path(),
        "native-codex",
        b"#!/bin/sh\nif [ \"${ABTOP_MANAGED_CODEX_BINARY+x}\" = x ]; then\n    printf 'private compatibility variable leaked' >&2\n    exit 71\nfi\n",
    );

    let output = compatibility_command(temp.path(), Some(native.as_os_str()))
        .output()
        .expect("run compatibility trampoline");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn creates_no_runtime_or_plugin_state() {
    let temp = tempfile::tempdir().expect("create private temporary directory");
    let native = write_executable(temp.path(), "native-codex", b"#!/bin/sh\nexit 0\n");

    let output = compatibility_command(temp.path(), Some(native.as_os_str()))
        .output()
        .expect("run compatibility trampoline");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for directory in ["home", "codex-home", "config", "cache"] {
        let path = temp.path().join(directory);
        let mut entries = fs::read_dir(&path).expect("inspect isolated state root");
        assert!(
            entries.next().is_none(),
            "compatibility trampoline wrote state below {}",
            path.display()
        );
    }
}

#[test]
fn rejects_a_missing_captured_binary() {
    let temp = tempfile::tempdir().expect("create private temporary directory");

    let output = compatibility_command(temp.path(), None)
        .output()
        .expect("run compatibility trampoline");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(LEGACY_BINARY_ENV),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn rejects_a_relative_captured_binary() {
    let temp = tempfile::tempdir().expect("create private temporary directory");

    let output = compatibility_command(temp.path(), Some(OsStr::new("relative/codex")))
        .output()
        .expect("run compatibility trampoline");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(LEGACY_BINARY_ENV), "stderr: {stderr}");
    assert!(stderr.contains("absolute"), "stderr: {stderr}");
}
