//! Where each agent harness looks for user-level skills. Every supported
//! harness reads `<skills dir>/<skill name>/SKILL.md`; the harness itself
//! owns where that directory is, so each concrete backend implements
//! [`SkillsHarness`] and [`skills_harnesses`] lists them. The caller supplies
//! the skill contents and decides what to do with an existing file.

use std::path::PathBuf;

use crate::claude_code_backend::ClaudeCodeBackend;
use crate::codex_backend::CodexBackend;
use crate::pi_backend::PiBackend;

/// One harness that can have user-level skills installed.
pub trait SkillsHarness: Sync {
    /// The agent backend name (`claude-code`, `codex`, `pi`).
    fn backend_name(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    /// The directory holding one `<name>/SKILL.md` folder per skill, or `None`
    /// when no home directory can be determined.
    fn skills_dir(&self) -> Option<PathBuf>;
}

static CLAUDE_CODE: ClaudeCodeBackend = ClaudeCodeBackend {
    keep_temporary_files: false,
    program_override: None,
};
static CODEX: CodexBackend = CodexBackend {
    keep_temporary_files: false,
    program_override: None,
};
static PI: PiBackend = PiBackend {
    keep_temporary_files: false,
    program_override: None,
};

/// Every harness skills can be installed for.
#[must_use]
pub fn skills_harnesses() -> Vec<&'static dyn SkillsHarness> {
    vec![&CLAUDE_CODE, &CODEX, &PI]
}
