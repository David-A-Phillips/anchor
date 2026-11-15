//! The only door through which the wizard talks to the user.
//!
//! [`Prompter`] has three implementations: [`InquirePrompter`] (a real terminal, with `inquire`
//! prompts and `indicatif` progress bars), [`ScriptedPrompter`] (a queue of answers that records
//! everything asked and shown, for tests) and [`DefaultsPrompter`] (accepts every default, for
//! `--yes`).

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

/// Why a prompt produced no answer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PromptError {
    /// The user cancelled (Esc or Ctrl+C).
    #[error("cancelled")]
    Cancelled,
    /// Non-interactive mode has no default for this question.
    #[error(
        "non-interactive mode needs an answer for \"{0}\" (pass it as an option or run without --yes)"
    )]
    NoDefault(String),
    /// The terminal failed, or there is none.
    #[error("terminal error: {0}")]
    Terminal(String),
    /// A scripted prompter ran out of answers or got one of the wrong type.
    #[error("script error: {0}")]
    Script(String),
}

/// Result of one prompt.
pub type PromptResult<T> = Result<T, PromptError>;

/// One option of a [`Prompter::select`] or [`Prompter::multi_select`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    /// Short label.
    pub label: String,
    /// One-line explanation of what choosing it means.
    pub help: Option<String>,
}

impl Choice {
    /// A choice with only a label.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            help: None,
        }
    }

    /// A choice with an explanation.
    pub fn with_help(label: impl Into<String>, help: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            help: Some(help.into()),
        }
    }
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.help {
            Some(help) => write!(f, "{} - {help}", self.label),
            None => f.write_str(&self.label),
        }
    }
}

/// Progress of a long operation, as the wizard reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// A phase started or a status line changed.
    Step(String),
    /// Bytes transferred of a named transfer.
    Bytes {
        /// What is being transferred.
        label: String,
        /// Bytes done.
        done: u64,
        /// Bytes in total, when known.
        total: Option<u64>,
    },
    /// The current transfer or phase is over.
    Done,
}

/// Every interaction of the wizard.
pub trait Prompter: Send + Sync {
    /// Free text; `default` is pre-filled and returned on an empty answer.
    fn text(&self, message: &str, default: Option<&str>) -> PromptResult<String>;
    /// Yes or no.
    fn confirm(&self, message: &str, default: bool) -> PromptResult<bool>;
    /// One of `choices`; returns its index. `default` is the preselected index.
    fn select(&self, message: &str, choices: &[Choice], default: usize) -> PromptResult<usize>;
    /// Any subset of `choices`; returns the indices. `defaults` are preselected.
    fn multi_select(
        &self,
        message: &str,
        choices: &[Choice],
        defaults: &[usize],
    ) -> PromptResult<Vec<usize>>;
    /// A value that must not be echoed. The wizard never stores API keys; this exists for
    /// callers that need a one-off secret.
    fn secret(&self, message: &str) -> PromptResult<String>;
    /// A filesystem path (asked as text, so every implementation shares the scripted flow).
    fn path(&self, message: &str, default: Option<&Path>) -> PromptResult<PathBuf> {
        let default = default.map(|path| path.to_string_lossy().into_owned());
        self.text(message, default.as_deref()).map(PathBuf::from)
    }
    /// Informational line.
    fn note(&self, text: &str);
    /// Warning line.
    fn warn(&self, text: &str);
    /// Progress of a long operation.
    fn progress(&self, progress: Progress);
}

// ---------------------------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------------------------

/// Prompts in the terminal with `inquire`, progress with `indicatif`.
#[derive(Default)]
pub struct InquirePrompter {
    bar: Mutex<Option<(String, indicatif::ProgressBar)>>,
}

impl InquirePrompter {
    /// A prompter on the process's terminal.
    pub fn new() -> Self {
        Self::default()
    }

    fn clear_bar(&self) {
        if let Some((_, bar)) = self.bar.lock().take() {
            bar.finish_and_clear();
        }
    }
}

fn map_inquire(err: inquire::InquireError) -> PromptError {
    match err {
        inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted => {
            PromptError::Cancelled
        }
        inquire::InquireError::NotTTY => PromptError::Terminal(
            "standard input is not a terminal; run `anchor setup --yes` for a non-interactive setup"
                .to_owned(),
        ),
        other => PromptError::Terminal(other.to_string()),
    }
}

/// A choice together with its position, so duplicate labels stay distinguishable.
struct Indexed<'a>(usize, &'a Choice);

impl fmt::Display for Indexed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.1.fmt(f)
    }
}

