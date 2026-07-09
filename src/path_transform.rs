// Copyright 2026 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::errors::*;
use crate::util::encode_path;
use directories::BaseDirs;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

/// Version marker for cache keys using path transformations.
const CACHE_KEY_TAG: &str = "sccache:path-transform-v1";

/// A configured path-prefix transformation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathTransformConfig {
    /// A regular expression matched against normalized absolute path ancestors.
    pub from: String,
    /// The stable prefix that replaces the matched ancestor.
    pub to: String,
}

#[derive(Clone, Debug)]
struct PathTransformRule {
    config: PathTransformConfig,
    regex: Regex,
}

#[derive(Clone, Debug, Default)]
pub struct PathTransforms {
    rules: Vec<PathTransformRule>,
    basedirs: Vec<PathBuf>,
}

impl PartialEq for PathTransforms {
    fn eq(&self, other: &Self) -> bool {
        self.configs() == other.configs() && self.basedirs == other.basedirs
    }
}

impl Eq for PathTransforms {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPathTransform {
    from: PathBuf,
    to: PathBuf,
    priority: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResolvedPathTransforms {
    mappings: Vec<ResolvedPathTransform>,
}

pub struct PathTransformResolver<'a> {
    config: &'a PathTransforms,
    cwd: PathBuf,
    resolved: ResolvedPathTransforms,
}

impl PathTransforms {
    pub fn new(configs: Vec<PathTransformConfig>, basedirs: &[Vec<u8>]) -> Result<Self> {
        let mut rules = Vec::with_capacity(configs.len());
        for mut config in configs {
            config.from = expand_home_regex(&config.from)?;
            if config.to.contains('=') {
                bail!("Path transform `to` may not contain '=': {:?}", config.to);
            }

            let regex = Regex::new(&format!("^(?:{})$", config.from))
                .with_context(|| format!("Invalid path transform regex {:?}", config.from))?;
            rules.push(PathTransformRule { config, regex });
        }

        let basedirs = basedirs
            .iter()
            .map(|basedir| {
                let basedir = String::from_utf8_lossy(basedir);
                PathBuf::from(basedir.trim_end_matches(['/', '\\']))
            })
            .collect();

        Ok(Self { rules, basedirs })
    }

    pub fn configs(&self) -> Vec<PathTransformConfig> {
        self.rules.iter().map(|rule| rule.config.clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.basedirs.is_empty()
    }

    pub fn resolve_invocation(
        &self,
        cwd: &Path,
        arguments: &[OsString],
        env_vars: &[(OsString, OsString)],
    ) -> ResolvedPathTransforms {
        let mut resolver = self.resolver(cwd);
        for argument in arguments {
            resolver.add_os_str(argument);
        }
        resolver.add_env(env_vars);
        resolver.finish()
    }

    pub fn resolver<'a>(&'a self, cwd: &Path) -> PathTransformResolver<'a> {
        let cwd = lexical_normalize(cwd);
        let mut resolver = PathTransformResolver {
            config: self,
            cwd: cwd.clone(),
            resolved: ResolvedPathTransforms::default(),
        };
        resolver.add_path(&cwd);
        resolver
    }

    fn resolve_path(&self, path: &Path) -> Option<ResolvedPathTransform> {
        let path = lexical_normalize(path);

        for (index, rule) in self.rules.iter().enumerate().rev() {
            for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
                let Some(candidate) = normalized_path_string(ancestor) else {
                    continue;
                };
                let Some(captures) = rule.regex.captures(&candidate) else {
                    continue;
                };
                let mut replacement = String::new();
                captures.expand(&rule.config.to, &mut replacement);
                return Some(ResolvedPathTransform {
                    from: ancestor.to_owned(),
                    to: PathBuf::from(replacement),
                    priority: self.basedirs.len() + index + 1,
                });
            }
        }

        self.basedirs
            .iter()
            .enumerate()
            .filter(|(_, basedir)| path.starts_with(basedir))
            .max_by_key(|(_, basedir)| basedir.components().count())
            .map(|(index, basedir)| ResolvedPathTransform {
                from: basedir.clone(),
                to: PathBuf::from("."),
                priority: index,
            })
    }
}

impl PathTransformResolver<'_> {
    pub fn add_path(&mut self, path: &Path) {
        let path = if path.is_absolute() {
            lexical_normalize(path)
        } else {
            lexical_normalize(&self.cwd.join(path))
        };
        if let Some(mapping) = self.config.resolve_path(&path) {
            self.resolved.insert(mapping);
        }
    }

    pub fn add_os_str(&mut self, value: &OsStr) {
        let value = value.to_string_lossy();

        for candidate in path_candidates(&value) {
            self.add_path(Path::new(candidate));
        }
    }

    pub fn add_env(&mut self, env_vars: &[(OsString, OsString)]) {
        for (_, value) in env_vars {
            self.add_os_str(value);
        }
    }

    pub fn finish(self) -> ResolvedPathTransforms {
        self.resolved
    }
}

