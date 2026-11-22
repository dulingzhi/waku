//! Headless Computer Use state and helper lifecycle.

use std::fs;
#[cfg(target_os = "macos")]
use std::io::Write as _;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;
#[cfg(target_os = "macos")]
use serde_json::json;
use uuid::Uuid;

#[cfg(target_os = "macos")]
const MAX_HELPER_OUTPUT_BYTES: usize = 24 * 1024 * 1024;

pub use waku_protocol::computer_use::{
    ComputerAppGrant, ComputerPermissions, ComputerTarget, ComputerUsePhase, ComputerUseState,
    is_available, resolve_enabled,
};

#[derive(Clone, Debug)]
pub struct ComputerToolRequest {
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
}

impl ComputerToolRequest {
    pub fn summary(&self) -> String {
        if self.tool != "use" {
            return match self.tool.as_str() {
                "status" => "Check computer-use access".into(),
                _ => self.tool.clone(),
            };
        }
        let actions = self
            .arguments
            .get("actions")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if actions.is_empty() {
            return "Inspect the window".into();
        }
        let mut labels = actions
            .iter()
            .filter_map(|action| action.get("type").and_then(Value::as_str))
            .map(action_label)
            .collect::<Vec<_>>();
        labels.dedup();
        format!("{} {}", labels.join(", "), plural(actions.len(), "action"))
    }
}

fn action_label(action: &str) -> &'static str {
    match action {
        "click" | "double_click" => "Click",
        "move" => "Move the pointer",
        "drag" => "Drag",
        "scroll" => "Scroll",
        "type" => "Type text",
        "keypress" => "Press keys",
        "wait" => "Wait",
        _ => "Interact",
    }
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComputerUsePreviewUpdate {
    target: ComputerTarget,
    image_url: String,
}

pub fn decode_preview_update(data: &[u8]) -> anyhow::Result<ComputerUseState> {
    let update: ComputerUsePreviewUpdate =
        serde_json::from_slice(data).context("Computer Use preview is invalid JSON")?;
    validate_preview_image_url(&update.image_url)?;
    Ok(ComputerUseState {
        target: Some(update.target),
        phase: ComputerUsePhase::Running,
        visible: true,
        image_url: Some(update.image_url),
    })
}

