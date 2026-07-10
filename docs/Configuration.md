# Available Configuration Options

## file

```toml
# If specified, wait this long for the server to start up.
server_startup_timeout_ms = 10000

# Use `path_transforms` when the same source tree is built in different absolute
# directories but should share cache entries. This happens when CI gives each
# job a unique workspace, the same repository is checked out in several Git
# worktrees, developers with different home directories share a cache, or a
# build tool puts outputs under generated or hash-named directories.
#
# sccache cannot assume arbitrary directories are equivalent because absolute
# paths occur in compiler arguments, preprocessor output, and generated debug
# information, where they can affect compiler output. It therefore hashes the
# real paths by default. A path transform explicitly declares which varying
# roots represent the same logical location, enabling cache hits across those
# directories and making embedded paths reproducible.
#
# `path_transforms` replaces selected machine-specific absolute path prefixes
# with stable paths. Each `[[path_transforms]]` table defines:
#
# - `from`: a regular expression that selects an absolute path prefix.
# - `to`: the stable path that replaces the matched prefix.
#
# This rule maps checkout directories such as `~/codex.foo` and `~/codex.bar`
# to the same stable root:
[[path_transforms]]
from = '~/codex\.[^/]+'
to = '/workspace'

# For example, both `~/codex.foo/crate/src/lib.rs` and
# `~/codex.bar/crate/src/lib.rs` normalize to
# `/workspace/crate/src/lib.rs`.
#
# Separate rules can normalize other unstable roots. Cargo can place build
# output outside the worktree when using, for example,
# build-dir = "{cargo-cache-home}/builds/cargo/{workspace-path-hash}".
# This rule removes that generated workspace hash:
[[path_transforms]]
from = '~/.cargo/builds/cargo/[^/]+'
to = '/cargo-build'

# sccache normalizes each absolute path and tests `from` against its ancestors.
# A matched ancestor is replaced with `to`; the rest of the path is preserved.
# Every test is a whole-ancestor match: sccache compiles the pattern as
# `^(?:<from>)$`. Explicit `^` and `$` anchors are allowed but redundant. A
# literal rule such as `from = '/home/user/project'` therefore handles every
# path below that directory without a trailing `.*`.
#
# `from` uses the Rust `regex` crate syntax, not PCRE:
# https://docs.rs/regex/latest/regex/#syntax
# It supports character classes, repetition, alternation, and capture groups,
# but not look-around or backreferences. TOML literal strings (`'...'`) preserve
# regex backslashes.
#
# `to` uses Rust regex replacement syntax, so it can refer to numbered captures
# with `$1` and named captures with `${name}`:
#
# [[path_transforms]]
# from = '/home/(?P<user>[^/]+)/codex\.[^/]+'
# to = '/workspace/${user}'
#
# Patterns use `/` as the path separator. On Windows, candidate paths are
# lowercased, so literal path text in `from` should also be lowercase.
# `from` must begin with `/`, a Windows drive root such as `c:/`, or use `~`
# exactly or `~/...` for the current user's home directory. Named-user forms
# such as `~alice/` and relative patterns are rejected. `to` may not contain
# `=`.
#
# Rules are ordered, and the last matching rule wins. Path transforms apply in
# daemon, client-side, and serverless modes. Rust receives
# `--remap-path-prefix`; GCC and Clang C/C++ receive `-ffile-prefix-map`. If a
# configured transform matches an invocation for another compiler kind,
# sccache fails the request rather than compiling with unnormalized debug paths.
#
# Only the stable `to` destinations, not the concrete matched source prefixes
# or `from` patterns, are included in cache keys. Adding another checkout that
# an existing rule already covers does not invalidate existing cache entries.
#
# `basedirs` is the older shorthand for one subset of this behavior. Each entry
# is a literal absolute directory that maps to `.`, with the longest matching
# directory winning. It cannot use regexes, captures, or another destination.
# For example:
#
# basedirs = ["/home/user/project"]
#
# is equivalent to this rule:
#
# [[path_transforms]]
# from = '/home/user/project'
# to = '.'
#
# To reproduce multiple overlapping `basedirs`, order the equivalent transforms
# from least specific to most specific because the last matching transform
# wins. If both forms match, `path_transforms` takes precedence. `basedirs` and
# `SCCACHE_BASEDIRS` remain supported for existing configurations.

[compile]
# Run the compile and cache pipeline in this process without a local daemon.
# Requires the directory cache or one remote backend; distributed and
# multi-level caching are not supported.
serverless = false
# Run the compile pipeline here while proxying cache I/O through the daemon.
client_side_mode = false

[dist]
# where to find the scheduler
scheduler_url = "http://1.2.3.4:10600"
# a set of prepackaged toolchains
toolchains = []
# the maximum size of the toolchain cache in bytes
toolchain_cache_size = 5368709120
cache_dir = "/home/user/.cache/sccache-dist-client"

[dist.auth]
type = "token"
token = "secrettoken"

# Multi-level cache configuration
# Define cache levels in order (fast to slow).
# The chain uses backend names only. Remote backends must be configured below.
# Configure the local `directory` backend with [cache.directory] or SCCACHE_DIRECTORY_*.
# See docs/MultiLevel.md for details.
[cache.multilevel]
chain = ["disk", "redis", "s3"]
write_error_policy = "l0"  # Optional: ignore, l0 (default), or all

#[cache.azure]
# Azure Storage connection string (see <https://docs.azure.cn/en-us/storage/common/storage-configure-connection-string>)
connection_string = "BlobEndpoint=https://example.blob.core.windows.net/;SharedAccessSignature=..."
# Name of container
container = "my_container_name"
# Optional string to prepend to each blob storage key
key_prefix = ""

[cache.disk]
dir = "/tmp/.cache/sccache"
size = 7516192768 # 7 GiBytes

# Directory-backed local cache. Stores each cache entry as raw files under the
# reserved `directory` child, enabling reflink/copy restore on cache hits.
[cache.directory]
dir = "/tmp/.cache/sccache"
size = 7516192768 # 7 GiBytes
rw_mode = "READ_WRITE"
[cache.directory.link]
type = "reflink" # Also: "hard_link" or "symlink".
required = false # Fall back to copying when the selected link operation fails.

# See the local docs on more explanations about this mode
[cache.disk.preprocessor_cache_mode]
# Whether to use the preprocessor cache mode
use_preprocessor_cache_mode = true
# Whether to use file times to check for changes
file_stat_matches = true
# Whether to also use ctime (file status change) time to check for changes
use_ctime_for_stat = true
# Whether to ignore `__TIME__` when caching
ignore_time_macros = false
# Whether to skip (meaning not cache, only hash) system headers
skip_system_headers = false
# Whether hash the current working directory
hash_working_directory = true

[cache.gcs]
# optional oauth url
oauth_url = "..."
# optional deprecated url
deprecated_url = "..."
rw_mode = "READ_ONLY"
# rw_mode = "READ_WRITE"
cred_path = "/psst/secret/cred"
bucket = "bucket"
key_prefix = "prefix"

[cache.gha]
url = "http://localhost"
token = "secret"
cache_to = "sccache-latest"
cache_from = "sccache-"

[cache.memcached]
# Deprecated alias for `endpoint`
# url = "127.0.0.1:11211"
endpoint = "tcp://127.0.0.1:11211"
# Username and password for authentication
username = "user"
password = "passwd"
# Entry expiration time in seconds. Default is 86400 (24 hours)
expiration = 3600
key_prefix = "/custom/prefix/if/need"

[cache.redis]
# Deprecated, use `endpoint` instead
url = "redis://user:passwd@1.2.3.4:6379/?db=1"
## Refer to the `opendal` documentation for more information about Redis endpoint
# Single-node endpoint. Mutually exclusive with `cluster_endpoints`
endpoint = "redis://127.0.0.1:6379"
# Multiple-node list of endpoints (cluster mode). Mutually exclusive with `endpoint`
cluster_endpoints = "redis://10.0.0.1:6379,redis://10.0.0.2:6379"
username = "user"
password = "passwd"
# Database number to use. Default is 0
db = 1
# Entry expiration time in seconds. Default is 0 (never expire)
expiration = 3600
key_prefix = "/custom/prefix/if/need"

[cache.s3]
bucket = "name"
endpoint = "s3-us-east-1.amazonaws.com"
use_ssl = true
key_prefix = "s3prefix"
server_side_encryption = false

[cache.webdav]
endpoint = "http://192.168.10.42:80/some/webdav.php"
key_prefix = "/custom/webdav/subfolder/if/need"
# Basic HTTP authentication credentials.
username = "alice"
password = "secret12"
# Mutually exclusive with username & password. Bearer token value
token = "token123"

[cache.oss]
bucket = "name"
endpoint = "oss-us-east-1.aliyuncs.com"
key_prefix = "ossprefix"
no_credentials = true

[cache.cos]
bucket = "name"
endpoint = "cos.na-siliconvalley.myqcloud.com"
key_prefix = "cosprefix"
```

