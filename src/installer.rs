//! Self-contained, verified install of OpenMausBot. Nothing from the host is
//! required: a pinned Node.js runtime is downloaded and checked against its
//! SHA-256, and the pinned `openmausbot` npm package is installed with
//! `npm ci` from the lockfile embedded in this library, so npm verifies every
//! package against its `integrity`.
use flate2::read::GzDecoder;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

pub const NODE_VERSION: &str = "v24.22.3";
pub const OMB_VERSION: &str = "0.1.170";
const PACKAGE_JSON: &str = include_str!("../runtime/package.json");
const PACKAGE_LOCK: &str = include_str!("../runtime/package-lock.json");
const MAX_NODE: u64 = 200 * 1024 * 1024;
/// The instance this plugin owns in OpenMausBot's `config.json`.
pub const POM_INSTANCE: &str = "pom";
/// The variable the server reads the gateway key from; it is never written to disk.
pub const POM_KEY_ENV: &str = "POM_API_KEY";

#[derive(Clone, Copy)]
struct Artifact {
    name: &'static str,
    sha: &'static str,
    zipped: bool,
}

fn node_artifact() -> Result<Artifact, String> {
    let (name, sha, zipped) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => (
            "node-v24.22.3-linux-x64.tar.gz",
            "061ec3e3062115705b56b51fa9cc330ab50d34b6969387d4e4b1a50c6809acc2",
            false,
        ),
        ("linux", "aarch64") => (
            "node-v24.22.3-linux-arm64.tar.gz",
            "261af2af250dfd7e9f25cf151d1b2713a0adf8f10a9ad116c4947f68627affe8",
            false,
        ),
        ("macos", "aarch64") => (
            "node-v24.22.3-darwin-arm64.tar.gz",
            "ff6e17d4874b76d272212b908cc020d6e815fba5a6c7eaaa06add725840307fd",
            false,
        ),
        ("macos", "x86_64") => (
            "node-v24.22.3-darwin-x64.tar.gz",
            "977614af0504c81045e7fd85596dbca0e5f951b96681516dadafedcca8c447f7",
            false,
        ),
        ("windows", "x86_64") => (
            "node-v24.22.3-win-x64.zip",
            "e612af99f626ab415ea225af6bce5c971feb448cb5cae250bb1c3fa50c5f7d9f",
            true,
        ),
        _ => return Err("OpenMausBot does not support this operating system/architecture".into()),
    };
    Ok(Artifact { name, sha, zipped })
}

fn node_url(artifact: Artifact) -> String {
    format!("https://nodejs.org/dist/{NODE_VERSION}/{}", artifact.name)
}

/// The stages of an install, in order, as the screen reports them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    Node,
    Packages,
    Connect,
}

