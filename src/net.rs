use std::io::Read;
use std::time::Duration;

use anyhow::{bail, Result};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
pub const READ_TIMEOUT: Duration = Duration::from_secs(300);
pub const RETRY_WAITS: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];

pub trait HttpClient {
    fn get(&self, url: &str) -> Result<Vec<u8>>;
}

/// Büyük dosya indirmeleri için: toplam zaman aşımı yok, yalnızca bağlantı ve okuma boşluğu sınırlı.
pub struct DownloadClient {
    agent: ureq::Agent,
}

impl DownloadClient {
    pub fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(CONNECT_TIMEOUT)
                .timeout_read(READ_TIMEOUT)
                .build(),
        }
    }
}

impl Default for DownloadClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient for DownloadClient {
    fn get(&self, url: &str) -> Result<Vec<u8>> {
        let response = self.agent.get(url).call()?;
        let mut bytes = Vec::new();
        response.into_reader().read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

pub struct Fetcher<'a> {
    pub client: &'a dyn HttpClient,
    pub log: &'a dyn Fn(&str),
    pub sleep: &'a dyn Fn(Duration),
}

impl Fetcher<'_> {
    /// Başlangıç ve bitiş satırını yazar; hata durumunda RETRY_WAITS kadar yeniden dener.
    pub fn get(&self, url: &str) -> Result<Vec<u8>> {
        (self.log)(&format!("İndiriliyor: {url}"));
        let mut attempt = 0;
        loop {
            match self.client.get(url) {
                Ok(bytes) => {
                    (self.log)(&format!("İndirildi: {url} ({} bayt)", bytes.len()));
                    return Ok(bytes);
                }
                Err(error) => {
                    let Some(wait) = RETRY_WAITS.get(attempt).copied() else {
                        bail!("{url} indirilemedi: {error:#}");
                    };
                    (self.log)(&format!(
                        "{url} denemesi başarısız ({error:#}); {} sn sonra yeniden denenecek.",
                        wait.as_secs()
                    ));
                    (self.sleep)(wait);
                    attempt += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct Flaky {
        failures: Cell<usize>,
        calls: RefCell<Vec<String>>,
    }

    impl HttpClient for Flaky {
        fn get(&self, url: &str) -> Result<Vec<u8>> {
            self.calls.borrow_mut().push(url.to_owned());
            if self.failures.get() > 0 {
                self.failures.set(self.failures.get() - 1);
                bail!("timed out reading response");
            }
            Ok(b"veri".to_vec())
        }
    }

    #[test]
    fn retries_with_growing_waits_then_succeeds() {
        let client = Flaky {
            failures: Cell::new(2),
            calls: RefCell::new(Vec::new()),
        };
        let waits = RefCell::new(Vec::new());
        let logs = RefCell::new(Vec::new());
        let log = |text: &str| logs.borrow_mut().push(text.to_owned());
        let sleep = |wait: Duration| waits.borrow_mut().push(wait);
        let fetcher = Fetcher {
            client: &client,
            log: &log,
            sleep: &sleep,
        };
        assert_eq!(fetcher.get("https://x.test/a").unwrap(), b"veri");
        assert_eq!(client.calls.borrow().len(), 3);
        assert_eq!(
            *waits.borrow(),
            vec![Duration::from_secs(2), Duration::from_secs(4)]
        );
        let logs = logs.borrow();
        assert_eq!(logs.first().unwrap(), "İndiriliyor: https://x.test/a");
        assert_eq!(logs.last().unwrap(), "İndirildi: https://x.test/a (4 bayt)");
    }

    #[test]
    fn gives_up_after_three_retries_with_url_in_error() {
        let client = Flaky {
            failures: Cell::new(usize::MAX),
            calls: RefCell::new(Vec::new()),
        };
        let waits = RefCell::new(Vec::new());
        let log = |_: &str| {};
        let sleep = |wait: Duration| waits.borrow_mut().push(wait);
        let fetcher = Fetcher {
            client: &client,
            log: &log,
            sleep: &sleep,
        };
        let error = fetcher.get("https://x.test/b").unwrap_err().to_string();
        assert!(
            error.starts_with("https://x.test/b indirilemedi:"),
            "{error}"
        );
        assert_eq!(client.calls.borrow().len(), 4);
        assert_eq!(
            *waits.borrow(),
            vec![
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8)
            ]
        );
    }
}