fn indexed(choices: &[Choice]) -> Vec<Indexed<'_>> {
    choices
        .iter()
        .enumerate()
        .map(|(i, c)| Indexed(i, c))
        .collect()
}

impl Prompter for InquirePrompter {
    fn text(&self, message: &str, default: Option<&str>) -> PromptResult<String> {
        self.clear_bar();
        let mut prompt = inquire::Text::new(message);
        if let Some(default) = default.filter(|d| !d.is_empty()) {
            prompt = prompt.with_default(default);
        }
        prompt.prompt().map_err(map_inquire)
    }

    fn confirm(&self, message: &str, default: bool) -> PromptResult<bool> {
        self.clear_bar();
        inquire::Confirm::new(message)
            .with_default(default)
            .prompt()
            .map_err(map_inquire)
    }

    fn select(&self, message: &str, choices: &[Choice], default: usize) -> PromptResult<usize> {
        self.clear_bar();
        inquire::Select::new(message, indexed(choices))
            .with_starting_cursor(default.min(choices.len().saturating_sub(1)))
            .raw_prompt()
            .map(|picked| picked.value.0)
            .map_err(map_inquire)
    }

    fn multi_select(
        &self,
        message: &str,
        choices: &[Choice],
        defaults: &[usize],
    ) -> PromptResult<Vec<usize>> {
        self.clear_bar();
        inquire::MultiSelect::new(message, indexed(choices))
            .with_default(defaults)
            .raw_prompt()
            .map(|picked| picked.into_iter().map(|p| p.value.0).collect())
            .map_err(map_inquire)
    }

    fn secret(&self, message: &str) -> PromptResult<String> {
        self.clear_bar();
        inquire::Password::new(message)
            .without_confirmation()
            .with_display_mode(inquire::PasswordDisplayMode::Masked)
            .prompt()
            .map_err(map_inquire)
    }

    fn note(&self, text: &str) {
        self.clear_bar();
        println!("{text}");
    }

    fn warn(&self, text: &str) {
        self.clear_bar();
        eprintln!("warning: {text}");
    }