sccache looks for its configuration file at the path indicated by env variable `SCCACHE_CONF`.

If no such env variable is set, sccache looks at default locations as below:
- Linux: `~/.config/sccache/config`
- macOS: `~/Library/Application Support/Mozilla.sccache/config`
- Windows: `%APPDATA%\Mozilla\sccache\config\config`

The latest `cache.XXX` entries may be found here: https://github.com/mozilla/sccache/blob/ffe3070f77ef3301c8ff718316e4ab017ec83042/src/config.rs#L300.

## env

Whatever is set by a file based configuration, it is overruled by the env
configuration variables

Note that some env variables may need sccache server restart to take effect.

### misc

* `SCCACHE_ALLOW_CORE_DUMPS` to enable core dumps by the server
* `SCCACHE_CONF` configuration file path
* `SCCACHE_BASEDIRS` supplies the legacy `basedirs` setting from the environment. Each entry must be an absolute directory and maps that directory to `.`. Separate entries with `;` on Windows and `:` on other operating systems; the longest matching directory wins. Matching is **case-insensitive** on Windows and **case-sensitive** elsewhere. This variable overrides `basedirs` in the configuration file. Use file-configured `path_transforms` when regex matching, capture replacement, or a destination other than `.` is required.
* `SCCACHE_CACHED_CONF`
* `SCCACHE_IDLE_TIMEOUT` how long the local daemon process waits for more client requests before exiting, in seconds. Set to `0` to run sccache permanently
* `SCCACHE_STARTUP_NOTIFY` specify a path to a socket which will be used for server completion notification
* `SCCACHE_MAX_FRAME_LENGTH` how much data can be transferred between client and server
* `SCCACHE_NO_DAEMON` set to `1` to disable putting the server to the background
* `SCCACHE_SERVERLESS` runs compilations and cache access in the invoking process without starting or contacting the local daemon. It requires the `directory` backend or one remote backend, and is incompatible with distributed and multi-level caching. With a shared `directory` cache, compile work and private entry construction remain concurrent while cache publication is coordinated between processes.
* `SCCACHE_CLIENT_SIDE` runs compilations in the invoking process while proxying cache I/O and statistics through the local daemon. `SCCACHE_SERVERLESS` takes precedence when both are enabled.
* `SCCACHE_CACHE_MULTIARCH` to disable caching of multi architecture builds.
* `SCCACHE_CACHE_ZSTD_LEVEL` to set zstd compression level of cache. the range is `1-22` and default is `3`.
  - For example, in `10`, it have about 0.9x size with about 1.6x time than default `3` (tested with compiling sccache code)
  - This option will only applied to newly compressed cache and don't affect existing cache.
  - If you want to be apply to all cache, you should reset cache and make new cache.