fn validate_preview_image_url(image_url: &str) -> anyhow::Result<()> {
    const PNG_PREFIX: &str = "data:image/png;base64,";
    let encoded = image_url
        .strip_prefix(PNG_PREFIX)
        .ok_or_else(|| anyhow!("Computer Use preview is not a PNG data URL"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("Computer Use preview contains invalid base64")?;
    if bytes.is_empty() {
        bail!("Computer Use preview is empty");
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct PendingComputerApproval {
    pub request: ComputerToolRequest,
    pub target: ComputerTarget,
    pub sensitive: bool,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HelperResponse {
    success: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    permissions: Option<ComputerPermissions>,
}

#[cfg(target_os = "macos")]
pub fn probe_permissions(prompt: bool) -> anyhow::Result<ComputerPermissions> {
    // A release daemon never installs or launches the helper app, not even to
    // read permission status, so production never opens its TCC prompts.
    if !is_available() {
        bail!("Waku Computer Use is not available in this build");
    }
    let operation = if prompt {
        json!({"operation": "requestPermissions"})
    } else {
        json!({"operation": "status"})
    };
    let helper = mcp_server_command()?;
    let active_helper_pid = AtomicU32::new(0);
    let response = invoke_helper_direct(&helper, &operation, &active_helper_pid)?;
    if !response.success {
        bail!(
            "{}",
            response
                .error
                .unwrap_or_else(|| tr!("computer_use.permission_check_failed"))
        );
    }
    Ok(response.permissions.unwrap_or_default())
}

#[cfg(not(target_os = "macos"))]
pub fn probe_permissions(_prompt: bool) -> anyhow::Result<ComputerPermissions> {
    bail!(
        "Use Cua Driver check_permissions to inspect this desktop's capture and input capabilities"
    )
}

#[cfg(target_os = "macos")]
fn invoke_helper_direct(
    helper: &Path,
    operation: &Value,
    active_helper_pid: &AtomicU32,
) -> anyhow::Result<HelperResponse> {
    let mode = match operation.get("operation").and_then(Value::as_str) {
        Some("status") => Some("status"),
        Some("requestPermissions") => Some("request-permissions"),
        _ => None,
    };
    let mut command = Command::new(helper);
    if let Some(mode) = mode {
        command.arg(mode);
    }
    let command = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::command_env::spawn(command)
        .with_context(|| format!("failed to start {}", helper.display()))?;
    let pid = child.id();
    active_helper_pid.store(pid, Ordering::SeqCst);
    let payload = serde_json::to_vec(operation)?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("computer-use helper stdin unavailable"))?
        .write_all(&payload)?;
    let output = child.wait_with_output()?;
    let _ = active_helper_pid.compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst);
    if output.stdout.len() > MAX_HELPER_OUTPUT_BYTES {
        bail!("computer-use helper returned too much data");
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("computer-use helper failed: {}", stderr.trim());
    }
    serde_json::from_slice(&output.stdout).context("computer-use helper returned invalid JSON")
}

fn helper_app_path() -> anyhow::Result<PathBuf> {
    let executable = host_executable_path()?;
    let macos = executable
        .parent()
        .ok_or_else(|| anyhow!("Waku executable has no parent directory"))?;
    let contents = macos
        .parent()
        .ok_or_else(|| anyhow!("Waku app bundle is malformed"))?;
    let app_name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("Waku executable name is invalid"))?;
    let helper_name = format!("{app_name} Computer Use");
    let path = contents.join("Helpers").join(format!("{helper_name}.app"));
    if !path.is_dir() {
        bail!("Computer Use helper is missing from this Waku build")
    }
    Ok(path)
}

pub fn helper_display_name() -> String {
    host_executable_path()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .map(|app_name| format!("{app_name} Computer Use"))
        .unwrap_or_else(|| "Waku Computer Use".into())
}

/// A child process Waku launches: the program, plus the arguments that select a
/// mode inside it.
///
/// Packaged builds ship one `waku` executable and reach the JavaScript REPL and
/// the Cua Driver host through `--js-repl` / `--computer-use`, so a consumer can
/// no longer be handed a bare path. Both halves travel together because they are
/// only correct as a pair: `waku.exe --computer-use mcp` is the helper, while
/// `waku.exe` alone is the GUI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelperCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl HelperCommand {
    /// The command a consumer writes into a spawn call.
    pub fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        command.args(&self.args);
        command
    }

    /// The command as the `[program, args…]` list a JSON MCP config wants.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = vec![self.program.display().to_string()];
        argv.extend(self.args.iter().cloned());
        argv
    }

    /// The mode arguments as a JSON array, for consumers that carry a helper
    /// through one string-valued environment variable.
    ///
    /// `WAKU_COMPUTER_USE_SERVER` is the program and this is what to pass it, so
    /// a merged `waku` reaches the same mode a standalone helper would.
    pub fn args_json(&self) -> String {
        serde_json::to_string(&self.args).unwrap_or_else(|_| "[]".into())
    }
}

/// Resolve one helper to a running command.
///
/// A dedicated helper binary next to `waku` still wins when it exists: the dev
/// watcher rebuilds `waku_js_repl` and `waku_computer_use` on their own, and a
/// standalone helper needs no mode word. A package ships only `waku`, so it
/// resolves to the merged program plus the flag that selects the same mode.
fn helper_command(standalone: Option<PathBuf>, mode: &str) -> anyhow::Result<HelperCommand> {
    if let Some(program) = standalone.filter(|path| path.is_file()) {
        return Ok(HelperCommand {
            program,
            args: Vec::new(),
        });
    }
    Ok(HelperCommand {
        program: host_executable_path()?,
        args: vec![format!("--{mode}")],
    })
}

pub fn mcp_server_command() -> anyhow::Result<HelperCommand> {
    if !cfg!(target_os = "macos") {
        let standalone = host_executable_path()?.with_file_name(helper_executable_name());
        return helper_command(Some(standalone), "computer-use");
    }
    // macOS installs a signed Launch Services helper, which is a different
    // program with its own TCC identity rather than a mode of `waku`.
    let bundled_helper = helper_app_path()?;
    let helper = install_helper_app(&bundled_helper)?;
    let executable = helper
        .file_stem()
        .ok_or_else(|| anyhow!("Computer Use helper name is invalid"))?;
    let program = helper.join("Contents").join("MacOS").join(executable);
    Ok(HelperCommand {
        program,
        args: Vec::new(),
    })
}