impl ResolvedPathTransforms {
    fn insert(&mut self, mapping: ResolvedPathTransform) {
        if let Some(existing) = self
            .mappings
            .iter_mut()
            .find(|existing| existing.from == mapping.from)
        {
            if mapping.priority >= existing.priority {
                *existing = mapping;
            }
            return;
        }
        self.mappings.push(mapping);
        self.mappings.sort_by(|a, b| {
            a.priority.cmp(&b.priority).then(
                a.from
                    .components()
                    .count()
                    .cmp(&b.from.components().count()),
            )
        });
    }

    pub fn is_empty(&self) -> bool {
        self.mappings.is_empty()
    }

    pub fn mappings(&self) -> impl Iterator<Item = (&Path, &Path)> {
        self.mappings
            .iter()
            .map(|mapping| (mapping.from.as_path(), mapping.to.as_path()))
    }

    pub fn transform_path(&self, path: &Path) -> PathBuf {
        let path = lexical_normalize(path);
        let Some(mapping) = self.best_mapping(&path) else {
            return path;
        };
        let suffix = path
            .strip_prefix(&mapping.from)
            .expect("selected mapping must be a prefix");
        mapping.to.join(suffix)
    }

    pub fn transform_os_str(&self, value: &OsStr) -> OsString {
        let bytes = os_str_bytes(value);
        let transformed = self.transform_bytes(&bytes);
        if matches!(transformed, Cow::Borrowed(_)) {
            return value.to_owned();
        }
        bytes_to_os_string(transformed.into_owned())
    }

    pub fn transform_bytes<'a>(&self, value: &'a [u8]) -> Cow<'a, [u8]> {
        let replacements = self
            .mappings
            .iter()
            .filter_map(|mapping| {
                let from = path_bytes(&mapping.from);
                if from.is_empty() {
                    return None;
                }
                Some((from, path_bytes(&mapping.to), mapping.priority))
            })
            .collect::<Vec<_>>();

        replace_bytes(value, &replacements)
    }

    pub fn cache_key_args(&self) -> Vec<OsString> {
        if self.is_empty() {
            return Vec::new();
        }

        let mut destinations = self
            .mappings
            .iter()
            .map(|mapping| mapping.to.as_os_str().to_owned())
            .collect::<Vec<_>>();
        destinations.sort();
        destinations.dedup();

        let mut result = Vec::with_capacity(destinations.len() + 1);
        result.push(OsString::from(CACHE_KEY_TAG));
        result.extend(destinations);
        result
    }

    pub fn rustc_args(&self) -> Vec<OsString> {
        self.mappings
            .iter()
            .flat_map(|mapping| {
                let mut value = mapping.from.as_os_str().to_owned();
                value.push("=");
                value.push(mapping.to.as_os_str());
                [OsString::from("--remap-path-prefix"), value]
            })
            .collect()
    }

    pub fn file_prefix_map_args(&self) -> Vec<OsString> {
        self.mappings
            .iter()
            .map(|mapping| {
                let mut value = OsString::from("-ffile-prefix-map=");
                value.push(mapping.from.as_os_str());
                value.push("=");
                value.push(mapping.to.as_os_str());
                value
            })
            .collect()
    }

    fn best_mapping(&self, path: &Path) -> Option<&ResolvedPathTransform> {
        self.mappings
            .iter()
            .filter(|mapping| path.starts_with(&mapping.from))
            .max_by_key(|mapping| (mapping.priority, mapping.from.components().count()))
    }
}

fn expand_home_regex(pattern: &str) -> Result<String> {
    let suffix = if pattern == "~" {
        Some("")
    } else {
        pattern.strip_prefix("~/")
    };

    if let Some(suffix) = suffix {
        let base_dirs = BaseDirs::new().context("Unable to determine home directory")?;
        let home = normalized_path_string(base_dirs.home_dir())
            .context("Home directory is not valid UTF-8")?;
        let home = regex::escape(&home);
        return if suffix.is_empty() {
            Ok(home)
        } else {
            Ok(format!("{home}/{suffix}"))
        };
    }

    if pattern.starts_with('~') {
        bail!(
            "Path transform `from` beginning with '~' must be exactly '~' or start with '~/': {pattern:?}"
        );
    }

    let bytes = pattern.as_bytes();
    let unix_absolute = pattern.starts_with('/');
    let windows_absolute =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/';

    if !unix_absolute && !windows_absolute {
        bail!(
            "Path transform `from` must be an absolute path regex or start with '~/': {pattern:?}"
        );
    }

    Ok(pattern.to_owned())
}