* `SCCACHE_LOG_MILLIS` when set (to any value), enables millisecond precision timestamps in log output instead of the default second precision.
* `SCCACHE_ERROR_LOG` path to a file where sccache will log errors
* `SCCACHE_LOG` log level, accepting standard env_logger values, see [env_logger documentation](https://docs.rs/env_logger/latest/env_logger/#enabling-logging) for details

### cache configs

#### multi-level cache

Multi-level caching enables hierarchical cache storage with automatic backfill. See the [Multi-Level Cache documentation](MultiLevel.md) for detailed information.

Multi-level caching is not supported in serverless mode because backfill and secondary writes may outlive the invoking process.

* `SCCACHE_MULTILEVEL_CHAIN` comma-separated list of cache backend names to use in hierarchy (e.g., `disk,redis,s3`)
  - Order matters: left-to-right is fast-to-slow (L0, L1, L2, ...)
  - Valid names: `disk`, `directory`, `redis`, `memcached`, `s3`, `gcs`, `azure`, `gha`, `webdav`, `oss`, `cos`
  - These are backend names, not config section names. Configure `disk` with `[cache.disk]`/`SCCACHE_DIR`; configure `directory` with `[cache.directory]`/`SCCACHE_DIRECTORY_*`. The directory backend stores data in the reserved `directory` child of its configured cache root.
  - If not set, sccache uses single-level mode (legacy behavior)
* `SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY` controls error handling on cache writes (default: `l0`)
  - `ignore` - never fail on write errors, log warnings only (most permissive)
  - `l0` - fail only if L0 (first level) write fails (default, balances reliability and performance)
  - `all` - fail if any read-write level fails (most strict)
  - Read-only levels are always skipped and never cause failures

**Basic example**:
```bash
export SCCACHE_MULTILEVEL_CHAIN="disk,redis,s3"
export SCCACHE_DIR="/tmp/cache"              # for disk level
export SCCACHE_REDIS_ENDPOINT="redis://..."  # for redis level
export SCCACHE_BUCKET="my-bucket"            # for s3 level
```

**Write policy examples**:
```bash
# Default: Fail only if disk write fails
export SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY="l0"

# Best effort: Never fail on cache writes
export SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY="ignore"

# Strict: Fail if any level write fails
export SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY="all"
```

#### disk (local)

* `SCCACHE_DIR` local on disk artifact cache directory
* `SCCACHE_CACHE_SIZE` maximum size of the local on disk cache i.e. `2G` - default is 10G
* `SCCACHE_DIRECT` enable/disable preprocessor caching (see [the local doc](Local.md))
* `SCCACHE_LOCAL_RW_MODE` the mode that the cache will operate in (`READ_ONLY` or `READ_WRITE`)

#### directory (local, reflinkable)

* `SCCACHE_DIRECTORY_DIR` sets the cache root for the `directory` backend; data is stored in its reserved `directory` child. The root defaults to `SCCACHE_DIR`.
* `SCCACHE_DIRECTORY_CACHE_SIZE` maximum size of the directory-backed cache i.e. `2G` - default is 10G
* `SCCACHE_DIRECTORY_RW_MODE` local directory cache mode, either `READ_ONLY` or `READ_WRITE`
* `SCCACHE_DIRECTORY_DIRECT` controls preprocessor cache mode for the directory-backed cache
* `SCCACHE_DIRECTORY_LINK_TYPE` selects cache-hit restoration using `reflink` (default), `hard_link`, or `symlink`. Hard-linked outputs share the cache object's inode and must be treated as immutable. Symlinked outputs may target a cache on another filesystem, but become dangling if their cache entries are evicted. On a hit, sccache first tries to update one data object's `atime`; only if that fails does it update manifest `mtime`. Eviction uses the newer of manifest `mtime` and object `atime`.
* `SCCACHE_DIRECTORY_LINK_REQUIRED` fails cache-hit restoration when the selected link operation fails instead of falling back to a full copy. It is disabled by default.

#### s3 compatible

* `SCCACHE_BUCKET` s3 bucket to be used
* `SCCACHE_ENDPOINT` s3 endpoint
* `SCCACHE_REGION` s3 region, required if using AWS S3
* `SCCACHE_S3_USE_SSL` s3 endpoint requires TLS, set this to `true`
* `SCCACHE_S3_KEY_PREFIX` s3 key prefix (optional)
* `SCCACHE_S3_RW_MODE` allows to use s3 backend in read-only mode if set to `READ_ONLY`

The endpoint used then becomes `${SCCACHE_BUCKET}.s3-{SCCACHE_REGION}.amazonaws.com`.
If you are not using the default endpoint and `SCCACHE_REGION` is undefined, it
will default to `us-east-1`.

#### cloudflare r2

* `SCCACHE_BUCKET` is the name of your R2 bucket.
* `SCCACHE_ENDPOINT` must follow the format of `https://<ACCOUNT_ID>.r2.cloudflarestorage.com`. Note that the `https://` must be included. Your account ID can be found [here](https://developers.cloudflare.com/fundamentals/get-started/basic-tasks/find-account-and-zone-ids/).
* `SCCACHE_REGION` should be set to `auto`.
* `SCCACHE_S3_KEY_PREFIX` s3 key prefix (optional).

#### redis

* `SCCACHE_REDIS` full redis url, including auth and access token/passwd (deprecated).
* `SCCACHE_REDIS_ENDPOINT` redis url without auth and access token/passwd - single node configuration.
* `SCCACHE_REDIS_CLUSTER_ENDPOINTS` redis cluster urls, separated by comma - shared cluster configuration.
* `SCCACHE_REDIS_USERNAME` redis username (optional).
* `SCCACHE_REDIS_PASSWORD` redis password (optional).
* `SCCACHE_REDIS_DB` redis database (optional, default is 0).
* `SCCACHE_REDIS_EXPIRATION` / `SCCACHE_REDIS_TTL` ttl for redis cache, don't set for default behavior.
* `SCCACHE_REDIS_KEY_PREFIX` key prefix (optional).
* `SCCACHE_REDIS_RW_MODE` allows to use redis backend in read-only mode if set to `READ_ONLY`

The full url appears then as `redis://user:passwd@1.2.3.4:6379/?db=1`.

#### memcached

* `SCCACHE_MEMCACHED` is a deprecated alias for `SCCACHE_MEMCACHED_ENDPOINT`.
* `SCCACHE_MEMCACHED_ENDPOINT` memcached url.
* `SCCACHE_MEMCACHED_USERNAME` memcached username (optional).
* `SCCACHE_MEMCACHED_PASSWORD` memcached password (optional).
* `SCCACHE_MEMCACHED_EXPIRATION` ttl for memcached cache, don't set for default behavior.
* `SCCACHE_MEMCACHED_KEY_PREFIX` key prefix (optional).
* `SCCACHE_MEMCACHED_RW_MODE` allows to use memcached backend in read-only mode if set to `READ_ONLY`

#### gcs

* `SCCACHE_GCS_BUCKET`
* `SCCACHE_GCS_CREDENTIALS_URL`
* `SCCACHE_GCS_KEY_PATH`
* `SCCACHE_GCS_RW_MODE`

#### azure

* `SCCACHE_AZURE_CONNECTION_STRING`
* `SCCACHE_AZURE_BLOB_CONTAINER`
* `SCCACHE_AZURE_KEY_PREFIX`
* `SCCACHE_AZURE_RW_MODE`

#### gha

* `SCCACHE_GHA_CACHE_URL` / `ACTIONS_RESULTS_URL` GitHub Actions cache API URL
* `SCCACHE_GHA_RUNTIME_TOKEN` / `ACTIONS_RUNTIME_TOKEN` GitHub Actions access token
* `SCCACHE_GHA_CACHE_TO` cache key to write
* `SCCACHE_GHA_CACHE_FROM` comma separated list of cache keys to read from
* `SCCACHE_GHA_RW_MODE` allows to use GHA cache backend in read-only mode if set to `READ_ONLY`

#### webdav

* `SCCACHE_WEBDAV_ENDPOINT` a webdav service endpoint to store cache, such as `http://127.0.0.1:8080/my/webdav.php`.
* `SCCACHE_WEBDAV_KEY_PREFIX` specify the key prefix (subfolder) of cache (optional).
* `SCCACHE_WEBDAV_USERNAME` a username to authenticate with webdav service (optional).
* `SCCACHE_WEBDAV_PASSWORD` a password to authenticate with webdav service (optional).
* `SCCACHE_WEBDAV_TOKEN` a token to authenticate with webdav service (optional) - may be used instead of login & password.
* `SCCACHE_WEBDAV_RW_MODE` allows to use webdav backend in read-only mode if set to `READ_ONLY`

#### OSS

* `SCCACHE_OSS_BUCKET`
* `SCCACHE_OSS_ENDPOINT`
* `SCCACHE_OSS_KEY_PREFIX`
* `ALIBABA_CLOUD_ACCESS_KEY_ID`
* `ALIBABA_CLOUD_ACCESS_KEY_SECRET`
* `SCCACHE_OSS_NO_CREDENTIALS`
* `SCCACHE_OSS_RW_MODE`

#### Tencent Cloud Object Storage (COS)

* `SCCACHE_COS_BUCKET`
* `SCCACHE_COS_ENDPOINT`
* `SCCACHE_COS_KEY_PREFIX`
* `TENCENTCLOUD_SECRET_ID`
* `TENCENTCLOUD_SECRET_KEY`
* `SCCACHE_COS_RW_MODE`
