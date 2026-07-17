#![cfg(unix)]

use sccache::server::ServerInfo;
use tempfile::tempdir;

use std::env::{consts::DLL_SUFFIX, var_os};
use std::ffi::OsString;
use std::fs::{self, File, create_dir, create_dir_all, remove_file, set_permissions};
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

struct IsolatedServer {
    cache_dir: PathBuf,
    server_socket: PathBuf,
}

impl Drop for IsolatedServer {
    fn drop(&mut self) {
        let _ = control_command(&self.cache_dir, &self.server_socket, false)
            .arg("--stop-server")
            .output();
    }
}

#[test]
fn statistics_persist_and_combine_across_execution_modes() {
    let root = tempdir().unwrap();
    let root = root.path();
    let compiler_dir = root.join("rust");
    create_mock_rustc(&compiler_dir);

    let cache_dir = root.join("cache");
    let server_socket = root.join("server.sock");
    let build_dir = root.join("build");
    create_dir(&build_dir).unwrap();
    fs::write(build_dir.join("counter"), b"0").unwrap();
    fs::write(build_dir.join("RUST_FILE.rs"), []).unwrap();

    let compiler_bin = compiler_dir.join("bin");
    run_compile(&build_dir, &compiler_bin, &cache_dir, &server_socket, true);
    remove_file(build_dir.join("RUST_FILE")).unwrap();
    run_compile(&build_dir, &compiler_bin, &cache_dir, &server_socket, true);

    let info = read_stats(&cache_dir, &server_socket, true);
    assert_eq!(info.stats.compile_requests, 2);
    assert_eq!(info.stats.requests_executed, 2);
    assert_eq!(info.stats.cache_hits.get("Rust"), Some(&1));
    assert_eq!(info.stats.cache_misses.get("Rust"), Some(&1));
    assert!(
        !server_socket.exists(),
        "serverless compilation started a daemon"
    );

    let output = control_command(&cache_dir, &server_socket, false)
        .arg("--start-server")
        .output()
        .unwrap();
    assert_success(&output, "starting daemon after serverless compiles");
    let _daemon = IsolatedServer {
        cache_dir: cache_dir.clone(),
        server_socket: server_socket.clone(),
    };

    let info = read_stats(&cache_dir, &server_socket, false);
    assert_eq!(info.stats.compile_requests, 2);
    assert_eq!(info.stats.cache_hits.get("Rust"), Some(&1));
    assert_eq!(info.stats.cache_misses.get("Rust"), Some(&1));

    remove_file(build_dir.join("RUST_FILE")).unwrap();
    run_compile(&build_dir, &compiler_bin, &cache_dir, &server_socket, false);

    let info = read_stats(&cache_dir, &server_socket, false);
    assert_eq!(info.stats.compile_requests, 3);
    assert_eq!(info.stats.requests_executed, 3);
    assert_eq!(info.stats.cache_hits.get("Rust"), Some(&2));
    assert_eq!(info.stats.cache_misses.get("Rust"), Some(&1));

    let output = control_command(&cache_dir, &server_socket, true)
        .arg("--zero-stats")
        .output()
        .unwrap();
    assert_success(&output, "zeroing combined statistics");

    let info = read_stats(&cache_dir, &server_socket, false);
    assert_eq!(info.stats.compile_requests, 0);
    assert_eq!(info.stats.requests_executed, 0);
    assert_eq!(info.stats.cache_hits.all(), 0);
    assert_eq!(info.stats.cache_misses.all(), 0);
}

#[test]
fn unsupported_compiler_statistics_are_persisted() {
    let root = tempdir().unwrap();
    let cache_dir = root.path().join("cache");
    let server_socket = root.path().join("server.sock");
    let compiler = root.path().join("unsupported-compiler");
    fs::write(&compiler, "#!/usr/bin/env sh\nexit 1\n").unwrap();
    let mut permissions = compiler.metadata().unwrap().permissions();
    permissions.set_mode(0o755);
    set_permissions(&compiler, permissions).unwrap();

    let output = base_command(&cache_dir, &server_socket, true)
        .current_dir(root.path())
        .arg(&compiler)
        .arg("input")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "unsupported compiler unexpectedly succeeded"
    );

    let info = read_stats(&cache_dir, &server_socket, true);
    assert_eq!(info.stats.compile_requests, 1);
    assert_eq!(info.stats.requests_unsupported_compiler, 1);
}

#[test]
fn statistics_database_is_stored_in_cache_directory() {
    let root = tempdir().unwrap();
    let cache_dir = root.path().join("cache");
    let config_dir = root.path().join("config");
    let server_socket = root.path().join("server.sock");

    let output = control_command(&cache_dir, &server_socket, true)
        .args(["--show-stats", "--stats-format=json"])
        .output()
        .unwrap();
    assert_success(&output, "reading statistics");

    assert!(cache_dir.join("stats.sqlite3").exists());
    assert!(cache_dir.join("stats.sqlite3.lock").exists());
    assert!(
        !config_dir.exists(),
        "statistics command wrote to the config directory"
    );
}

