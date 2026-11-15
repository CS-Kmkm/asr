//! Offline correction evaluation through the app's correction path.
//!
//! Usage (normally via `uv run scripts/eval_correction.py run --provider app`):
//! `cargo run --example correction_eval -- --dataset <cases.json> --out <rows.jsonl> [--settings <patch.json>] [--case <id>]...`
fn main() {
    std::process::exit(local_voice_input_lib::run_correction_eval_cli(
        std::env::args().skip(1).collect(),
    ));
}