fn helper_executable_name() -> &'static str {
    if cfg!(windows) {
        "waku_computer_use.exe"
    } else {
        "waku_computer_use"
    }
}

fn resources_directory(executable: &Path, os: &str) -> anyhow::Result<PathBuf> {
    let directory = executable
        .parent()
        .ok_or_else(|| anyhow!("Waku executable has no parent"))?;
    Ok(match os {
        "macos" => directory
            .parent()
            .ok_or_else(|| anyhow!("Waku app bundle is malformed"))?
            .join("Resources"),
        "linux" if directory.file_name().is_some_and(|name| name == "bin") => directory
            .parent()
            .ok_or_else(|| anyhow!("Waku installation is malformed"))?
            .join("share/waku"),
        _ => directory.join("resources"),
    })
}

fn packaged_file(path: &Path, name: &str) -> anyhow::Result<PathBuf> {
    if !path.is_file() {
        bail!("{name} is missing from this Waku build: {}", path.display());
    }
    Ok(path.to_path_buf())
}

pub fn js_repl_server_path() -> anyhow::Result<HelperCommand> {
    // macOS keeps code in `Contents/MacOS` and resources in `Contents/Resources`.
    // A packaged build has one executable, so the REPL is a mode of it rather
    // than a second program under `Resources`; a development bundle that still
    // has the old standalone copy keeps using it, which is why this prefers a
    // file that exists over a mode.
    let executable = host_executable_path()?;
    let standalone = if cfg!(target_os = "macos") {
        resources_directory(&executable, "macos")?.join("waku_js_repl")
    } else {
        executable.with_file_name(if cfg!(windows) {
            "waku_js_repl.exe"
        } else {
            "waku_js_repl"
        })
    };
    helper_command(Some(standalone), "js-repl")
}

pub fn pi_extension_path() -> anyhow::Result<PathBuf> {
    let path = resources_directory(&host_executable_path()?, std::env::consts::OS)?
        .join("computer-use/pi-extension.ts");
    packaged_file(&path, "Waku Pi Computer Use extension")
}

/// Install the bundled helper as an independent, stable runtime service.
///
/// Screen Recording differs from Accessibility on macOS: it follows the
/// responsible application. A helper launched from inside Waku's bundle is
/// therefore attributed to Waku even though the capture API runs in the
/// helper. Launching this standalone copy through Launch Services gives the
/// helper its own TCC identity while the signed app bundle remains the source
/// shipped with Waku.
fn install_helper_app(source: &Path) -> anyhow::Result<PathBuf> {
    let application_support =
        dirs::data_dir().ok_or_else(|| anyhow!("Application Support directory is unavailable"))?;
    let install_root = application_support.join("Waku").join("Computer Use");
    crate::fs_ext::create_private_dir_all(&install_root)
        .with_context(|| format!("could not create {}", install_root.display()))?;
    let bundle_name = source
        .file_name()
        .ok_or_else(|| anyhow!("Computer Use helper bundle name is invalid"))?;
    let destination = install_root.join(bundle_name);
    if helper_install_matches(source, &destination)? {
        return Ok(destination);
    }

    let staging = install_root.join(format!(".install-{}.app", Uuid::new_v4().simple()));
    copy_directory(source, &staging)?;
    let previous = install_root.join(format!(".previous-{}.app", Uuid::new_v4().simple()));
    let had_previous = destination.exists();
    if had_previous {
        fs::rename(&destination, &previous)
            .with_context(|| format!("could not replace {}", destination.display()))?;
    }
    if let Err(error) = fs::rename(&staging, &destination) {
        if had_previous {
            let _ = fs::rename(&previous, &destination);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(error).context("could not install Computer Use helper");
    }
    if had_previous {
        let _ = fs::remove_dir_all(previous);
    }
    Ok(destination)
}

fn helper_install_matches(source: &Path, destination: &Path) -> anyhow::Result<bool> {
    if !destination.is_dir() {
        return Ok(false);
    }
    let fingerprint = Path::new("Contents/Resources/.waku-helper-fingerprint");
    let source_fingerprint = fs::read(source.join(fingerprint))?;
    let Ok(installed_fingerprint) = fs::read(destination.join(fingerprint)) else {
        return Ok(false);
    };
    Ok(source_fingerprint == installed_fingerprint)
}

fn copy_directory(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    fs::create_dir(destination)?;
    fs::set_permissions(destination, metadata.permissions())?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_directory(&source_path, &destination_path)?;
        } else if file_type.is_symlink() {
            crate::fs_ext::symlink(&fs::read_link(&source_path)?, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path)?;
            fs::set_permissions(
                &destination_path,
                fs::symlink_metadata(&source_path)?.permissions(),
            )?;
        }
    }
    Ok(())
}

