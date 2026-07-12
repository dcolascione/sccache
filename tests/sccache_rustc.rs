#![cfg(unix)]

use assert_cmd::Command;
use tempfile::tempdir;

use std::{
    env::{consts::DLL_SUFFIX, var_os},
    ffi::OsString,
    fs::{self, File, create_dir, create_dir_all, remove_file, set_permissions},
    io::Write,
    os::unix::{
        fs::symlink,
        prelude::{OsStrExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
};

struct StopServer;
impl Drop for StopServer {
    fn drop(&mut self) {
        let _ = Command::from_std(std::process::Command::new(env!("CARGO_BIN_EXE_sccache")))
            .arg("--stop-server")
            .ok();
    }
}

// (temp dir)
// ├── rust // symlinks to rust1 on the first run and rust2 on the second
// ├── rust1/
// │  ├── bin
// │  │  └── rustc
// │  ├── lib
// │  │  └── driver.so -> ../driver.so
// │  └── driver.so
// ├── rust2/
// │  ├── bin
// │  │  └── rustc
// │  ├── lib
// │  │  └── driver.so -> ../driver.so
// │  └── driver.so
// ├── sccache/
// ├── counter // increases by 1 for every compilation that is not cached
// ├── RUST_FILE // compile output copied from counter, same content means it was cached
// └── RUST_FILE.rs
#[test]
fn test_symlinks() {
    let root = tempdir().unwrap();
    let root = root.path();

    fs::write(root.join("counter"), b"0").unwrap();
    fs::write(root.join("RUST_FILE.rs"), []).unwrap();

    create_mock_rustc(root.join("rust1"));
    create_mock_rustc(root.join("rust2"));

    let rust = root.join("rust");
    let bin = rust.join("bin");
    let out_file = root.join("RUST_FILE");

    symlink(root.join("rust1"), &rust).unwrap();
    drop(StopServer);
    let _stop_server = StopServer;
    run_sccache(root, &bin);
    let output1 = fs::read(&out_file).unwrap();

    remove_file(&rust).unwrap();
    symlink(root.join("rust2"), &rust).unwrap();
    run_sccache(root, &bin);
    let output2 = fs::read(out_file).unwrap();

    assert_ne!(output1, output2);
}

#[test]
fn test_serverless_path_transforms_share_cache_across_worktrees() {
    let root = tempdir().unwrap();
    let root = root.path();
    let compiler_dir = root.join("rust");
    create_mock_rustc(compiler_dir.clone());

    let cache_dir = root.join("sccache");
    let server_socket = root.join("server.sock");
    let config_path = root.join("config");
    let build_a = root.join("codex.foo");
    let build_b = root.join("codex.bar");

    for build_dir in [&build_a, &build_b] {
        create_dir(build_dir).unwrap();
        fs::write(build_dir.join("counter"), b"0").unwrap();
        fs::write(build_dir.join("RUST_FILE.rs"), []).unwrap();
    }

    let from = format!(r"{}/codex\.[^/]+", regex::escape(&root.to_string_lossy()));
    fs::write(
        &config_path,
        format!(
            "[[path_transforms]]\nfrom = '{}'\nto = '/workspace'\n",
            from
        ),
    )
    .unwrap();

    let compiler_bin = compiler_dir.join("bin");
    let output = serverless_path_transform_command(
        &build_a,
        &compiler_bin,
        &cache_dir,
        &server_socket,
        &config_path,
    )
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "first serverless compile failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(fs::read(build_a.join("counter")).unwrap(), b"1\n");

    let output = serverless_path_transform_command(
        &build_b,
        &compiler_bin,
        &cache_dir,
        &server_socket,
        &config_path,
    )
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "normalized serverless compile failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(fs::read(build_b.join("counter")).unwrap(), b"0");
    assert_eq!(fs::read(build_b.join("RUST_FILE")).unwrap(), b"1\n");
    assert!(
        !server_socket.exists(),
        "serverless compilation started a daemon"
    );
}

#[test]
fn test_serverless_directory_cache() {
    let root = tempdir().unwrap();
    let root = root.path();
    let compiler_dir = root.join("rust");
    create_mock_rustc(compiler_dir.clone());

    let cache_dir = root.join("sccache");
    let barrier_dir = root.join("barrier");
    let server_socket = root.join("server.sock");
    create_dir(&barrier_dir).unwrap();

    let build_a = root.join("build-a");
    let build_b = root.join("build-b");
    for build_dir in [&build_a, &build_b] {
        create_dir(build_dir).unwrap();
        fs::write(build_dir.join("counter"), b"0").unwrap();
        fs::write(build_dir.join("RUST_FILE.rs"), []).unwrap();
    }

    let compiler_bin = compiler_dir.join("bin");
    let mut command_a = serverless_sccache_command(
        &build_a,
        &compiler_bin,
        &cache_dir,
        &barrier_dir,
        &server_socket,
        "a",
    );
    let mut command_b = serverless_sccache_command(
        &build_b,
        &compiler_bin,
        &cache_dir,
        &barrier_dir,
        &server_socket,
        "b",
    );
    let child_a = command_a
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let child_b = command_b
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    for (name, output) in [
        ("a", child_a.wait_with_output().unwrap()),
        ("b", child_b.wait_with_output().unwrap()),
    ] {
        assert!(
            output.status.success(),
            "serverless compile {name} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    assert_eq!(fs::read(build_a.join("counter")).unwrap(), b"1\n");
    assert_eq!(fs::read(build_b.join("counter")).unwrap(), b"1\n");

    remove_file(build_a.join("RUST_FILE")).unwrap();
    let output = serverless_sccache_command(
        &build_a,
        &compiler_bin,
        &cache_dir,
        &barrier_dir,
        &server_socket,
        "a",
    )
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "serverless cache-hit compile failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(fs::read(build_a.join("counter")).unwrap(), b"1\n");
    assert!(build_a.join("RUST_FILE").is_file());
    assert!(
        !server_socket.exists(),
        "serverless compilation started a daemon"
    );
}

fn serverless_path_transform_command(
    root: &Path,
    compiler_bin: &Path,
    cache_dir: &Path,
    server_socket: &Path,
    config_path: &Path,
) -> ProcessCommand {
    let mut paths: OsString = compiler_bin.into();
    paths.push(":");
    paths.push(var_os("PATH").unwrap());

    let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_sccache"));
    for (var, _) in std::env::vars_os() {
        if var.to_string_lossy().starts_with("SCCACHE_") {
            command.env_remove(var);
        }
    }
    command
        .current_dir(root)
        .env("PATH", paths)
        .env("SCCACHE_CONF", config_path)
        .env("SCCACHE_CACHED_CONF", cache_dir.join("cached-config"))
        .env("SCCACHE_SERVERLESS", "true")
        .env("SCCACHE_DIRECTORY_DIR", cache_dir)
        .env("SCCACHE_DIRECTORY_DIRECT", "false")
        .env("SCCACHE_TEST_OUTPUT_FILE", "1")
        .env("SCCACHE_TEST_REQUIRE_REMAP", "1")
        .env("SCCACHE_SERVER_UDS", server_socket)
        .arg("rustc")
        .arg("RUST_FILE.rs")
        .arg("--crate-name=sccache_rustc_tests")
        .arg("--crate-type=lib")
        .arg("--emit=link")
        .arg("--out-dir")
        .arg(root);
    command
}

fn serverless_sccache_command(
    root: &Path,
    compiler_bin: &Path,
    cache_dir: &Path,
    barrier_dir: &Path,
    server_socket: &Path,
    build_id: &str,
) -> ProcessCommand {
    let mut paths: OsString = compiler_bin.into();
    paths.push(":");
    paths.push(var_os("PATH").unwrap());

    let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_sccache"));
    for (var, _) in std::env::vars_os() {
        if var.to_string_lossy().starts_with("SCCACHE_") {
            command.env_remove(var);
        }
    }
    command
        .current_dir(root)
        .env("PATH", paths)
        .env("SCCACHE_CONF", root.join("missing-config"))
        .env("SCCACHE_CACHED_CONF", cache_dir.join("cached-config"))
        .env("SCCACHE_SERVERLESS", "true")
        .env("SCCACHE_DIRECTORY_DIR", cache_dir)
        .env("SCCACHE_DIRECTORY_DIRECT", "false")
        .env("SCCACHE_SERVER_UDS", server_socket)
        .env("SCCACHE_TEST_BARRIER_DIR", barrier_dir)
        .env("SCCACHE_TEST_BUILD_ID", build_id)
        .arg("rustc")
        .arg("RUST_FILE.rs")
        .arg("--crate-name=sccache_rustc_tests")
        .arg("--crate-type=lib")
        .arg("--emit=link")
        .arg("--out-dir")
        .arg(root);
    command
}

fn create_mock_rustc(dir: PathBuf) {
    let bin = dir.join("bin");
    create_dir_all(&bin).unwrap();

    let dll_name = format!("driver{DLL_SUFFIX}");
    let dll = dir.join(&dll_name);
    fs::write(&dll, dir.as_os_str().as_bytes()).unwrap();

    let lib = dir.join("lib");
    create_dir(&lib).unwrap();
    symlink(dll, lib.join(&dll_name)).unwrap();

    let rustc = bin.join("rustc");
    write!(
        File::create(&rustc).unwrap(),
        r#"#!/usr/bin/env sh

set -e
build=0
saw_remap=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        -vV)
            echo rustc 1.0.0
            exec echo "host: unknown"
            ;;
        --remap-path-prefix)
            shift
            case "$1" in
                *=/workspace)
                    saw_remap=1
                    ;;
            esac
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
                if [ -n "$SCCACHE_TEST_BARRIER_DIR" ] || [ -n "$SCCACHE_TEST_OUTPUT_FILE" ]; then
                    exec echo RUST_FILE
                fi
                exec echo RUST_FILE.rs
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
    if [ -n "$SCCACHE_TEST_REQUIRE_REMAP" ] && [ "$saw_remap" -ne 1 ]; then
        exit 89
    fi
    if [ -n "$SCCACHE_TEST_BARRIER_DIR" ]; then
        touch "$SCCACHE_TEST_BARRIER_DIR/ready-$SCCACHE_TEST_BUILD_ID"
        waited=0
        while [ ! -f "$SCCACHE_TEST_BARRIER_DIR/ready-a" ] || [ ! -f "$SCCACHE_TEST_BARRIER_DIR/ready-b" ]; do
            waited=$((waited + 1))
            if [ "$waited" -ge 200 ]; then
                exit 88
            fi
            sleep 0.05
        done
    fi
    echo $(($(cat counter) + 1)) > counter
    cp counter RUST_FILE
fi
"#,
        dir.display(),
    )
    .unwrap();

    let mut perm = rustc.metadata().unwrap().permissions();
    perm.set_mode(0o755);
    set_permissions(&rustc, perm).unwrap();
}

fn run_sccache(root: &Path, path: &Path) {
    let mut paths: OsString = path.into();
    paths.push(":");
    paths.push(var_os("PATH").unwrap());

    Command::cargo_bin("sccache")
        .unwrap()
        .current_dir(root)
        .env("PATH", paths)
        .env("SCCACHE_DIR", root.join("sccache"))
        .arg("rustc")
        .arg("RUST_FILE.rs")
        .arg("--crate-name=sccache_rustc_tests")
        .arg("--crate-type=lib")
        .arg("--emit=link")
        .arg("--out-dir")
        .arg(root)
        .unwrap();
}
