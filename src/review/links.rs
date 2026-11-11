//! Cursor URL recognition and one nonblocking desktop-opening request.

use regex::Regex;
use std::{
    process::{Command, Stdio},
    sync::{OnceLock, mpsc},
    thread,
};

/// Plain-text URLs must start with HTTP(S), at a token boundary. Quoted text,
/// whitespace and hard newlines terminate a URL. Terminal soft wraps are
/// removed by ReviewDocument before this function is called.
pub(crate) fn plain_url_at(text: &str, offset: usize) -> Option<&str> {
    static URL: OnceLock<Regex> = OnceLock::new();
    let pattern = URL.get_or_init(|| Regex::new(r#"(?i-u:https?)://[^\s<>"'`]+"#).unwrap());
    for found in pattern.find_iter(text) {
        if text[..found.start()]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-' | '/' | '.'))
        {
            continue;
        }
        let mut candidate = found.as_str();
        let mut opens = [0usize; 3];
        let mut closes = [0usize; 3];
        for ch in candidate.chars() {
            match ch {
                '(' => opens[0] += 1,
                '[' => opens[1] += 1,
                '{' => opens[2] += 1,
                ')' => closes[0] += 1,
                ']' => closes[1] += 1,
                '}' => closes[2] += 1,
                _ => {}
            }
        }
        while let Some(last) = candidate.chars().next_back() {
            let closing = match last {
                ')' => Some(0),
                ']' => Some(1),
                '}' => Some(2),
                _ => None,
            };
            if matches!(last, '.' | ',' | ';' | ':' | '!' | '?') {
                candidate = &candidate[..candidate.len() - last.len_utf8()];
            } else if let Some(index) = closing.filter(|index| closes[*index] > opens[*index]) {
                closes[index] -= 1;
                candidate = &candidate[..candidate.len() - last.len_utf8()];
            } else {
                break;
            }
        }
        if offset >= found.start()
            && offset < found.start() + candidate.len()
            && validate_open_url(candidate).is_ok()
        {
            return Some(candidate);
        }
    }
    None
}

fn validate_open_url(url: &str) -> Result<(), String> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err("only HTTP and HTTPS links can be opened".into());
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err("only HTTP and HTTPS links can be opened".into());
    }
    if url.chars().any(|ch| ch.is_control() || ch.is_whitespace())
        || rest.split(['/', '?', '#']).next().is_none_or(str::is_empty)
    {
        return Err("invalid HTTP or HTTPS link".into());
    }
    Ok(())
}

type OpenResult = Result<(), String>;

/// At most one helper is outstanding. Its worker owns and reaps the process,
/// even if Review or App closes. Some Linux desktop helpers remain alive for
/// the browser's lifetime: do not block the reactor or kill that browser on a
/// deadline. While it remains alive, further gx requests report busy.
#[derive(Default)]
pub(crate) struct UrlOpener {
    pending: Option<(mpsc::Receiver<OpenResult>, u128)>,
}

impl UrlOpener {
    pub(crate) fn next_poll_ms(&self) -> Option<u128> {
        self.pending.as_ref().map(|(_, deadline)| *deadline)
    }

    pub(crate) fn open(&mut self, url: &str, now_ms: u128) -> OpenResult {
        self.open_with(url, now_ms, run_desktop_opener)
    }

    pub(crate) fn open_with(
        &mut self,
        url: &str,
        now_ms: u128,
        run: impl FnOnce(String) -> OpenResult + Send + 'static,
    ) -> OpenResult {
        validate_open_url(url)?;
        if self.pending.is_some() {
            return Err("a link opener is already running".into());
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let url = url.to_owned();
        thread::Builder::new()
            .name("lector-url-opener".into())
            .spawn(move || {
                // A closed receiver does not abandon wait/reaping in `run`.
                let result = run(url);
                let _ = sender.send(result);
            })
            .map_err(|error| error.to_string())?;
        self.pending = Some((receiver, now_ms.saturating_add(50)));
        Ok(())
    }

    pub(crate) fn poll(&mut self, now_ms: u128) -> Option<OpenResult> {
        let (receiver, deadline) = self.pending.as_mut()?;
        *deadline = now_ms.saturating_add(50);
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err("link opener stopped unexpectedly".into()),
        };
        self.pending = None;
        Some(result)
    }
}