    fn progress(&self, progress: Progress) {
        let mut slot = self.bar.lock();
        match progress {
            Progress::Step(text) => {
                if let Some((_, bar)) = slot.take() {
                    bar.finish_and_clear();
                }
                println!("  {text}");
            }
            Progress::Bytes { label, done, total } => {
                if slot.as_ref().is_none_or(|(current, _)| *current != label) {
                    if let Some((_, bar)) = slot.take() {
                        bar.finish_and_clear();
                    }
                    let bar = match total.filter(|t| *t > 0) {
                        Some(total) => {
                            let bar = indicatif::ProgressBar::new(total);
                            if let Ok(style) = indicatif::ProgressStyle::with_template(
                                "  {msg:<28} [{bar:30}] {bytes}/{total_bytes} ({eta})",
                            ) {
                                bar.set_style(style.progress_chars("=> "));
                            }
                            bar
                        }
                        None => {
                            let bar = indicatif::ProgressBar::new_spinner();
                            bar.enable_steady_tick(std::time::Duration::from_millis(200));
                            bar
                        }
                    };
                    bar.set_message(label.clone());
                    *slot = Some((label, bar));
                }
                if let Some((_, bar)) = slot.as_ref() {
                    if let Some(total) = total.filter(|t| *t > 0) {
                        bar.set_length(total);
                    }
                    bar.set_position(done);
                }
            }
            Progress::Done => {
                if let Some((_, bar)) = slot.take() {
                    bar.finish_and_clear();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Scripted
// ---------------------------------------------------------------------------------------------

/// One scripted answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Text (also paths and secrets).
    Text(String),
    /// Yes or no.
    Confirm(bool),
    /// Index of the chosen option.
    Select(usize),
    /// Chosen option, found by a substring of its label.
    Pick(String),
    /// Indices chosen in a multi-select.
    Multi(Vec<usize>),
    /// Options chosen in a multi-select, found by label substrings.
    Picks(Vec<String>),
    /// Accept the default of whatever is asked.
    Default,
    /// Cancel at this prompt.
    Cancel,
}

impl Answer {
    /// `Answer::Text`.
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    /// `Answer::Pick`.
    pub fn pick(label: impl Into<String>) -> Self {
        Self::Pick(label.into())
    }

    /// `Answer::Picks`.
    pub fn picks<S: Into<String>>(labels: impl IntoIterator<Item = S>) -> Self {
        Self::Picks(labels.into_iter().map(Into::into).collect())
    }
}

/// The default a prompt offered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptDefault {
    /// Pre-filled text.
    Text(Option<String>),
    /// Default of a confirmation.
    Bool(bool),
    /// Preselected option.
    Index(usize),
    /// Preselected options.
    Indices(Vec<usize>),
    /// A secret has no default.
    None,
}

/// One question a [`ScriptedPrompter`] was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    /// The question text.
    pub message: String,
    /// The options offered (empty for text and confirmations).
    pub choices: Vec<Choice>,
    /// The default offered.
    pub default: PromptDefault,
}

impl Asked {
    /// Label of the preselected option of a select prompt.
    pub fn default_label(&self) -> Option<&str> {
        match self.default {
            PromptDefault::Index(i) => self.choices.get(i).map(|c| c.label.as_str()),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Script {
    answers: VecDeque<Answer>,
    asked: Vec<Asked>,
    notes: Vec<String>,
    warnings: Vec<String>,
    progress: Vec<Progress>,
}

/// Answers from a queue and records everything asked and shown.
#[derive(Default)]
pub struct ScriptedPrompter {
    script: Mutex<Script>,
}

impl ScriptedPrompter {
    /// A prompter that will give `answers` in order.
    pub fn new(answers: impl IntoIterator<Item = Answer>) -> Self {
        Self {
            script: Mutex::new(Script {
                answers: answers.into_iter().collect(),
                ..Script::default()
            }),
        }
    }

    /// Queues one more answer.
    pub fn push(&self, answer: Answer) {
        self.script.lock().answers.push_back(answer);
    }

    /// Every question asked so far.
    pub fn asked(&self) -> Vec<Asked> {
        self.script.lock().asked.clone()
    }

    /// Notes shown so far.
    pub fn notes(&self) -> Vec<String> {
        self.script.lock().notes.clone()
    }

    /// Warnings shown so far.
    pub fn warnings(&self) -> Vec<String> {
        self.script.lock().warnings.clone()
    }

    /// Progress reported so far.
    pub fn progress_events(&self) -> Vec<Progress> {
        self.script.lock().progress.clone()
    }

    /// Answers not consumed yet.
    pub fn remaining(&self) -> usize {
        self.script.lock().answers.len()
    }

    /// `true` when any note or warning contains `needle`.
    pub fn shown(&self, needle: &str) -> bool {
        let script = self.script.lock();
        script
            .notes
            .iter()
            .chain(script.warnings.iter())
            .any(|line| line.contains(needle))
    }

    fn next(&self, asked: Asked) -> PromptResult<Answer> {
        let mut script = self.script.lock();
        let message = asked.message.clone();
        script.asked.push(asked);
        script.answers.pop_front().ok_or_else(|| {
            PromptError::Script(format!("no scripted answer left for \"{message}\""))
        })
    }
}

fn wrong(message: &str, answer: &Answer) -> PromptError {
    PromptError::Script(format!(
        "answer {answer:?} does not fit the question \"{message}\""
    ))
}

fn find_label(message: &str, choices: &[Choice], needle: &str) -> PromptResult<usize> {
    choices
        .iter()
        .position(|c| c.label.contains(needle))
        .ok_or_else(|| {
            PromptError::Script(format!(
                "no option containing \"{needle}\" in \"{message}\": {:?}",
                choices.iter().map(|c| c.label.as_str()).collect::<Vec<_>>()
            ))
        })
}

impl Prompter for ScriptedPrompter {
    fn text(&self, message: &str, default: Option<&str>) -> PromptResult<String> {
        let answer = self.next(Asked {
            message: message.to_owned(),
            choices: Vec::new(),
            default: PromptDefault::Text(default.map(str::to_owned)),
        })?;
        match answer {
            Answer::Text(text) if text.is_empty() => Ok(default.unwrap_or_default().to_owned()),
            Answer::Text(text) => Ok(text),
            Answer::Default => default
                .map(str::to_owned)
                .ok_or_else(|| PromptError::NoDefault(message.to_owned())),
            Answer::Cancel => Err(PromptError::Cancelled),
            other => Err(wrong(message, &other)),
        }
    }

    fn confirm(&self, message: &str, default: bool) -> PromptResult<bool> {
        match self.next(Asked {
            message: message.to_owned(),
            choices: Vec::new(),
            default: PromptDefault::Bool(default),
        })? {
            Answer::Confirm(value) => Ok(value),
            Answer::Default => Ok(default),
            Answer::Cancel => Err(PromptError::Cancelled),
            other => Err(wrong(message, &other)),
        }
    }

    fn select(&self, message: &str, choices: &[Choice], default: usize) -> PromptResult<usize> {
        let answer = self.next(Asked {
            message: message.to_owned(),
            choices: choices.to_vec(),
            default: PromptDefault::Index(default),
        })?;
        let index = match answer {
            Answer::Select(index) => index,
            Answer::Pick(needle) => find_label(message, choices, &needle)?,
            Answer::Default => default,
            Answer::Cancel => return Err(PromptError::Cancelled),
            other => return Err(wrong(message, &other)),
        };
        if index < choices.len() {
            Ok(index)
        } else {
            Err(PromptError::Script(format!(
                "option {index} is out of range in \"{message}\""
            )))
        }
    }

    fn multi_select(
        &self,
        message: &str,
        choices: &[Choice],
        defaults: &[usize],
    ) -> PromptResult<Vec<usize>> {
        let answer = self.next(Asked {
            message: message.to_owned(),
            choices: choices.to_vec(),
            default: PromptDefault::Indices(defaults.to_vec()),
        })?;
        let indices = match answer {
            Answer::Multi(indices) => indices,
            Answer::Picks(needles) => needles
                .iter()
                .map(|needle| find_label(message, choices, needle))
                .collect::<PromptResult<Vec<_>>>()?,
            Answer::Default => defaults.to_vec(),
            Answer::Cancel => return Err(PromptError::Cancelled),
            other => return Err(wrong(message, &other)),
        };
        if indices.iter().all(|i| *i < choices.len()) {
            Ok(indices)
        } else {
            Err(PromptError::Script(format!(
                "an option is out of range in \"{message}\""
            )))
        }
    }

    fn secret(&self, message: &str) -> PromptResult<String> {
        match self.next(Asked {
            message: message.to_owned(),
            choices: Vec::new(),
            default: PromptDefault::None,
        })? {
            Answer::Text(text) => Ok(text),
            Answer::Cancel => Err(PromptError::Cancelled),
            other => Err(wrong(message, &other)),
        }
    }

    fn note(&self, text: &str) {
        self.script.lock().notes.push(text.to_owned());
    }

    fn warn(&self, text: &str) {
        self.script.lock().warnings.push(text.to_owned());
    }

    fn progress(&self, progress: Progress) {
        self.script.lock().progress.push(progress);
    }
}

// ---------------------------------------------------------------------------------------------
// Non-interactive
// ---------------------------------------------------------------------------------------------

/// Accepts the default of every question and forwards output to an inner prompter. A question
/// without a default, or one that keeps being asked because its default is rejected, is an error
/// rather than an endless loop.
pub struct DefaultsPrompter<'a> {
    output: &'a dyn Prompter,
    asked: Mutex<BTreeMap<String, usize>>,
}

/// How often the same question may be asked before the defaults are declared unusable.
const MAX_REPEATS: usize = 3;

impl<'a> DefaultsPrompter<'a> {
    /// Answers by default; notes, warnings and progress go to `output`.
    pub fn new(output: &'a dyn Prompter) -> Self {
        Self {
            output,
            asked: Mutex::new(BTreeMap::new()),
        }
    }

    fn count(&self, message: &str) -> PromptResult<()> {
        let mut asked = self.asked.lock();
        let seen = asked.entry(message.to_owned()).or_insert(0);
        *seen += 1;
        if *seen > MAX_REPEATS {
            return Err(PromptError::NoDefault(message.to_owned()));
        }
        Ok(())
    }
}

impl Prompter for DefaultsPrompter<'_> {
    fn text(&self, message: &str, default: Option<&str>) -> PromptResult<String> {
        self.count(message)?;
        default
            .map(str::to_owned)
            .ok_or_else(|| PromptError::NoDefault(message.to_owned()))
    }

    fn confirm(&self, message: &str, default: bool) -> PromptResult<bool> {
        self.count(message)?;
        Ok(default)
    }

    fn select(&self, message: &str, _choices: &[Choice], default: usize) -> PromptResult<usize> {
        self.count(message)?;
        Ok(default)
    }

    fn multi_select(
        &self,
        message: &str,
        _choices: &[Choice],
        defaults: &[usize],
    ) -> PromptResult<Vec<usize>> {
        self.count(message)?;
        Ok(defaults.to_vec())
    }

    fn secret(&self, message: &str) -> PromptResult<String> {
        Err(PromptError::NoDefault(message.to_owned()))
    }

    fn note(&self, text: &str) {
        self.output.note(text);
    }

    fn warn(&self, text: &str) {
        self.output.warn(text);
    }

    fn progress(&self, progress: Progress) {
        self.output.progress(progress);
    }
}
