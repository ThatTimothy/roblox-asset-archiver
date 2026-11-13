use std::ops::Deref;

use env_logger::Logger;
use indicatif::MultiProgress;
use indicatif_log_bridge::LogWrapper;

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

    /// Prints to stderr without conflicting with progress bar
    /// Cannot just use println as that disappears when progress bar is hidden
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
