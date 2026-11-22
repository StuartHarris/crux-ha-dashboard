//! Reads the build-time configuration from `cfg.toml` and writes it into the
//! firmware as a constant. Also gives the firmware its version.
//!
//! The constant goes to `$OUT_DIR/config.rs`, which is in the `target`
//! directory. The build never prints a value from the file.
//!
//! The version is in the environment variable `FIRMWARE_VERSION` of the
//! compiler: the version of the package, `+`, and a short commit id, for
//! example `0.1.0+1b073946` or `0.1.0+1b073946.dirty`. The commit id is
//! build metadata of `SemVer`. Refer to [`commit`].

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{self, Command},
};

#[path = "logic/src/config.rs"]
mod config;

/// The environment variable that gives another configuration file, for
/// example `cfg.toml.example` in CI.
const PATH_VARIABLE: &str = "DASHBOARD_CONFIG";

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    version(&manifest);
    // A relative path is relative to the `firmware` directory.
    let path = manifest.join(env::var_os(PATH_VARIABLE).unwrap_or_else(|| "cfg.toml".into()));
    println!("cargo:rerun-if-env-changed={PATH_VARIABLE}");
    println!("cargo:rerun-if-changed={}", path.display());
    println!("cargo:rerun-if-changed=logic/src/config.rs");

    let Ok(text) = fs::read_to_string(&path) else {
        eprintln!(
            "error: cannot read {}. Copy cfg.toml.example to cfg.toml and put \
             your Wi-Fi and broker settings in it.",
            path.display()
        );
        process::exit(1);
    };
    let config = match config::Config::parse(&text) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: {}: {error}", path.display());
            process::exit(1);
        }
    };

    let config::Config {
        wifi_ssid,
        wifi_password,
        wifi_auth,
        mqtt_host,
        mqtt_port,
        mqtt_username,
        mqtt_password,
        device_id,
        read_only,
        force_battery,
    } = config;
    let source = format!(
        "/// The build-time configuration, from `cfg.toml`.\n\
         pub const CONFIG: logic::config::Config<'static> = logic::config::Config {{\n\
         wifi_ssid: {wifi_ssid:?},\n\
         wifi_password: {wifi_password:?},\n\
         wifi_auth: logic::config::WifiAuth::{wifi_auth:?},\n\
         mqtt_host: {mqtt_host:?},\n\
         mqtt_port: {mqtt_port},\n\
         mqtt_username: {mqtt_username:?},\n\
         mqtt_password: {mqtt_password:?},\n\
         device_id: {device_id:?},\n\
         read_only: {read_only},\n\
         force_battery: {force_battery},\n\
         }};\n"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("set by cargo"));
    fs::write(out.join("config.rs"), source).expect("write the configuration");
}

/// Gives the compiler the version of the firmware in `FIRMWARE_VERSION`, and
/// tells cargo to run this script again when the commit can change.
fn version(manifest: &Path) {
    let root = manifest.parent().unwrap_or(manifest);
    // Each jj operation changes `op_heads/heads`. A move of a detached HEAD
    // changes `HEAD`, and each commit on a branch adds a line to
    // `logs/HEAD`. Cargo always runs the script again for a path that is not
    // there, so the script names only the paths that are there.
    let git = git_dir(root);
    let paths = [
        Some(root.join(".jj/repo/op_heads/heads")),
        git.as_ref().map(|git| git.join("HEAD")),
        git.as_ref().map(|git| git.join("logs/HEAD")),
    ];
    for path in paths.into_iter().flatten() {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    let package = env::var("CARGO_PKG_VERSION").expect("set by cargo");
    println!(
        "cargo:rustc-env=FIRMWARE_VERSION={package}+{}",
        commit(root)
    );
}

/// Returns the git directory of the repository at `root`, or `None` if
/// there is no repository or no git.
///
/// In the default workspace of a colocated repo, `.git` is a directory. In a
/// git worktree or another colocated workspace, `.git` is a file that names
/// the git directory. `git rev-parse --absolute-git-dir` reads both, and
/// changes nothing.
fn git_dir(root: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let path = PathBuf::from(text.trim());
    path.is_dir().then_some(path)
}

/// Returns a short id of the commit that the firmware builds from.
///
/// 1. The jj commit id (8 hex digits) of the nearest change that has code:
///    the working copy, or its nearest ancestor that is not empty. If the
///    working copy has edits, the id ends with `.dirty`.
///
///    The jj commands use `--at-op=@`, so they take no snapshot of the
///    working copy and change nothing. Thus `.dirty` tells the state of the
///    working copy at the last jj command. An edit after the last jj
///    command does not count.
/// 2. Else the git commit of `HEAD`.
/// 3. Else `unknown`, for example in a copy of the source with no
///    repository.
///
/// A missing tool or a failed command does not stop the build.
fn commit(root: &Path) -> String {
    let jj = |revset: &str, template: &str| {
        let args = [
            "--at-op=@",
            "--no-pager",
            "--color=never",
            "log",
            "-r",
            revset,
            "--no-graph",
            "-T",
            template,
        ];
        run(root, "jj", &args)
    };
    let nearest = jj("latest(heads(::@ & ~empty()))", "commit_id.short(8)");
    let jj_id = nearest.and_then(|id| {
        let state = jj("@", r#"if(empty, "clean", "dirty")"#)?;
        Some(if state == "dirty" {
            format!("{id}.dirty")
        } else {
            id
        })
    });
    let git = ["rev-parse", "--short=8", "HEAD"];
    jj_id
        .or_else(|| run(root, "git", &git))
        .unwrap_or_else(|| "unknown".into())
}

/// Runs `program` with `args` in `root`, and returns its output as one
/// word. Returns `None` if the command fails or gives no word.
fn run(root: &Path, program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let word = text.trim();
    let valid = !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric());
    // A `.dirty` suffix comes after this check, so each word is one
    // identifier of build metadata.
    valid.then(|| word.into())
}