pub fn skill_root_path() -> anyhow::Result<PathBuf> {
    let path = resources_directory(&host_executable_path()?, std::env::consts::OS)?.join("skills");
    packaged_file(
        &path.join("waku-computer-use/SKILL.md"),
        "Waku Computer Use skill",
    )?;
    Ok(path)
}

fn host_executable_path() -> anyhow::Result<PathBuf> {
    std::env::var_os(crate::APP_EXECUTABLE_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(|| std::env::current_exe().context("Waku executable path is unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every helper mode is selected by an argument, because a package ships one
    /// executable. Dropping the flag would silently start the GUI instead, which
    /// is why it is pinned here rather than only in a driver's own test.
    #[test]
    fn a_merged_helper_is_reached_through_its_mode_word() {
        // A dedicated helper that exists wins and needs no mode word.
        let directory = std::env::temp_dir().join(format!("waku-helper-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let standalone = directory.join("waku_computer_use");
        fs::write(&standalone, b"").unwrap();
        let dedicated = helper_command(Some(standalone.clone()), "computer-use").unwrap();
        assert_eq!(dedicated.program, standalone);
        assert!(dedicated.args.is_empty());

        // Without one, the same request becomes this executable plus the mode.
        let merged = helper_command(Some(directory.join("absent")), "computer-use").unwrap();
        assert_eq!(merged.program, host_executable_path().unwrap());
        assert_eq!(merged.args, ["--computer-use"]);

        // The JSON form is what string-only consumers pass along.
        assert_eq!(merged.args_json(), r#"["--computer-use"]"#);
        assert_eq!(dedicated.args_json(), "[]");
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn packaged_resources_follow_each_platform_layout() {
        for (executable, os, resources) in [
            (
                "/Applications/Waku.app/Contents/MacOS/Waku",
                "macos",
                "/Applications/Waku.app/Contents/Resources",
            ),
            ("/opt/waku/bin/waku", "linux", "/opt/waku/share/waku"),
            (
                "/dev/waku/target/debug/waku",
                "linux",
                "/dev/waku/target/debug/resources",
            ),
            ("/Waku/waku.exe", "windows", "/Waku/resources"),
        ] {
            assert_eq!(
                resources_directory(Path::new(executable), os).unwrap(),
                PathBuf::from(resources)
            );
        }
    }

    #[test]
    fn app_grants_preserve_bundle_identity() {
        let target = ComputerTarget {
            window_id: 42,
            bundle_id: "net.imput.helium".into(),
            team_id: Some("S4Q33XPHB4".into()),
            app_name: "Helium".into(),
            window_title: "Window".into(),
            width: 1440,
            height: 823,
        };
        let grant = ComputerAppGrant {
            bundle_id: "net.imput.helium".into(),
            app_name: "Helium".into(),
        };
        assert_eq!(target.grant_key(), grant.key());
        assert!(target.persistable());
    }

    #[test]
    fn preview_updates_restore_the_pip_state() {
        let state = decode_preview_update(
            br#"{
                "target": {
                    "windowId": 42,
                    "bundleId": "net.imput.helium",
                    "teamId": "S4Q33XPHB4",
                    "appName": "Helium",
                    "windowTitle": "Window",
                    "width": 1440,
                    "height": 823
                },
                "imageUrl": "data:image/png;base64,aGVsbG8="
            }"#,
        )
        .unwrap();

        assert_eq!(state.target.unwrap().window_id, 42);
        assert_eq!(state.phase, ComputerUsePhase::Running);
        assert!(state.visible);
        assert!(state.image_url.is_some());
    }
}