fn normalized_path_string(path: &Path) -> Option<String> {
    let mut value = path.to_str()?.replace('\\', "/");
    while value.contains("//") {
        value = value.replace("//", "/");
    }
    while value.ends_with('/') && value.len() > 1 {
        value.pop();
    }
    #[cfg(windows)]
    value.make_ascii_lowercase();
    Some(value)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component);
                }
            }
            _ => normalized.push(component),
        }
    }
    normalized
}

fn path_candidates(value: &str) -> Vec<&str> {
    let mut candidates = Vec::new();
    let bytes = value.as_bytes();

    for (index, byte) in bytes.iter().enumerate() {
        let unix_absolute = *byte == b'/';
        let windows_absolute = index + 2 < bytes.len()
            && byte.is_ascii_alphabetic()
            && bytes[index + 1] == b':'
            && matches!(bytes[index + 2], b'/' | b'\\');

        if !unix_absolute && !windows_absolute {
            continue;
        }

        if index > 0
            && !matches!(
                bytes[index - 1],
                b'=' | b',' | b';' | b':' | b'"' | b'\'' | b' ' | b'\t'
            )
            && !value[..index].ends_with("-I")
            && !value[..index].ends_with("-L")
        {
            continue;
        }

        let end = bytes[index..]
            .iter()
            .position(|byte| matches!(byte, b',' | b';' | b'"' | b'\'' | b' ' | b'\t'))
            .map_or(bytes.len(), |offset| index + offset);
        candidates.push(&value[index..end]);
    }

    candidates
}

fn path_bytes(path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    encode_path(&mut bytes, path).expect("encoding a path into memory cannot fail");
    bytes
}

#[cfg(unix)]
fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn bytes_to_os_string(value: Vec<u8>) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(value)
}

#[cfg(windows)]
fn bytes_to_os_string(value: Vec<u8>) -> OsString {
    String::from_utf8_lossy(&value).into_owned().into()
}

