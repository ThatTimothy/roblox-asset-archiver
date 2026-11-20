use std::{fmt::Write, ops::Deref};

use env_logger::Logger;
use indicatif::{MultiProgress, ProgressBar, ProgressState, ProgressStyle};
use indicatif_log_bridge::LogWrapper;

/// Wrapper of [MultiProgress] that contains utility methods & styles
pub struct Progress {
    pub multi: MultiProgress,
}

impl Progress {
    pub fn new(logger: Logger) -> Self {
        let multi = MultiProgress::new();
        // Wrap logs so indicatif doesn't conflict - https://docs.rs/indicatif-log-bridge/latest/indicatif_log_bridge/
        let level = logger.filter();
        LogWrapper::new(multi.clone(), logger).try_init().unwrap();
        log::set_max_level(level);

        Self { multi }
    }

    /// Prints to stderr without conflicting with progress bar.
    /// Cannot just use println as that disappears when progress bar is hidden.
    pub fn print(&self, message: impl AsRef<str>) {
        // println does not output if bar is hidden (when outputting to non-tty like file)
        // suspend causes a re-draw each time which results in flickering
        // so, use stderr for non-tty and builtin println otherwise
        if self.is_hidden() {
            eprintln!("{}", message.as_ref());
        } else {
            self.println(message.as_ref()).expect("Failed to print");
        }
    }

    /// Adds a basic spinner
    pub fn add_spinner(&self, primary: bool) -> ProgressBar {
        self.add(ProgressBar::new_spinner().with_style(Progress::spinner_style(primary)))
    }

    /// Adds a basic bar
    pub fn add_bar(&self, len: u64, primary: bool, bytes: bool) -> ProgressBar {
        self.add(ProgressBar::new(len).with_style(Progress::bar_style(primary, bytes)))
    }

    /// Shared function for determining spinner element
    fn spinner_element(primary: bool) -> &'static str {
        if primary {
            "{spinner:.blue}"
        } else {
            "{spinner:.green}"
        }
    }

    /// The basic spinner style
    pub fn spinner_style(primary: bool) -> ProgressStyle {
        let template = Self::spinner_element(primary).to_owned() + " {msg}";
        ProgressStyle::with_template(&template).unwrap()
    }

    /// The spinner for a download style
    pub fn spinner_download_style(primary: bool) -> ProgressStyle {
        let template = Self::spinner_element(primary).to_owned()
            + " {bytes} downloaded ({bytes_per_sec}) | {msg}";
        ProgressStyle::with_template(&template).unwrap()
    }

    /// The basic bar style
    pub fn bar_style(primary: bool, bytes: bool) -> ProgressStyle {
        let template = "[{elapsed_precise}]".to_owned()
            + if primary {
                " [{bar:30.cyan/blue}]"
            } else {
                " [{bar:30.green}]"
            }
            + if bytes {
                " {bytes:>11} / {total_bytes:>11} at {bytes_per_sec:>13}"
            } else {
                " {pos}/{len} at {per_sec}"
            }
            + " | {msg}";
        let mut style = ProgressStyle::with_template(&template).unwrap();
        if !bytes {
            style = style.with_key(
                "per_sec",
                move |state: &ProgressState, w: &mut dyn Write| {
                    write!(w, "{:.1}/s", state.per_sec()).unwrap();
                },
            )
        }
        style
    }
}

/// Send message to [log::debug!] and bar's [ProgressBar::set_message]
macro_rules! message {
    ($bar:expr, $($arg:tt)+) => {{
        let msg = format!($($arg)+);
        log::debug!("{msg}");
        $bar.set_message(msg);
    }};
}
pub(crate) use message;

// Support direct operations on multi
impl Deref for Progress {
    type Target = MultiProgress;
    fn deref(&self) -> &MultiProgress {
        &self.multi
    }
}
