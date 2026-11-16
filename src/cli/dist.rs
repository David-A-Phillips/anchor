//! Packaging helpers: shell completions and the man page, generated from the clap definition so
//! distribution packages never ship stale copies.

use clap::{Command, CommandFactory, ValueEnum};
use clap_complete::Shell;

use crate::base::{Error, Result};

/// Shells that `anchor completions` can target.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
}

impl From<CompletionShell> for Shell {
    fn from(shell: CompletionShell) -> Self {
        match shell {
            CompletionShell::Bash => Shell::Bash,
            CompletionShell::Zsh => Shell::Zsh,
            CompletionShell::Fish => Shell::Fish,
            CompletionShell::Elvish => Shell::Elvish,
            CompletionShell::Powershell => Shell::PowerShell,
        }
    }
}

/// Print the completion script for `shell` to stdout.
pub fn completions<C: CommandFactory>(shell: CompletionShell) {
    let mut command: Command = C::command();
    clap_complete::generate(
        Shell::from(shell),
        &mut command,
        "anchor",
        &mut std::io::stdout(),
    );
}

/// Print the roff man page to stdout.
pub fn man<C: CommandFactory>() -> Result<()> {
    let command: Command = C::command();
    clap_mangen::Man::new(command)
        .render(&mut std::io::stdout())
        .map_err(|err| Error::invalid(format!("cannot render the man page: {err}")))
}
