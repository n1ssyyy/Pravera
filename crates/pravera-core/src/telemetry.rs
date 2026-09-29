use std::time::Duration;

/// Where the milliseconds went for a single frame, measured end to end.
///
/// Every stage records into this so the UI can show a breakdown rather than one
/// opaque number. When a session feels bad, this says which stage to blame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LatencyBreakdown {
    pub capture: Duration,
    pub encode: Duration,
    pub network: Duration,
    pub decode: Duration,
    pub present: Duration,
}

impl LatencyBreakdown {
    pub fn total(&self) -> Duration {
        self.capture + self.encode + self.network + self.decode + self.present
    }

    /// The stage costing the most, for the UI to highlight.
    pub fn dominant_stage(&self) -> (&'static str, Duration) {
        [
            ("capture", self.capture),
            ("encode", self.encode),
            ("network", self.network),
            ("decode", self.decode),
            ("present", self.present),
        ]
        .into_iter()
        .max_by_key(|(_, d)| *d)
        .unwrap_or(("capture", Duration::ZERO))
    }

    /// Around 20 ms is the threshold below which streaming becomes hard to
    /// distinguish from working locally.
    pub fn feels_local(&self) -> bool {
        self.total() <= Duration::from_millis(20)
    }
}

/// Installs the process-wide tracing subscriber. Honours RUST_LOG, defaulting
/// to the supplied filter so a noisy dependency cannot flood the log.
pub fn init(default_filter: &str) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter(default_filter))
        .with_target(true)
        .with_ansi(true)
        .try_init();
}

/// The same, writing to a file instead of the terminal.
///
/// A windowed build has no console attached, so without this every line the
/// app records is thrown away — including the one saying why a connection
/// failed, which is the only line anyone wants. Returns the path actually
/// being written to, or `None` if the file could not be opened and the log
/// went to standard output instead.
///
/// The previous run is kept alongside as `<name>.1`. One rotation, because a
/// log that grows without bound is a log nobody dares open, and a log that is
/// truncated on start loses exactly the run that crashed.
pub fn init_to_file(default_filter: &str, path: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::fs;

    let opened = (|| -> std::io::Result<fs::File> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // A failed rename is not a reason to give up the log: the common cause
        // is a second copy of the app already holding the previous file open.
        let _ = fs::rename(path, path.with_extension("log.1"));
        fs::File::create(path)
    })();

    match opened {
        Ok(file) => {
            let res = tracing_subscriber::fmt()
                .with_env_filter(filter(default_filter))
                .with_target(true)
                // No escape codes: this is read in a text editor, and a file
                // full of `\u{1b}[2m` is worse than no colour.
                .with_ansi(false)
                .with_writer(std::sync::Arc::new(file))
                .try_init();
            if res.is_err() {
                // `try_init` fails when a global subscriber is already set.
                // Callers run this before the single-instance check, so this
                // is a second instance about to `exit(7)`: it already renamed
                // the live log aside and truncated a fresh file. Remove the
                // truncation and report `None` so the caller knows logging
                // went nowhere — returning `Some` here made the caller believe
                // "logging to file" while nothing would ever be written.
                let _ = std::fs::remove_file(path);
                return None;
            }
            Some(path.to_path_buf())
        }
        Err(error) => {
            init(default_filter);
            tracing::warn!(
                path = %path.display(),
                %error,
                "could not open a log file; logging to standard output"
            );
            None
        }
    }
}

fn filter(default_filter: &str) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn total_sums_every_stage() {
        let b = LatencyBreakdown {
            capture: ms(1),
            encode: ms(2),
            network: ms(3),
            decode: ms(4),
            present: ms(5),
        };
        assert_eq!(b.total(), ms(15));
    }

    #[test]
    fn dominant_stage_finds_the_worst_offender() {
        let b = LatencyBreakdown {
            capture: ms(1),
            encode: ms(9),
            network: ms(3),
            decode: ms(2),
            present: ms(1),
        };
        assert_eq!(b.dominant_stage().0, "encode");
    }

    #[test]
    fn twenty_milliseconds_is_the_local_threshold() {
        let good = LatencyBreakdown {
            capture: ms(4),
            encode: ms(4),
            network: ms(4),
            decode: ms(4),
            present: ms(4),
        };
        assert!(good.feels_local());

        let bad = LatencyBreakdown {
            network: ms(50),
            ..Default::default()
        };
        assert!(!bad.feels_local());
    }
}

/// Make a peer-supplied string safe to put in a log line.
///
/// Usernames, host names, client names and goodbye reasons are all chosen by
/// whoever is at the other end. Untreated, a newline in one forges a second log
/// entry, an ANSI escape rewrites the operator's terminal, and a megabyte-long
/// value fills the disk. None of those is exotic; all three are cheap to send.
///
/// This lives in `pravera-core` rather than beside either end of a session
/// because both ends need it: a host logs what a client called itself, and a
/// client logs what a host called itself, and neither may trust the other.
pub fn for_log(text: &str) -> String {
    const MAX: usize = 64;
    let mut out: String = text.chars().filter(|c| !c.is_control()).take(MAX).collect();
    if text.chars().count() > MAX {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod log_tests {
    use super::for_log;

    #[test]
    fn a_forged_log_entry_cannot_be_smuggled_through_a_peer_supplied_name() {
        // Everything after the newline would otherwise appear as its own entry,
        // at whatever severity the attacker chose.
        let forged = "operator\n2026-01-01 ERROR host: intrusion detected";
        let safe = for_log(forged);

        assert!(!safe.contains('\n'));
        assert!(safe.starts_with("operator"));
    }

    #[test]
    fn an_escape_sequence_cannot_rewrite_the_operators_terminal() {
        let safe = for_log("normal\u{1b}[2J\u{1b}[H");
        assert!(!safe.contains('\u{1b}'));
    }

    #[test]
    fn a_very_long_name_is_truncated_rather_than_logged_whole() {
        let safe = for_log(&"a".repeat(100_000));
        assert!(safe.chars().count() <= 67, "{}", safe.chars().count());
        assert!(safe.ends_with("..."));
    }

    #[test]
    fn an_ordinary_name_is_left_exactly_as_it_is() {
        // Including non-ASCII: truncation counts characters, not bytes, so a
        // name in another script must not be cut mid-character.
        assert_eq!(for_log("Ünïcödé Machine"), "Ünïcödé Machine");
        assert_eq!(for_log(""), "");
    }
}