fn desktop_command(url: &str) -> Command {
    let mut command = Command::new(if cfg!(target_os = "macos") {
        "/usr/bin/open"
    } else {
        "xdg-open"
    });
    // The validated URL starts with http(s), never an option. Pass one exact
    // argument: shell punctuation has no command-interpolation semantics.
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn run_desktop_opener(url: String) -> OpenResult {
    let status = desktop_command(&url)
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("desktop opener returned {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn completed(opener: &mut UrlOpener) -> OpenResult {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = opener.poll(0) {
                return result;
            }
            assert!(Instant::now() < deadline, "injected opener did not finish");
            thread::yield_now();
        }
    }

    #[test]
    fn cursor_must_be_inside_a_url_not_adjacent_punctuation() {
        let text = "See (https://example.test/a_(b)). next https://other.test/x?q=1&v=2!";
        assert_eq!(plain_url_at(text, 8), Some("https://example.test/a_(b)"));
        assert_eq!(plain_url_at(text, text.find(").").unwrap()), None);
        assert_eq!(plain_url_at(text, text.len() - 1), None);
        assert_eq!(
            plain_url_at(text, text.len() - 2),
            Some("https://other.test/x?q=1&v=2")
        );
        assert_eq!(plain_url_at("abchttps://example.test", 8), None);
        assert_eq!(plain_url_at("https://\nexample.test", 2), None);
        assert_eq!(
            plain_url_at("https://one.test\nhttps://two.test", 25),
            Some("https://two.test")
        );
    }

    #[test]
    fn opener_is_injected_bounded_and_reports_failure() {
        let mut opener = UrlOpener::default();
        let (release, wait) = mpsc::channel();
        opener
            .open_with("https://example.test/", 0, move |url| {
                assert_eq!(url, "https://example.test/");
                wait.recv().unwrap();
                Err("modeled desktop failure".into())
            })
            .unwrap();
        assert!(opener.next_poll_ms().is_some());
        assert!(
            opener
                .open_with("https://other.test", 0, |_| panic!("busy request ran"))
                .is_err()
        );
        assert_eq!(opener.next_poll_ms(), Some(50));
        assert!(opener.poll(75).is_none());
        assert_eq!(opener.next_poll_ms(), Some(125));
        release.send(()).unwrap();
        assert_eq!(
            completed(&mut opener),
            Err("modeled desktop failure".into())
        );
        assert!(!opener.next_poll_ms().is_some());
        opener
            .open_with("https://example.test", 0, |_| Ok(()))
            .unwrap();
        assert_eq!(completed(&mut opener), Ok(()));
    }

    #[test]
    fn closing_owner_does_not_abandon_the_running_opener() {
        let (release, wait) = mpsc::channel();
        let (finished, completion) = mpsc::channel();
        let mut opener = UrlOpener::default();
        opener
            .open_with("https://example.test", 0, move |_| {
                wait.recv().unwrap();
                finished.send(()).unwrap();
                Ok(())
            })
            .unwrap();
        drop(opener);
        release.send(()).unwrap();
        completion.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn opening_accepts_only_http_s_and_preserves_argument_bytes() {
        let mut opener = UrlOpener::default();
        for url in [
            "file:///tmp/test",
            "javascript:alert(1)",
            "--help",
            "https://",
            "https://x.test/\n",
        ] {
            assert!(
                opener
                    .open_with(url, 0, |_| panic!("invalid URL ran"))
                    .is_err()
            );
        }
        let url = "https://example.test/$(dummy)?a=1&b=2;test";
        assert_eq!(desktop_command(url).get_args().collect::<Vec<_>>(), [url]);
        assert!(validate_open_url("HTTPS://example.test").is_ok());
    }
}
