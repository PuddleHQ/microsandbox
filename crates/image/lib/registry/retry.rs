//! Retries for registry requests that failed for a transient reason.
//!
//! A registry under load (Docker Hub included) now and then answers a token, manifest or blob
//! request with a 5xx or drops the connection. Docker retries such requests; without a retry, one
//! bad answer fails the whole pull.

use std::{future::Future, time::Duration};

use oci_client::errors::OciDistributionError;

use crate::error::{ImageError, ImageResult};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Pauses before each retry of a request that failed for a transient reason. About 15 s in all,
/// then the last error is returned.
#[cfg(not(test))]
pub(crate) const RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];
#[cfg(test)]
pub(crate) const RETRY_DELAYS: [Duration; 4] = [Duration::from_millis(10); 4];

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Runs `request` and runs it again after each of [`RETRY_DELAYS`] while it fails for a
/// transient reason ([`is_transient`]). Any other result is returned as is. `what` names the
/// request in the retry log line.
pub(crate) async fn with_retries<T, F, Fut>(what: &str, mut request: F) -> ImageResult<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ImageResult<T>>,
{
    let mut delays = RETRY_DELAYS.iter();
    loop {
        match request().await {
            Err(ImageError::Registry(err)) if is_transient(&err) => {
                let Some(delay) = delays.next() else {
                    return Err(ImageError::Registry(err));
                };
                tracing::warn!(
                    request = what,
                    error = %err,
                    retry_in_ms = delay.as_millis(),
                    "registry request failed, retrying"
                );
                tokio::time::sleep(*delay).await;
            }
            other => return other,
        }
    }
}

/// Whether a registry failure may pass if the same request is sent again: a request that never
/// got an answer, a server-side error, a 429 or a refused token request. A 401, 403 or 404 from
/// the registry itself, or a malformed answer, is final.
pub(crate) fn is_transient(err: &OciDistributionError) -> bool {
    match err {
        OciDistributionError::RequestError(_) | OciDistributionError::AuthenticationFailure(_) => {
            true
        }
        OciDistributionError::ServerError { code, .. } => *code >= 500 || *code == 429,
        _ => false,
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use oci_client::errors::OciDistributionError as E;

    use super::{RETRY_DELAYS, is_transient, with_retries};
    use crate::error::ImageError;

    fn server(code: u16) -> E {
        E::ServerError {
            code,
            url: "u".into(),
            message: String::new(),
        }
    }

    #[test]
    fn test_only_transient_registry_failures_are_retried() {
        assert!(is_transient(&E::AuthenticationFailure("x".into())));
        assert!(is_transient(&server(500)));
        assert!(is_transient(&server(503)));
        assert!(is_transient(&server(429)));
        assert!(!is_transient(&server(404)));
        assert!(!is_transient(&E::UnauthorizedError { url: "u".into() }));
        assert!(!is_transient(&E::ManifestParsingError("x".into())));
    }

    #[tokio::test]
    async fn test_a_transient_failure_is_retried_until_it_passes() {
        let calls = AtomicUsize::new(0);
        let got = with_retries("test", || async {
            if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                Err(ImageError::Registry(server(500)))
            } else {
                Ok(7)
            }
        })
        .await
        .unwrap();
        assert_eq!(got, 7);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_a_transient_failure_that_persists_returns_the_last_error() {
        let calls = AtomicUsize::new(0);
        let err = with_retries("test", || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(ImageError::Registry(server(503)))
        })
        .await
        .unwrap_err();
        assert!(
            matches!(err, ImageError::Registry(E::ServerError { code: 503, .. })),
            "{err:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1 + RETRY_DELAYS.len());
    }

    #[tokio::test]
    async fn test_a_final_failure_is_not_retried() {
        let calls = AtomicUsize::new(0);
        let err = with_retries("test", || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(ImageError::Registry(server(404)))
        })
        .await
        .unwrap_err();
        assert!(
            matches!(err, ImageError::Registry(E::ServerError { code: 404, .. })),
            "{err:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