impl Step {
    pub fn code(self) -> &'static str {
        match self {
            Step::Node => "node",
            Step::Packages => "packages",
            Step::Connect => "connect",
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum InstallMode {
    Fresh,
    Update,
    Installed,
}

/// What `.installed` records: the pinned runtime and package together.
fn install_stamp() -> String {
    format!("node {NODE_VERSION}\nopenmausbot {OMB_VERSION}\n")
}

pub fn install_mode(base: &Path) -> InstallMode {
    match fs::read_to_string(base.join(".installed")) {
        Err(_) => InstallMode::Fresh,
        Ok(stamp)
            if stamp == install_stamp()
                && omb_cli(base).is_file()
                && node_binary(base).is_file() =>
        {
            InstallMode::Installed
        }
        Ok(_) => InstallMode::Update,
    }
}

pub fn node_binary(base: &Path) -> PathBuf {
    let node = base.join("node");
    if cfg!(windows) {
        node.join("node.exe")
    } else {
        node.join("bin/node")
    }
}

fn npm_cli(base: &Path) -> PathBuf {
    let node = base.join("node");
    if cfg!(windows) {
        node.join("node_modules/npm/bin/npm-cli.js")
    } else {
        node.join("lib/node_modules/npm/bin/npm-cli.js")
    }
}

pub fn omb_cli(base: &Path) -> PathBuf {
    base.join("app/node_modules/openmausbot/cli.js")
}

fn download(
    url: &str,
    target: &Path,
    sha: &str,
    max: u64,
    progress: &mut dyn FnMut(f64),
) -> Result<(), String> {
    if target.is_file() && hash_file(target).is_ok_and(|hash| hash == sha) {
        return Ok(());
    }
    let part = target.with_extension("download");
    let response = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(60))
        .redirects(5)
        .build()
        .get(url)
        .call()
        .map_err(|err| match err {
            ureq::Error::Status(code, _) => format!("Node.js download failed (HTTP {code})"),
            ureq::Error::Transport(transport) => {
                format!(
                    "Node.js download failed ({:?}); check network access",
                    transport.kind()
                )
            }
        })?;
    let total = response
        .header("Content-Length")
        .and_then(|length| length.parse::<u64>().ok())
        .filter(|length| *length > 0)
        .unwrap_or(max);
    let mut reader = response.into_reader();
    let mut out = fs::File::create(&part).map_err(|_| "Cannot write Node.js download")?;
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut bytes = [0u8; 256 * 1024];
    let result = (|| -> Result<(), String> {
        loop {
            let read = reader
                .read(&mut bytes)
                .map_err(|_| "Node.js download interrupted")?;
            if read == 0 {
                break;
            }
            size += read as u64;
            if size > max {
                return Err("Node.js download exceeds size limit".into());
            }
            out.write_all(&bytes[..read])
                .map_err(|_| "Cannot write Node.js download")?;
            hash.update(&bytes[..read]);
            progress((size as f64 / total as f64).min(1.0));
        }
        out.sync_all()
            .map_err(|_| "Cannot commit Node.js download")?;
        if format!("{:x}", hash.finalize()) != sha {
            return Err("Node.js download checksum mismatch".into());
        }
        if target.exists() {
            fs::remove_file(target).map_err(|_| "Cannot replace Node.js download")?;
        }
        fs::rename(&part, target).map_err(|_| "Cannot publish Node.js download".to_owned())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

fn hash_file(path: &Path) -> Result<String, io::Error> {
    let mut hash = Sha256::new();
    io::copy(&mut fs::File::open(path)?, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

/// Archive member path under the archive's single top folder, confined.
fn confined(path: &Path, root: &str) -> Option<PathBuf> {
    let relative = path.strip_prefix(root).ok()?;
    let ok = !relative.as_os_str().is_empty()
        && relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
    ok.then(|| relative.to_path_buf())
}

/// Unpacks the Node.js archive into `<base>/node`. Only directories and
/// regular files are extracted; npm and node are run by path, so the
/// archive's convenience symlinks (`bin/npm`, `bin/npx`) are not needed.
fn extract_node(archive: &Path, base: &Path, artifact: Artifact) -> Result<(), String> {
    let marker = base.join("node/.verified");
    if fs::read_to_string(&marker).ok().as_deref() == Some(artifact.sha)
        && node_binary(base).is_file()
    {
        return Ok(());
    }
    let stage = base.join("node.partial");
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(&stage).map_err(|_| "Cannot prepare Node.js directory")?;
    let root = artifact
        .name
        .trim_end_matches(".tar.gz")
        .trim_end_matches(".zip");
    let write = |relative: &Path, reader: &mut dyn Read, mode: Option<u32>| -> Result<(), String> {
        let dest = stage.join(relative);
        fs::create_dir_all(dest.parent().ok_or("Invalid Node.js path")?)
            .map_err(|_| "Cannot create Node.js directory")?;
        let mut out = fs::File::create(&dest).map_err(|_| "Cannot write Node.js runtime")?;
        io::copy(reader, &mut out).map_err(|_| "Cannot extract Node.js runtime")?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest, fs::Permissions::from_mode(mode & 0o755))
                .map_err(|_| "Cannot set Node.js permissions")?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        Ok(())
    };
    let file = fs::File::open(archive).map_err(|_| "Cannot open Node.js archive")?;
    if artifact.zipped {
        let mut zip = zip::ZipArchive::new(file).map_err(|_| "Invalid Node.js archive")?;
        for index in 0..zip.len() {
            let mut entry = zip
                .by_index(index)
                .map_err(|_| "Invalid Node.js archive member")?;
            let Some(path) = entry.enclosed_name() else {
                return Err("Unsafe Node.js archive path".into());
            };
            if entry.is_dir() || path == Path::new(root) {
                continue;
            }
            let relative = confined(&path, root).ok_or("Unexpected Node.js archive path")?;
            write(&relative, &mut entry, None)?;
        }
    } else {
        let mut tar = tar::Archive::new(GzDecoder::new(file));
        for entry in tar.entries().map_err(|_| "Invalid Node.js archive")? {
            let mut entry = entry.map_err(|_| "Invalid Node.js archive member")?;
            let kind = entry.header().entry_type();
            if kind.is_dir()
                || kind.is_symlink()
                || kind.is_pax_global_extensions()
                || kind.is_pax_local_extensions()
            {
                continue;
            }
            if !kind.is_file() {
                return Err("Node.js archive contains an unsupported file type".into());
            }
            let path = entry
                .path()
                .map_err(|_| "Invalid Node.js archive path")?
                .into_owned();
            let relative = confined(&path, root).ok_or("Unexpected Node.js archive path")?;
            let mode = entry.header().mode().ok();
            write(&relative, &mut entry, mode)?;
        }
    }
    if !stage
        .join(
            node_binary(Path::new(""))
                .strip_prefix("node")
                .unwrap_or(Path::new("")),
        )
        .is_file()
    {
        return Err("Verified Node.js runtime is incomplete".into());
    }
    fs::write(stage.join(".verified"), artifact.sha).map_err(|_| "Cannot mark Node.js runtime")?;
    let target = base.join("node");
    if target.exists() {
        fs::remove_dir_all(&target).map_err(|_| "Cannot replace Node.js runtime")?;
    }
    fs::rename(&stage, &target).map_err(|_| "Cannot publish Node.js runtime".to_owned())
}

/// The environment of every Node process this plugin starts: only what it needs.
pub fn node_environment(command: &mut Command, base: &Path, home: &Path) {
    let bin = node_binary(base)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut path =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    path.insert(0, bin);
    command
        .env_clear()
        .env("PATH", std::env::join_paths(path).unwrap_or_default())
        .env("HOME", home)
        .env("npm_config_cache", base.join("npm-cache"))
        .env("npm_config_update_notifier", "false")
        .env("npm_config_fund", "false")
        .env("npm_config_audit", "false");
    for name in [
        "LANG",
        "LC_ALL",
        "TZ",
        "TMPDIR",
        "SYSTEMROOT",
        "SystemRoot",
        "WINDIR",
        "TEMP",
        "TMP",
        "APPDATA",
        "LOCALAPPDATA",
        "USERPROFILE",
        "PATHEXT",
        "COMSPEC",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    #[cfg(windows)]
    command.env("USERPROFILE", home);
}

/// Installs or checks the pinned runtime. `run` executes a command to its end
/// (logging to `install.log`); `report` tells the screen the step and, for a
/// download, its fraction.
pub fn ensure(
    base: &Path,
    home: &Path,
    run: impl Fn(Command) -> Result<(), String>,
    report: &dyn Fn(Step, Option<f64>),
) -> Result<(), String> {
    if install_mode(base) == InstallMode::Installed {
        return Ok(());
    }
    let artifact = node_artifact()?;
    report(Step::Node, Some(0.0));
    let archive = base.join(artifact.name);
    download(
        &node_url(artifact),
        &archive,
        artifact.sha,
        MAX_NODE,
        &mut |done| report(Step::Node, Some(done)),
    )?;
    extract_node(&archive, base, artifact)?;
    let _ = fs::remove_file(&archive);

    report(Step::Packages, None);
    let app = base.join("app");
    fs::create_dir_all(&app).map_err(|_| "Cannot create OpenMausBot directory")?;
    fs::write(app.join("package.json"), PACKAGE_JSON)
        .map_err(|_| "Cannot write OpenMausBot package")?;
    fs::write(app.join("package-lock.json"), PACKAGE_LOCK)
        .map_err(|_| "Cannot write OpenMausBot lockfile")?;
    let mut command = Command::new(node_binary(base));
    command
        .arg(npm_cli(base))
        .args([
            "ci",
            "--omit=dev",
            "--ignore-scripts",
            "--no-fund",
            "--no-audit",
        ])
        .current_dir(&app);
    node_environment(&mut command, base, home);
    run(command)?;
    if !omb_cli(base).is_file() {
        return Err("OpenMausBot package is incomplete; see install.log".into());
    }
    fs::write(base.join(".installed"), install_stamp())
        .map_err(|_| "Cannot mark OpenMausBot install")?;
    Ok(())
}

/// Adds or refreshes the `pom` OpenAI-compatible instance in OpenMausBot's
/// `config.json`, pointing at the node's gateway. The key itself is not
/// written: the server reads it from [`POM_KEY_ENV`]. An instance named `pom`
/// that this plugin did not create (another driver) is left alone, and every
/// other field of the file is kept.
pub fn register_pom(data: &Path, base_url: &str, model: Option<&str>) -> Result<(), String> {
    let url = base_url.trim_end_matches('/');
    if !url.starts_with("http://127.0.0.1:") && !url.starts_with("http://localhost:") {
        return Err("POM gateway must be loopback".into());
    }
    let path = data.join("config.json");
    let mut root: Value = match fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|_| "OpenMausBot config.json is not valid JSON")?
        }
        Err(_) => json!({}),
    };
    let object = root
        .as_object_mut()
        .ok_or("OpenMausBot config.json is not an object")?;
    let instances = object.entry("instances").or_insert_with(|| json!({}));
    let instances = instances
        .as_object_mut()
        .ok_or("OpenMausBot instances is not an object")?;
    let changed = match instances.get_mut(POM_INSTANCE) {
        Some(existing)
            if existing["driver"] != "openai-compat"
                || existing["config"]["apiKeyEnv"] != POM_KEY_ENV =>
        {
            false
        }
        Some(existing) => {
            let config = existing["config"]
                .as_object_mut()
                .ok_or("Invalid POM instance")?;
            let mut changed = config.get("url").and_then(Value::as_str) != Some(url);
            config.insert("url".into(), json!(url));
            if config.get("model").is_none() {
                if let Some(model) = model {
                    config.insert("model".into(), json!(model));
                    changed = true;
                }
            }
            changed
        }
        None => {
            let mut config = Map::new();
            config.insert("url".into(), json!(url));
            config.insert("apiKeyEnv".into(), json!(POM_KEY_ENV));
            config.insert("catalog".into(), json!("openai"));
            if let Some(model) = model {
                config.insert("model".into(), json!(model));
            }
            instances.insert(
                POM_INSTANCE.into(),
                json!({"driver": "openai-compat", "displayName": "POM", "config": Value::Object(config)}),
            );
            true
        }
    };
    if !changed {
        return Ok(());
    }
    fs::create_dir_all(data).map_err(|_| "Cannot create OpenMausBot data directory")?;
    let text =
        serde_json::to_string_pretty(&root).map_err(|_| "Cannot encode OpenMausBot config")?;
    let part = path.with_extension("json.pom-partial");
    fs::write(&part, text + "\n").map_err(|_| "Cannot write OpenMausBot config")?;
    if path.exists() && cfg!(windows) {
        fs::remove_file(&path).map_err(|_| "Cannot replace OpenMausBot config")?;
    }
    fs::rename(&part, &path).map_err(|_| "Cannot publish OpenMausBot config".to_owned())
}

/// The first model the gateway lists, to seed the POM instance's default.
pub fn first_gateway_model(base_url: &str, api_key: &str) -> Option<String> {
    let reply: Value = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(5))
        .try_proxy_from_env(false)
        .build()
        .get(&format!("{}/models", base_url.trim_end_matches('/')))
        .set("Authorization", &format!("Bearer {api_key}"))
        .call()
        .ok()?
        .into_json()
        .ok()?;
    reply["data"]
        .as_array()?
        .iter()
        .find_map(|model| model["id"].as_str().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("omb-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn pins_are_complete_and_the_lockfile_matches_the_version() {
        let artifact = node_artifact().unwrap();
        assert_eq!(artifact.sha.len(), 64);
        assert!(artifact.name.starts_with(&format!("node-{NODE_VERSION}-")));
        let lock: Value = serde_json::from_str(PACKAGE_LOCK).unwrap();
        let package = &lock["packages"]["node_modules/openmausbot"];
        assert_eq!(package["version"], OMB_VERSION);
        assert!(package["integrity"]
            .as_str()
            .unwrap()
            .starts_with("sha512-"));
        let manifest: Value = serde_json::from_str(PACKAGE_JSON).unwrap();
        assert_eq!(manifest["dependencies"]["openmausbot"], OMB_VERSION);
        for (name, entry) in lock["packages"].as_object().unwrap() {
            assert!(
                entry.get("hasInstallScript").is_none(),
                "{name} has an install script"
            );
        }
    }

    #[test]
    fn archive_paths_are_confined_to_the_top_folder() {
        let root = "node-v24.22.3-linux-x64";
        assert_eq!(
            confined(Path::new("node-v24.22.3-linux-x64/bin/node"), root).unwrap(),
            Path::new("bin/node")
        );
        assert!(confined(Path::new("other/bin/node"), root).is_none());
        assert!(confined(Path::new("node-v24.22.3-linux-x64/../etc"), root).is_none());
        assert!(confined(Path::new("node-v24.22.3-linux-x64"), root).is_none());
    }

    #[test]
    fn install_mode_tells_fresh_update_and_installed_apart() {
        let base = temp("mode");
        assert_eq!(install_mode(&base), InstallMode::Fresh);
        fs::write(base.join(".installed"), "node v1\nopenmausbot 0.0.1\n").unwrap();
        assert_eq!(install_mode(&base), InstallMode::Update);
        fs::write(base.join(".installed"), install_stamp()).unwrap();
        assert_eq!(install_mode(&base), InstallMode::Update, "files missing");
        for file in [omb_cli(&base), node_binary(&base)] {
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, "").unwrap();
        }
        assert_eq!(install_mode(&base), InstallMode::Installed);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn pom_instance_is_added_refreshed_and_never_carries_the_key() {
        let data = temp("config");
        fs::write(
            data.join("config.json"),
            r#"{"theme":"dark","instances":{"mine":{"driver":"claude"}}}"#,
        )
        .unwrap();
        assert!(register_pom(&data, "http://10.0.0.1:8080/v1", None).is_err());
        register_pom(&data, "http://127.0.0.1:8080/v1/", Some("org/model")).unwrap();
        let config: Value =
            serde_json::from_str(&fs::read_to_string(data.join("config.json")).unwrap()).unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(config["instances"]["mine"]["driver"], "claude");
        let pom = &config["instances"]["pom"];
        assert_eq!(pom["driver"], "openai-compat");
        assert_eq!(pom["config"]["url"], "http://127.0.0.1:8080/v1");
        assert_eq!(pom["config"]["apiKeyEnv"], POM_KEY_ENV);
        assert_eq!(pom["config"]["model"], "org/model");
        assert!(pom["environment"].is_null());

        // A new gateway port moves the URL; the operator's model choice stays.
        register_pom(&data, "http://127.0.0.1:9090/v1", Some("other")).unwrap();
        let config: Value =
            serde_json::from_str(&fs::read_to_string(data.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            config["instances"]["pom"]["config"]["url"],
            "http://127.0.0.1:9090/v1"
        );
        assert_eq!(config["instances"]["pom"]["config"]["model"], "org/model");

        // An instance named `pom` that is not ours is left alone.
        fs::write(
            data.join("config.json"),
            r#"{"instances":{"pom":{"driver":"customAcp"}}}"#,
        )
        .unwrap();
        register_pom(&data, "http://127.0.0.1:8080/v1", None).unwrap();
        let config: Value =
            serde_json::from_str(&fs::read_to_string(data.join("config.json")).unwrap()).unwrap();
        assert_eq!(config["instances"]["pom"], json!({"driver": "customAcp"}));
        fs::remove_dir_all(data).unwrap();
    }

    /// Full install from the network into a temporary directory: downloads
    /// Node.js, runs `npm ci` and checks the CLI answers its version.
    #[test]
    #[ignore]
    fn installs_the_pinned_runtime() {
        let base = temp("install");
        let home = base.join("home");
        fs::create_dir_all(&home).unwrap();
        ensure(
            &base,
            &home,
            |mut command| {
                let status = command.status().map_err(|e| e.to_string())?;
                status
                    .success()
                    .then_some(())
                    .ok_or_else(|| "npm failed".to_string())
            },
            &|_, _| {},
        )
        .unwrap();
        assert_eq!(install_mode(&base), InstallMode::Installed);
        let mut version = Command::new(node_binary(&base));
        version.arg(omb_cli(&base)).arg("--version");
        node_environment(&mut version, &base, &home);
        let output = version.output().unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains(OMB_VERSION));
        fs::remove_dir_all(base).unwrap();
    }
}
