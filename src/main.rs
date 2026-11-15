//! The `anchor` binary. All logic lives in the library; see [`anchor::cli`].

fn main() -> std::process::ExitCode {
    anchor::cli::run()
}