fn replace_bytes<'a>(value: &'a [u8], replacements: &[(Vec<u8>, Vec<u8>, usize)]) -> Cow<'a, [u8]> {
    let mut matches = Vec::new();

    for (from, to, priority) in replacements {
        for position in memchr::memmem::find_iter(value, from) {
            matches.push((position, from.len(), to.as_slice(), *priority));
        }
    }

    if matches.is_empty() {
        return Cow::Borrowed(value);
    }

    matches.sort_by(|a, b| a.0.cmp(&b.0).then(b.3.cmp(&a.3)).then(b.1.cmp(&a.1)));

    let mut output = Vec::with_capacity(value.len());
    let mut cursor = 0;
    for (position, length, replacement, _) in matches {
        if position < cursor {
            continue;
        }
        output.extend_from_slice(&value[cursor..position]);
        output.extend_from_slice(replacement);
        cursor = position + length;
    }
    output.extend_from_slice(&value[cursor..]);
    Cow::Owned(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transforms() -> PathTransforms {
        PathTransforms::new(
            vec![
                PathTransformConfig {
                    from: r"/home/[^/]+/codex\.[^/]+".into(),
                    to: "/workspace".into(),
                },
                PathTransformConfig {
                    from: "/cargo/builds/[^/]+".into(),
                    to: "/cargo-build".into(),
                },
            ],
            &[],
        )
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn expands_current_home_patterns() {
        let home = BaseDirs::new().unwrap().home_dir().to_owned();
        let cwd = home.join("codex.foo/crate");
        let transforms = PathTransforms::new(
            vec![PathTransformConfig {
                from: r"~/codex\.[^/]+".into(),
                to: "/workspace".into(),
            }],
            &[],
        )
        .unwrap();
        let resolved = transforms.resolver(&cwd).finish();

        assert_eq!(
            resolved.transform_path(&cwd.join("src/lib.rs")),
            Path::new("/workspace/crate/src/lib.rs")
        );

        let home_transform = PathTransforms::new(
            vec![PathTransformConfig {
                from: "~".into(),
                to: "/home".into(),
            }],
            &[],
        )
        .unwrap();
        let home_resolved = home_transform.resolver(&cwd).finish();
        assert_eq!(
            home_resolved.transform_path(&cwd.join("src/lib.rs")),
            Path::new("/home/codex.foo/crate/src/lib.rs")
        );
    }

    #[test]
    fn resolves_worktrees_to_the_same_path() {
        let transforms = transforms();

        let mut foo = transforms.resolver(Path::new("/home/me/codex.foo/crate"));
        foo.add_path(Path::new("/cargo/builds/abc/debug/deps"));
        let foo = foo.finish();

        let mut bar = transforms.resolver(Path::new("/home/me/codex.bar/crate"));
        bar.add_path(Path::new("/cargo/builds/xyz/debug/deps"));
        let bar = bar.finish();

        assert_eq!(
            foo.transform_path(Path::new("/home/me/codex.foo/crate/src/lib.rs")),
            Path::new("/workspace/crate/src/lib.rs")
        );
        assert_eq!(
            bar.transform_path(Path::new("/home/me/codex.bar/crate/src/lib.rs")),
            Path::new("/workspace/crate/src/lib.rs")
        );
        assert_eq!(
            foo.transform_path(Path::new("/cargo/builds/abc/debug/deps")),
            Path::new("/cargo-build/debug/deps")
        );
        assert_eq!(
            bar.transform_path(Path::new("/cargo/builds/xyz/debug/deps")),
            Path::new("/cargo-build/debug/deps")
        );
        assert_eq!(foo.cache_key_args(), bar.cache_key_args());
        assert!(
            foo.cache_key_args()
                .iter()
                .all(|value| !value.to_string_lossy().contains("/home/me"))
        );
    }

    #[test]
    fn last_matching_rule_wins() {
        let transforms = PathTransforms::new(
            vec![
                PathTransformConfig {
                    from: "/home/.*".into(),
                    to: "/generic".into(),
                },
                PathTransformConfig {
                    from: r"/home/[^/]+/codex\.[^/]+".into(),
                    to: "/workspace".into(),
                },
            ],
            &[],
        )
        .unwrap();
        let resolved = transforms
            .resolver(Path::new("/home/me/codex.foo/crate"))
            .finish();
        assert_eq!(
            resolved.transform_path(Path::new("/home/me/codex.foo/crate")),
            Path::new("/workspace/crate")
        );
    }

    #[test]
    fn transforms_embedded_paths_without_hashing_sources() {
        let resolved = transforms()
            .resolver(Path::new("/home/me/codex.foo/crate"))
            .finish();
        assert_eq!(
            resolved.transform_os_str(OsStr::new(
                "--cfg=path=\"/home/me/codex.foo/crate/src/lib.rs\""
            )),
            OsStr::new("--cfg=path=\"/workspace/crate/src/lib.rs\"")
        );
        assert_eq!(
            &*resolved.transform_bytes(b"# 1 \"/home/me/codex.foo/crate/src/lib.rs\""),
            b"# 1 \"/workspace/crate/src/lib.rs\""
        );
    }

    #[test]
    fn basedirs_are_upgraded_to_stable_relative_roots() {
        let transforms = PathTransforms::new(vec![], &[b"/home/me/project/".to_vec()]).unwrap();
        let resolved = transforms
            .resolver(Path::new("/home/me/project/crate"))
            .finish();
        assert_eq!(
            resolved.transform_path(Path::new("/home/me/project/crate/src/lib.rs")),
            Path::new("./crate/src/lib.rs")
        );
    }

    #[test]
    fn expands_capture_groups_and_rejects_invalid_regexes() {
        let transforms = PathTransforms::new(
            vec![PathTransformConfig {
                from: "/home/([^/]+)/codex\\.[^/]+".into(),
                to: "/workspace/$1".into(),
            }],
            &[],
        )
        .unwrap();
        let resolved = transforms
            .resolver(Path::new("/home/me/codex.foo/crate"))
            .finish();
        assert_eq!(
            resolved.transform_path(Path::new("/home/me/codex.foo/crate/src/lib.rs")),
            Path::new("/workspace/me/crate/src/lib.rs")
        );

        assert!(
            PathTransforms::new(
                vec![PathTransformConfig {
                    from: "C:/worktrees/.*".into(),
                    to: "/workspace".into(),
                }],
                &[],
            )
            .is_ok()
        );

        for from in ["~user/.*", r"~\relative\.*", "relative/.*"] {
            assert!(
                PathTransforms::new(
                    vec![PathTransformConfig {
                        from: from.into(),
                        to: "/workspace".into(),
                    }],
                    &[],
                )
                .is_err(),
                "{from:?} should be rejected"
            );
        }

        assert!(
            PathTransforms::new(
                vec![PathTransformConfig {
                    from: "/home/[abc".into(),
                    to: "/workspace".into(),
                }],
                &[],
            )
            .is_err()
        );
        assert!(
            PathTransforms::new(
                vec![PathTransformConfig {
                    from: "/home/.*".into(),
                    to: "/workspace=bad".into(),
                }],
                &[],
            )
            .is_err()
        );
    }
}
