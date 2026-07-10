#![cfg(unix)]
#![allow(dead_code, unused_imports)]

mod harness;

use harness::{sccache_client_cfg, sccache_command, write_json_cfg};
use object::read::archive::ArchiveFile;
use object::{Object, ObjectSection};
use sccache::config::FileConfig;
use sccache::path_transform::PathTransformConfig;
use sccache::server::ServerInfo;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use tempfile::tempdir;

#[derive(Clone, Copy)]
enum CompilerKind {
    Rust,
    Cxx,
}

impl CompilerKind {
    fn source_name(self) -> &'static str {
        match self {
            Self::Rust => "lib.rs",
            Self::Cxx => "foo.cc",
        }
    }

    fn source(self) -> &'static str {
        match self {
            Self::Rust => "pub fn answer() -> u32 { 42 }\n",
            Self::Cxx => "unsigned answer() { return 42; }\n",
        }
    }

    fn artifact_name(self) -> &'static str {
        match self {
            Self::Rust => "libpath_transform_e2e.rlib",
            Self::Cxx => "foo.o",
        }
    }

    fn stats_language(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Cxx => "C/C++",
        }
    }
}

struct DaemonGuard {
    config: PathBuf,
    cached_config: PathBuf,
    socket: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = sccache_command()
            .arg("--stop-server")
            .env("SCCACHE_CONF", &self.config)
            .env("SCCACHE_CACHED_CONF", &self.cached_config)
            .env("SCCACHE_SERVER_UDS", &self.socket)
            .output();
    }
}

