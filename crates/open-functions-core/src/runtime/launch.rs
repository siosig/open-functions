//! Launch descriptor (002-python-runtime): how to start one function
//! instance, expressed as one of the two shapes the existing drivers already
//! understand -- a plain child process ([`ProcessDriver`](super::process::ProcessDriver))
//! or a Docker container ([`ContainerDriver`](super::container::ContainerDriver)).
//!
//! Before this module, [`super::InstanceSpec`] carried `artifact_path` (a
//! bare executable path) and `image_ref` (an optional image reference)
//! directly, which was enough for Rust source-mode (`Command::new(artifact)`)
//! and image-mode (`docker run <image>`) but has no way to express "run this
//! *interpreter* against *these arguments* from *this working directory*" --
//! exactly what a Python instance needs (`functions-framework`, with `cwd`
//! set to the source snapshot so it can `import main`). [`Launch`]
//! generalizes both existing cases and the new Python ones into the same two
//! variants the drivers were already built around, per research.md R8.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::model::function::Function;

/// How to start one function instance. `Process` is handled by
/// [`ProcessDriver`](super::process::ProcessDriver), `Container` by
/// [`ContainerDriver`](super::container::ContainerDriver) -- see each
/// variant's constructor below for which registration/runtime combination
/// produces it.
#[derive(Debug, Clone)]
pub enum Launch {
    Process {
        /// Absolute path to the executable to run.
        program: PathBuf,
        /// Extra argv entries after `program` (empty for Rust source-mode
        /// and Python host-mode, which both take their configuration
        /// entirely through environment variables).
        args: Vec<String>,
        /// Working directory to run `program` from. `None` means "inherit
        /// the host process's own cwd" (Rust source-mode's existing
        /// behavior); Python host-mode sets this to the version's source
        /// snapshot directory so `functions-framework` can `import main`.
        cwd: Option<PathBuf>,
    },
    Container {
        /// Image reference to run.
        image: String,
        /// Bind mounts in bollard's `"host:container[:ro]"` string form.
        /// Empty for image-mode (the image is self-contained); non-empty
        /// for Python container-mode (the artifact directory, containing
        /// the venv and source snapshot, bind-mounted read-only).
        binds: Vec<String>,
        /// Overrides the image's own `CMD`/`ENTRYPOINT`. `None` for
        /// image-mode (the user's own image already knows how to serve
        /// itself); `Some` for Python container-mode (the venv's
        /// `functions-framework` binary).
        cmd: Option<Vec<String>>,
        /// Overrides the image's own working directory. `None` for
        /// image-mode; `Some` for Python container-mode (the bind-mounted
        /// source snapshot).
        working_dir: Option<String>,
    },
}

impl Launch {
    /// Rust source-mode: run the built/copied executable directly, no
    /// extra args, inherit the host's own cwd (unchanged from before this
    /// module existed).
    pub fn rust_process(artifact: PathBuf) -> Self {
        Launch::Process {
            program: artifact,
            args: Vec::new(),
            cwd: None,
        }
    }

    /// Image-mode: run the referenced image as-is, no bind mounts, no
    /// command/working-directory override (unchanged from before this
    /// module existed).
    pub fn image(image_ref: String) -> Self {
        Launch::Container {
            image: image_ref,
            binds: Vec::new(),
            cmd: None,
            working_dir: None,
        }
    }

    /// Python host-mode: run the version's own venv `functions-framework`
    /// binary with the source snapshot as cwd (contracts/
    /// python-function-contract.md's "Startup and environment variables" table).
    ///
    /// `artifact_dir` is the version's artifact directory,
    /// `<data_dir>/artifacts/<name>/<rev>/`, containing `venv/` and `src/`.
    pub fn python_host(artifact_dir: &Path) -> Self {
        Launch::Process {
            program: artifact_dir.join("venv/bin/functions-framework"),
            args: Vec::new(),
            cwd: Some(artifact_dir.join("src")),
        }
    }

    /// Python container-mode: bind-mount the artifact directory read-only
    /// at [`CONTAINER_ARTIFACT_DIR`] inside `image`, and run that bind's
    /// `venv/bin/functions-framework` with cwd set to its `src/` --
    /// mirroring [`Self::python_host`] but through a container, so the
    /// venv's own absolute paths (baked in at creation time) stay valid.
    pub fn python_container(artifact_dir: &Path, image: String) -> Self {
        let container_dir = CONTAINER_ARTIFACT_DIR;
        Launch::Container {
            image,
            binds: vec![format!("{}:{}:ro", artifact_dir.display(), container_dir)],
            cmd: Some(vec![format!(
                "{container_dir}/venv/bin/functions-framework"
            )]),
            working_dir: Some(format!("{container_dir}/src")),
        }
    }
}

