// Release builds are tray-resident GUI apps: without this attribute Windows
// opens a console window whose close button kills the process before the
// graceful shutdown runs. Debug builds keep the console for diagnostics.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if std::env::args().any(|argument| argument == "--input-monitor") {
        local_voice_input_lib::run_input_monitor_worker();
    } else {
        local_voice_input_lib::run();
    }
}