#[test]
fn corrupt_statistics_database_warns_and_rebuilds() {
    let root = tempdir().unwrap();
    let cache_dir = root.path().join("cache");
    create_dir(&cache_dir).unwrap();
    let server_socket = root.path().join("server.sock");
    let stats_path = cache_dir.join("stats.sqlite3");
    fs::write(&stats_path, b"not a sqlite database").unwrap();

    let output = control_command(&cache_dir, &server_socket, true)
        .args(["--show-stats", "--stats-format=json"])
        .output()
        .unwrap();
    assert_success(&output, "reading corrupt statistics database");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("warning: statistics database"),
        "missing corruption warning in stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );

    let info: ServerInfo = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(info.stats.compile_requests, 0);
}
fn run_compile(
    root: &Path,
    compiler_bin: &Path,
    cache_dir: &Path,
    server_socket: &Path,
    serverless: bool,
) {
    let output = compile_command(root, compiler_bin, cache_dir, server_socket, serverless)
        .output()
        .unwrap();
    assert_success(&output, "compiling");
}

fn compile_command(
    root: &Path,
    compiler_bin: &Path,
    cache_dir: &Path,
    server_socket: &Path,
    serverless: bool,
) -> Command {
    let mut paths: OsString = compiler_bin.into();
    paths.push(":");
    paths.push(var_os("PATH").unwrap());

    let mut command = base_command(cache_dir, server_socket, serverless);
    command
        .current_dir(root)
        .env("PATH", paths)
        .arg("rustc")
        .arg("RUST_FILE.rs")
        .arg("--crate-name=sccache_stats_tests")
        .arg("--crate-type=lib")
        .arg("--emit=link")
        .arg("--out-dir")
        .arg(root);
    command
}

fn read_stats(cache_dir: &Path, server_socket: &Path, serverless: bool) -> ServerInfo {
    let output = control_command(cache_dir, server_socket, serverless)
        .args(["--show-stats", "--stats-format=json"])
        .output()
        .unwrap();
    assert_success(&output, "reading statistics");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn control_command(cache_dir: &Path, server_socket: &Path, serverless: bool) -> Command {
    base_command(cache_dir, server_socket, serverless)
}

fn base_command(cache_dir: &Path, server_socket: &Path, serverless: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sccache"));
    for (var, _) in std::env::vars_os() {
        if var.to_string_lossy().starts_with("SCCACHE_") {
            command.env_remove(var);
        }
    }
    command
        .env("SCCACHE_CONF", cache_dir.join("missing-config"))
        .env(
            "SCCACHE_CACHED_CONF",
            cache_dir.with_file_name("config").join("cached-config"),
        )
        .env("SCCACHE_SERVERLESS", serverless.to_string())
        .env("SCCACHE_DIRECTORY_DIR", cache_dir)
        .env("SCCACHE_DIRECTORY_DIRECT", "false")
        .env("SCCACHE_SERVER_UDS", server_socket);
    command
}

fn assert_success(output: &std::process::Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn create_mock_rustc(dir: &Path) {
    let bin = dir.join("bin");
    create_dir_all(&bin).unwrap();

    let dll_name = format!("driver{DLL_SUFFIX}");
    let dll = dir.join(&dll_name);
    fs::write(&dll, dir.as_os_str().as_encoded_bytes()).unwrap();

    let lib = dir.join("lib");
    create_dir(&lib).unwrap();
    symlink(dll, lib.join(&dll_name)).unwrap();

    let rustc = bin.join("rustc");
    write!(
        File::create(&rustc).unwrap(),
        r#"#!/usr/bin/env sh

set -e
build=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        -vV)
            echo rustc 1.0.0
            exec echo "host: unknown"
            ;;
        +stable)
            exit 1
            ;;
        --print=sysroot)
            exec echo {}
            ;;
        --print)
            shift
            if [ "$1" = file-names ]; then
                exec echo RUST_FILE
            fi
            ;;
        --emit)
            shift
            if [ "$1" = dep-info ]; then
                echo "deps.d: RUST_FILE.rs" > "$3"
                exec echo "RUST_FILE.rs:" "$3"
            fi
            ;;
        RUST_FILE.rs)
            build=1
            ;;
    esac
    shift
done

if [ "$build" -eq 1 ]; then
    echo $(($(cat counter) + 1)) > counter
    cp counter RUST_FILE
fi
"#,
        dir.display(),
    )
    .unwrap();

    let mut permissions = rustc.metadata().unwrap().permissions();
    permissions.set_mode(0o755);
    set_permissions(&rustc, permissions).unwrap();
}
