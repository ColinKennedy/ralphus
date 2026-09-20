//! Shared shapes for configuring an MCP host from its concrete agent backend.

use std::path::PathBuf;

/// An edit an MCP host initializer will make to a local configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpFileEdit {
    pub path: PathBuf,
    pub description: String,
    pub content: String,
    pub mode: McpFileEditMode,
}

/// Whether an edit adds a self-contained fragment or replaces a structured file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpFileEditMode {
    Append,
    Replace,
}

/// A third-party dependency that an MCP host needs before its configuration can work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpThirdPartyInstall {
    pub description: String,
    pub publisher: String,
    pub url: String,
}

/// A host-owned setup command shown in the plan before it is run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSetupCommand {
    pub description: String,
    pub program: String,
    pub args: Vec<String>,
}

/// The complete, reviewable set of local edits required by one MCP host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpInitializationPlan {
    pub host: &'static str,
    pub mcp_program: PathBuf,
    pub third_party_installs: Vec<McpThirdPartyInstall>,
    pub commands: Vec<McpSetupCommand>,
    pub edits: Vec<McpFileEdit>,
}

/// Configuration support supplied by a concrete CLI-agent backend.
pub trait McpInitializer {
    fn mcp_initialization_plan(
        &self,
        profile_path: PathBuf,
    ) -> Result<McpInitializationPlan, String>;

    fn apply_mcp_initialization(&self, plan: &McpInitializationPlan) -> Result<(), String>;
}

/// Finds the bundled MCP executable without relying on a future profile edit
/// taking effect in this process.
pub fn find_mcp_program() -> Result<PathBuf, String> {
    let name = if cfg!(windows) {
        "ralphus-mcp.exe"
    } else {
        "ralphus-mcp"
    };
    let mut candidates = Vec::new();
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            candidates.push(dir.join(name));
        }
    }
    if let Ok(dir) = std::env::current_dir() {
        candidates.push(dir.join("target").join("debug").join(name));
        candidates.push(dir.join("target").join("release").join(name));
        candidates.push(dir.join(name));
    }
    if let Some(path) = std::env::var_os("RALPHUS_MCP_PROGRAM") {
        candidates.insert(0, PathBuf::from(path));
    }
    if let Some(path) = crate::shellcmd::find_program("ralphus-mcp") {
        candidates.insert(0, path.into());
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            format!(
                "could not find {name}; build or install ralphus-mcp first, or set RALPHUS_MCP_PROGRAM to its full path"
            )
        })
}

/// Appends the selected shell's idempotent PATH edit.
pub fn profile_path_edit(profile_path: PathBuf, directory: &std::path::Path) -> McpFileEdit {
    let directory = directory.display().to_string();
    let content = match profile_path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("ps1") => {
            format!("\n# Added by ralphus mcp initialize\n$env:PATH += \";{directory}\"\n")
        }
        Some("fish") => {
            format!("\n# Added by ralphus mcp initialize\nfish_add_path {directory:?}\n")
        }
        Some("cmd") | Some("bat") => format!(
            "\r\nREM Added by ralphus mcp initialize\r\nset \"PATH=%PATH%;{directory}\"\r\n"
        ),
        _ => format!("\n# Added by ralphus mcp initialize\nexport PATH=\"$PATH:{directory}\"\n"),
    };
    McpFileEdit {
        path: profile_path,
        description: format!("add {} to PATH", directory),
        content,
        mode: McpFileEditMode::Append,
    }
}

/// Writes each planned append, creating its parent directory when necessary.
pub fn apply_file_edits(edits: &[McpFileEdit]) -> Result<(), String> {
    for edit in edits {
        if let Some(parent) = edit.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(matches!(edit.mode, McpFileEditMode::Append))
            .truncate(matches!(edit.mode, McpFileEditMode::Replace))
            .open(&edit.path)
            .map_err(|error| format!("could not open {}: {error}", edit.path.display()))?;
        file.write_all(edit.content.as_bytes())
            .map_err(|error| format!("could not write {}: {error}", edit.path.display()))?;
    }
    Ok(())
}
