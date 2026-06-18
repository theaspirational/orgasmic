// arch: arch_A53QX.3
pub mod claude;
pub mod codex;
pub mod cursor;
pub mod cursor_acp;
pub mod hermes;
pub mod shell;

pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use cursor::CursorAdapter;
pub use cursor_acp::CursorAcpAdapter;
pub use hermes::HermesAdapter;
pub use shell::ShellAdapter;
