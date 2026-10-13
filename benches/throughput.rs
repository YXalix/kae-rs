//! `cargo bench` entry point: the exact same orchestrator as the kae-bench
//! CLI; arguments after `--` pass straight through, e.g.
//! `cargo bench -- --ops compress`.

fn main() {
    std::process::exit(kae::orchestrator::main());
}