/// Where a Python container-mode `Launch::Container` bind-mounts the host's
/// artifact directory inside the container. Fixed (not derived from the
/// host path) so the venv created there at dependency-resolution time
/// (`build/python/container.rs`, same mount point) and the venv run here at
/// launch time see identical absolute paths.
pub const CONTAINER_ARTIFACT_DIR: &str = "/function";

/// Which of [`Launch::Process`]/[`Launch::Container`] a Python instance is
/// being launched as, needed only to pick the right value for the FF
/// contract's `HOST` variable (contracts/python-function-contract.md):
/// `127.0.0.1` for a process on the host's own loopback, `0.0.0.0` for a
/// container's own network namespace (Docker's port/bridge routing can't
/// reach a container's loopback, mirroring [`super::InstanceSpec`]'s
/// existing container-vs-process port convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonLaunchMode {
    Process,
    Container,
}

impl PythonLaunchMode {
    fn host_value(self) -> &'static str {
        match self {
            PythonLaunchMode::Process => "127.0.0.1",
            PythonLaunchMode::Container => "0.0.0.0",
        }
    }
}

/// Builds the Functions Framework environment variables for a Python
/// instance (contracts/python-function-contract.md's "Startup and environment variables"
/// table): `HOST`, `FUNCTION_SOURCE`, `WORKERS`, `THREADS`,
/// `CLOUD_RUN_TIMEOUT_SECONDS`, `LOG_EXECUTION_ID`, `PYTHONUNBUFFERED`,
/// `PYTHONDONTWRITEBYTECODE`, `VIRTUAL_ENV`, `HOME`, and a `PATH` with the
/// venv's `bin/` first.
///
/// Every value here can be overridden by the function's own declared `env`
/// (`function.env`) -- unlike the FF-contract-reserved variables
/// (`PORT`/`FUNCTION_TARGET`/`FUNCTION_SIGNATURE_TYPE`/`K_*`, which
/// `model::validate` already rejects a user from setting), `THREADS` /
/// `WORKERS` / `CLOUD_RUN_TIMEOUT_SECONDS` / `LOG_EXECUTION_ID` /
/// `GUNICORN_LOG_LEVEL` are ordinary Functions Framework knobs a Cloud Run
/// user can already tune, so this project doesn't reserve them either --
/// this function only supplies *defaults*; callers apply the function's own
/// `env` map on top (last write wins). `ProcessDriver`/`ContainerDriver`
/// layer this map *under* their own FF-contract-reserved variables (always
/// win) but *over* their own generic `PATH`/`HOME`/`LANG` baseline defaults
/// (Rust source-mode has no entry here for those keys, so it still gets the
/// generic baseline unchanged).
pub fn python_instance_env(
    function: &Function,
    mode: PythonLaunchMode,
    venv_dir: &Path,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("HOST".to_string(), mode.host_value().to_string());
    env.insert("FUNCTION_SOURCE".to_string(), "main.py".to_string());
    env.insert("WORKERS".to_string(), "1".to_string());
    env.insert("THREADS".to_string(), function.concurrency.to_string());
    env.insert(
        "CLOUD_RUN_TIMEOUT_SECONDS".to_string(),
        function.timeout_secs.to_string(),
    );
    env.insert("LOG_EXECUTION_ID".to_string(), "true".to_string());
    env.insert("PYTHONUNBUFFERED".to_string(), "1".to_string());
    env.insert("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string());
    env.insert(
        "VIRTUAL_ENV".to_string(),
        venv_dir.to_string_lossy().to_string(),
    );
    env.insert(
        "PATH".to_string(),
        format!(
            "{}/bin:/usr/local/bin:/usr/bin:/bin",
            venv_dir.to_string_lossy()
        ),
    );
    // contracts/python-function-contract.md: `HOME` is the artifact root
    // (`<artifact>` for host, `/function` for container) -- `venv_dir` is
    // always that root's own `venv/` child (`<artifact>/venv` or
    // `/function/venv`), so its parent *is* that root.
    env.insert(
        "HOME".to_string(),
        venv_dir
            .parent()
            .unwrap_or(venv_dir)
            .to_string_lossy()
            .to_string(),
    );
    env
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::model::function::{FunctionState, QueuePolicy, Source, Trigger};
    use crate::model::runtime::Runtime;

    fn function_with(concurrency: u32, timeout_secs: u32) -> Function {
        let now = chrono::Utc::now();
        Function {
            name: "hello-py".to_string(),
            trigger: Trigger::Http,
            runtime: Some(Runtime::Python314),
            source: Source::Dir {
                path: "/src".to_string(),
                bin: None,
            },
            env: Default::default(),
            entry_point: "hello".to_string(),
            timeout_secs,
            concurrency,
            memory_mib: 256,
            min_instances: 0,
            max_instances: 1,
            idle_timeout_secs: 300,
            queue_policy: QueuePolicy::Wait,
            queue_max_wait_secs: 30,
            state: FunctionState::Ready,
            current_revision: Some(1),
            last_error: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn rust_process_has_no_args_or_cwd() {
        let launch = Launch::rust_process(PathBuf::from("/artifacts/hello/1/function"));
        match launch {
            Launch::Process { program, args, cwd } => {
                assert_eq!(program, PathBuf::from("/artifacts/hello/1/function"));
                assert!(args.is_empty());
                assert!(cwd.is_none());
            }
            Launch::Container { .. } => panic!("expected Process"),
        }
    }

    #[test]
    fn image_has_no_binds_or_cmd() {
        let launch = Launch::image("ghcr.io/me/hello:1.0".to_string());
        match launch {
            Launch::Container {
                image,
                binds,
                cmd,
                working_dir,
            } => {
                assert_eq!(image, "ghcr.io/me/hello:1.0");
                assert!(binds.is_empty());
                assert!(cmd.is_none());
                assert!(working_dir.is_none());
            }
            Launch::Process { .. } => panic!("expected Container"),
        }
    }

    #[test]
    fn python_host_runs_venv_functions_framework_from_src() {
        let artifact_dir = Path::new("/data/artifacts/hello-py/1");
        let launch = Launch::python_host(artifact_dir);
        match launch {
            Launch::Process { program, args, cwd } => {
                assert_eq!(
                    program,
                    PathBuf::from("/data/artifacts/hello-py/1/venv/bin/functions-framework")
                );
                assert!(args.is_empty());
                assert_eq!(cwd, Some(PathBuf::from("/data/artifacts/hello-py/1/src")));
            }
            Launch::Container { .. } => panic!("expected Process"),
        }
    }

    #[test]
    fn python_container_binds_artifact_dir_readonly_and_sets_cmd_and_cwd() {
        let artifact_dir = Path::new("/data/artifacts/hello-py/1");
        let launch = Launch::python_container(artifact_dir, "uv:python3.14".to_string());
        match launch {
            Launch::Container {
                image,
                binds,
                cmd,
                working_dir,
            } => {
                assert_eq!(image, "uv:python3.14");
                assert_eq!(binds, vec!["/data/artifacts/hello-py/1:/function:ro"]);
                assert_eq!(
                    cmd,
                    Some(vec!["/function/venv/bin/functions-framework".to_string()])
                );
                assert_eq!(working_dir, Some("/function/src".to_string()));
            }
            Launch::Process { .. } => panic!("expected Container"),
        }
    }

    #[test]
    fn python_instance_env_process_uses_loopback_host() {
        let function = function_with(4, 90);
        let env = python_instance_env(
            &function,
            PythonLaunchMode::Process,
            Path::new("/data/artifacts/hello-py/1/venv"),
        );
        assert_eq!(env.get("HOST").map(String::as_str), Some("127.0.0.1"));
        assert_eq!(
            env.get("FUNCTION_SOURCE").map(String::as_str),
            Some("main.py")
        );
        assert_eq!(env.get("WORKERS").map(String::as_str), Some("1"));
        assert_eq!(env.get("THREADS").map(String::as_str), Some("4"));
        assert_eq!(
            env.get("CLOUD_RUN_TIMEOUT_SECONDS").map(String::as_str),
            Some("90")
        );
        assert_eq!(
            env.get("LOG_EXECUTION_ID").map(String::as_str),
            Some("true")
        );
        assert_eq!(env.get("PYTHONUNBUFFERED").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("PYTHONDONTWRITEBYTECODE").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            env.get("VIRTUAL_ENV").map(String::as_str),
            Some("/data/artifacts/hello-py/1/venv")
        );
        assert!(
            env.get("PATH")
                .expect("PATH set")
                .starts_with("/data/artifacts/hello-py/1/venv/bin:")
        );
        assert_eq!(
            env.get("HOME").map(String::as_str),
            Some("/data/artifacts/hello-py/1"),
            "HOME must be the artifact root (venv_dir's parent), not venv_dir itself"
        );
    }

    #[test]
    fn python_instance_env_container_uses_all_interfaces_host() {
        let function = function_with(1, 60);
        let env = python_instance_env(
            &function,
            PythonLaunchMode::Container,
            Path::new("/function/venv"),
        );
        assert_eq!(env.get("HOST").map(String::as_str), Some("0.0.0.0"));
    }
}
