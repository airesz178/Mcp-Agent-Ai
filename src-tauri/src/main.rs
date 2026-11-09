// This is a GUI application even in debug/preview builds. Do not allocate a
// second Windows console when launched through the installer or shortcut.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    mcp_agent_ai_native::run();
}