fn assert_success(output: Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn append_object_debug_sections(data: &[u8], debug_info: &mut Vec<u8>) {
    let Ok(file) = object::File::parse(data) else {
        return;
    };

    for section in file.sections() {
        let Ok(name) = section.name_bytes() else {
            continue;
        };
        if memchr::memmem::find(name, b"debug").is_none() {
            continue;
        }
        let data = section.uncompressed_data().unwrap();
        debug_info.extend_from_slice(&data);
        debug_info.push(0);
    }
}

fn artifact_debug_info(artifact: &[u8]) -> Vec<u8> {
    let mut debug_info = Vec::new();
    // Rust rlibs are archives; C++ compiler outputs are object files.
    if matches!(
        object::FileKind::parse(artifact),
        Ok(object::FileKind::Archive)
    ) {
        let archive = ArchiveFile::parse(artifact).unwrap();
        for member in archive.members() {
            let member = member.unwrap();
            append_object_debug_sections(member.data(artifact).unwrap(), &mut debug_info);
        }
    } else {
        append_object_debug_sections(artifact, &mut debug_info);
    }
    debug_info
}

fn read_artifact_with_remapped_debug_info(artifact: &Path, physical_root: &Path) -> Vec<u8> {
    let artifact_data = fs::read(artifact).unwrap();
    let debug_info = artifact_debug_info(&artifact_data);
    assert!(
        !debug_info.is_empty(),
        "{} contains no parseable debug information",
        artifact.display()
    );
    assert!(
        memchr::memmem::find(&debug_info, b"/workspace").is_some(),
        "{} debug information does not contain /workspace",
        artifact.display()
    );

    let physical_root = physical_root.to_string_lossy();
    assert!(
        memchr::memmem::find(&debug_info, physical_root.as_bytes()).is_none(),
        "{} debug information contains physical root {physical_root:?}",
        artifact.display()
    );
    artifact_data
}

fn run_compile(
    compiler: &Path,
    kind: CompilerKind,
    worktree: &Path,
    build_dir: &Path,
    configure: impl FnOnce(&mut std::process::Command),
) {
    let source = worktree.join(kind.source_name());
    let artifact = build_dir.join(kind.artifact_name());
    let mut command = sccache_command();
    command.current_dir(worktree).arg(compiler);
    match kind {
        CompilerKind::Rust => {
            command
                .arg(&source)
                .args([
                    "--crate-name",
                    "path_transform_e2e",
                    "--crate-type",
                    "lib",
                    "--emit",
                    "link",
                    "-Cdebuginfo=2",
                    "--out-dir",
                ])
                .arg(build_dir);
        }
        CompilerKind::Cxx => {
            command
                .args(["-c", "-g"])
                .arg(&source)
                .arg("-o")
                .arg(&artifact);
        }
    }
    configure(&mut command);
    assert_success(command.output().unwrap(), "real compiler invocation");
}

fn run_real_path_transform_test(compiler_name: &str, kind: CompilerKind) {
    let compiler = match which::which(compiler_name) {
        Ok(compiler) => compiler,
        Err(_) => {
            eprintln!("skipping path transform e2e test: {compiler_name} not found");
            return;
        }
    };

    let root = tempdir().unwrap();
    let root_path = root.path();
    let worktree_a = root_path.join("codex.foo");
    let worktree_b = root_path.join("codex.bar");
    let build_a = root_path.join("cargo-builds/workspace-hash-a");
    let build_b = root_path.join("cargo-builds/workspace-hash-b");
    for (worktree, build_dir) in [(&worktree_a, &build_a), (&worktree_b, &build_b)] {
        fs::create_dir_all(worktree).unwrap();
        fs::create_dir_all(build_dir).unwrap();
        fs::write(worktree.join(kind.source_name()), kind.source()).unwrap();
    }

    let normalized_root = root_path.to_string_lossy().replace('\\', "/");
    let transforms = vec![
        PathTransformConfig {
            from: format!(r"{}/codex\.[^/]+", regex::escape(&normalized_root)),
            to: "/workspace".to_owned(),
        },
        PathTransformConfig {
            from: format!(r"{}/cargo-builds/[^/]+", regex::escape(&normalized_root)),
            to: "/cargo-build".to_owned(),
        },
    ];

    let serverless_config = FileConfig {
        path_transforms: transforms.clone(),
        ..FileConfig::default()
    };
    write_json_cfg(root_path, "serverless-config.json", &serverless_config);
    let serverless_config = root_path.join("serverless-config.json");

    for (index, (worktree, build_dir)) in [(&worktree_a, &build_a), (&worktree_b, &build_b)]
        .into_iter()
        .enumerate()
    {
        let cache_dir = root_path.join(format!("cold-cache-{index}"));
        run_compile(&compiler, kind, worktree, build_dir, |command| {
            command
                .env("SCCACHE_CONF", &serverless_config)
                .env("SCCACHE_SERVERLESS", "true")
                .env("SCCACHE_DIRECTORY_DIR", &cache_dir)
                .env("SCCACHE_DIRECTORY_DIRECT", "false")
                .env(
                    "SCCACHE_SERVER_UDS",
                    root_path.join(format!("unused-{index}.sock")),
                );
        });
    }

    let artifact_a = build_a.join(kind.artifact_name());
    let artifact_b = build_b.join(kind.artifact_name());
    let artifact_a_data = read_artifact_with_remapped_debug_info(&artifact_a, root_path);
    let artifact_b_data = read_artifact_with_remapped_debug_info(&artifact_b, root_path);
    assert_eq!(
        artifact_a_data, artifact_b_data,
        "{compiler_name} artifacts differ across normalized directories"
    );

    let mut daemon_config = sccache_client_cfg(root_path, false);
    daemon_config.path_transforms = transforms;
    write_json_cfg(root_path, "daemon-config.json", &daemon_config);
    let daemon_config = root_path.join("daemon-config.json");
    let cached_config = root_path.join("cached-config");
    let socket = root_path.join("server.sock");

    let output = sccache_command()
        .arg("--start-server")
        .env("SCCACHE_CONF", &daemon_config)
        .env("SCCACHE_CACHED_CONF", &cached_config)
        .env("SCCACHE_SERVER_UDS", &socket)
        .output()
        .unwrap();
    assert_success(output, "starting isolated sccache daemon");
    let _guard = DaemonGuard {
        config: daemon_config.clone(),
        cached_config: cached_config.clone(),
        socket: socket.clone(),
    };

    fs::remove_file(&artifact_a).unwrap();
    fs::remove_file(&artifact_b).unwrap();
    for (worktree, build_dir) in [(&worktree_a, &build_a), (&worktree_b, &build_b)] {
        run_compile(&compiler, kind, worktree, build_dir, |command| {
            command
                .env("SCCACHE_CONF", &daemon_config)
                .env("SCCACHE_CACHED_CONF", &cached_config)
                .env("SCCACHE_SERVER_UDS", &socket);
        });
    }

    let output = sccache_command()
        .args(["--show-stats", "--stats-format=json"])
        .env("SCCACHE_CONF", &daemon_config)
        .env("SCCACHE_CACHED_CONF", &cached_config)
        .env("SCCACHE_SERVER_UDS", &socket)
        .output()
        .unwrap();
    assert_success(output.clone(), "reading isolated sccache stats");
    let info: ServerInfo = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        info.stats
            .cache_hits
            .get(kind.stats_language())
            .copied()
            .unwrap_or_default(),
        1,
        "second {compiler_name} compile was not a cache hit"
    );
    assert_eq!(
        info.stats
            .cache_misses
            .get(kind.stats_language())
            .copied()
            .unwrap_or_default(),
        1,
        "expected exactly one cold {compiler_name} compile"
    );
    let artifact_a_data = read_artifact_with_remapped_debug_info(&artifact_a, root_path);
    let artifact_b_data = read_artifact_with_remapped_debug_info(&artifact_b, root_path);
    assert_eq!(artifact_a_data, artifact_b_data);
}

#[test]
fn real_rustc_path_transforms() {
    run_real_path_transform_test("rustc", CompilerKind::Rust);
}

#[test]
fn real_gxx_path_transforms() {
    run_real_path_transform_test("g++", CompilerKind::Cxx);
}

#[test]
fn real_clangxx_path_transforms() {
    run_real_path_transform_test("clang++", CompilerKind::Cxx);
}
