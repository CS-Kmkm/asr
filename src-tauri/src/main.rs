fn main() {
    if std::env::args().any(|argument| argument == "--input-monitor") {
        local_voice_input_lib::run_input_monitor_worker();
    } else {
        local_voice_input_lib::run();
    }
}
