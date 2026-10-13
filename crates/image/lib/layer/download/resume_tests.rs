//! A layer body that breaks off after the stream opened resumes inside the same download.
//!
//! The registry here speaks only HTTP/2 over TLS and cancels a layer's stream with `RST_STREAM`
//! (`CANCEL`) part way through, as a CDN now and then does; the client reports that as "stream
//! error received: stream no longer needed" in the middle of the body.

use std::{
    future::poll_fn,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use h2::{Reason, SendStream, server::SendResponse};
use sha2::{Digest as _, Sha256};
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use super::Layer;
use crate::{error::ImageError, registry::retry::RETRY_DELAYS};

/// Size of the layer: well past the cut, so the cut falls inside it.
const BLOB_BYTES: usize = 600 * 1024;

/// Bytes of a cut download sent before the stream is cancelled.
const CUT_AFTER: usize = 200 * 1024;

/// How the registry treats the layer's downloads.
struct Behaviour {
    /// Downloads to cut before one is served whole (`usize::MAX`: every one).
    cuts: usize,
    /// Bytes sent before a cut.
    cut_after: usize,
    /// Answer a `Range` request with the whole blob and a 200, as a registry without range
    /// support does.
    ignore_range: bool,
}

struct Registry {
    blob: Vec<u8>,
    behaviour: Behaviour,
    cut_count: AtomicUsize,
    /// The `Range` header of each request for the blob.
    requests: Mutex<Vec<Option<String>>>,
}

impl Registry {
    fn requests(&self) -> Vec<Option<String>> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn blob() -> Vec<u8> {
    #[expect(clippy::cast_possible_truncation, reason = "a repeating byte pattern")]
    (0..BLOB_BYTES)
        .map(|i| (i.wrapping_mul(31) >> 3) as u8)
        .collect()
}

/// Sends `data` on `stream` as flow control allows.
async fn send_all(stream: &mut SendStream<Bytes>, mut data: Bytes) {
    while !data.is_empty() {
        stream.reserve_capacity(data.len());
        let granted = poll_fn(|cx| stream.poll_capacity(cx))
            .await
            .unwrap()
            .unwrap();
        let chunk = data.split_to(granted.min(data.len()));
        stream.send_data(chunk, false).unwrap();
    }
}

async fn answer(
    registry: Arc<Registry>,
    request: http::Request<h2::RecvStream>,
    mut respond: SendResponse<Bytes>,
) {
    let range = request
        .headers()
        .get("range")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    registry
        .requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(range.clone());
    let start = range
        .as_deref()
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.split('-').next())
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|_| !registry.behaviour.ignore_range);
    let total = registry.blob.len();
    let body = Bytes::from(registry.blob[start.unwrap_or(0)..].to_vec());
    let mut response = http::Response::builder()
        .status(if start.is_some() { 206 } else { 200 })
        .header("content-length", body.len());
    if let Some(start) = start {
        response = response.header(
            "content-range",
            format!("bytes {start}-{}/{total}", total - 1),
        );
    }
    let mut out = respond
        .send_response(response.body(()).unwrap(), false)
        .unwrap();
    if registry.cut_count.load(Ordering::SeqCst) < registry.behaviour.cuts {
        registry.cut_count.fetch_add(1, Ordering::SeqCst);
        send_all(
            &mut out,
            body.slice(..registry.behaviour.cut_after.min(body.len())),
        )
        .await;
        // The client reads what was sent before the stream goes away.
        tokio::time::sleep(Duration::from_millis(200)).await;
        out.send_reset(Reason::CANCEL);
    } else {
        send_all(&mut out, body).await;
        out.send_data(Bytes::new(), true).unwrap();
    }
}

/// Serves the registry on a local port over TLS, HTTP/2 only. Returns `localhost:<port>` and the
/// root certificate (PEM) the client has to trust.
async fn serve(registry: Arc<Registry>) -> (String, Vec<u8>) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    // The CA and the leaf need different names: with rcgen's default one for both, Windows' chain
    // builder takes the leaf for a self-signed certificate and cannot verify its signature.
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "layer download test root");
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca_params, ca_key);
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let mut leaf_params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    leaf_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");
    let leaf = leaf_params.signed_by(&leaf_key, &issuer).unwrap();
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![leaf.der().clone(), ca.der().clone()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
    )
    .unwrap();
    tls.alpn_protocols = vec![b"h2".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("localhost:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let (acceptor, registry) = (acceptor.clone(), Arc::clone(&registry));
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let Ok(mut conn) = h2::server::handshake(tls).await else {
                    return;
                };
                while let Some(Ok((request, respond))) = conn.accept().await {
                    tokio::spawn(answer(Arc::clone(&registry), request, respond));
                }
            });
        }
    });
    (host, ca.pem().into_bytes())
}

/// Downloads the registry's layer into a fresh cache. Returns the registry (for its request log),
/// the layer's result and the cached tarball, if it landed.
async fn download(
    behaviour: Behaviour,
) -> (Arc<Registry>, Result<(), ImageError>, Option<Vec<u8>>) {
    let blob = blob();
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(&blob)));
    let registry = Arc::new(Registry {
        blob,
        behaviour,
        cut_count: AtomicUsize::new(0),
        requests: Mutex::default(),
    });
    let (host, root) = serve(Arc::clone(&registry)).await;
    let temp = tempdir().unwrap();
    let cache = crate::cache::GlobalCache::new(temp.path()).unwrap();
    let client = oci_client::Client::new(oci_client::client::ClientConfig {
        extra_root_certificates: vec![oci_client::client::Certificate {
            encoding: oci_client::client::CertificateEncoding::Pem,
            data: root,
        }],
        ..Default::default()
    });
    let reference: oci_client::Reference = format!("{host}/app:1").parse().unwrap();
    let layer = Layer::new(digest.parse().unwrap(), &cache);
    let result = layer
        .download(
            &client,
            &reference,
            Some(registry.blob.len() as u64),
            false,
            None,
            0,
        )
        .await;
    let tarball = std::fs::read(layer.tar_path_ref()).ok();
    (registry, result, tarball)
}

/// The offset a `Range: bytes=<n>-` header resumes from.
fn resume_offset(range: Option<&str>) -> usize {
    range
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.strip_suffix('-'))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("not a resume from an offset: {range:?}"))
}

#[tokio::test]
async fn test_layer_download_resumes_a_body_the_registry_cancelled_midway() {
    let (registry, result, tarball) = download(Behaviour {
        cuts: 1,
        cut_after: CUT_AFTER,
        ignore_range: false,
    })
    .await;

    result.unwrap();
    assert_eq!(tarball.as_deref(), Some(registry.blob.as_slice()));
    let requests = registry.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_eq!(requests[0], None);
    let offset = resume_offset(requests[1].as_deref());
    assert!(
        offset > 0 && offset <= CUT_AFTER,
        "resumed at {offset}, the cut was at {CUT_AFTER}"
    );
}

#[tokio::test]
async fn test_layer_download_resumes_more_than_once() {
    let (registry, result, tarball) = download(Behaviour {
        cuts: RETRY_DELAYS.len(),
        cut_after: CUT_AFTER / 4,
        ignore_range: false,
    })
    .await;

    result.unwrap();
    assert_eq!(tarball.as_deref(), Some(registry.blob.as_slice()));
    let offsets: Vec<usize> = registry.requests()[1..]
        .iter()
        .map(|range| resume_offset(range.as_deref()))
        .collect();
    assert_eq!(offsets.len(), RETRY_DELAYS.len());
    assert!(
        offsets.windows(2).all(|pair| pair[0] < pair[1]),
        "every resume starts further on: {offsets:?}"
    );
}

#[tokio::test]
async fn test_layer_download_restarts_when_the_registry_ignores_the_range() {
    let (registry, result, tarball) = download(Behaviour {
        cuts: 1,
        cut_after: CUT_AFTER,
        ignore_range: true,
    })
    .await;

    result.unwrap();
    assert_eq!(tarball.as_deref(), Some(registry.blob.as_slice()));
    assert_eq!(registry.requests().len(), 2);
}

#[tokio::test]
async fn test_layer_download_gives_up_with_the_reason_when_every_body_breaks_off() {
    let (registry, result, tarball) = download(Behaviour {
        cuts: usize::MAX,
        cut_after: 1000,
        ignore_range: false,
    })
    .await;

    let err = result.unwrap_err();
    assert!(tarball.is_none());
    assert_eq!(registry.requests().len(), 1 + RETRY_DELAYS.len());
    let ImageError::LayerBodyInterrupted {
        downloaded,
        attempts,
        ..
    } = &err
    else {
        panic!("expected LayerBodyInterrupted, got {err:?}");
    };
    assert_eq!(*attempts, 1 + RETRY_DELAYS.len());
    assert!(*downloaded > 0 && *downloaded <= 5000, "{downloaded}");
    let message = err.to_string();
    assert!(message.contains("gave up after 5 attempts"), "{message}");
    assert!(message.contains("stream no longer needed"), "{message}");
}

#[test]
fn test_only_transport_errors_are_resumed() {
    let transport = reqwest::Client::new()
        .get("http://[::1")
        .build()
        .unwrap_err();
    assert!(super::is_transport_error(&std::io::Error::other(transport)));
    assert!(!super::is_transport_error(&std::io::Error::other(
        "digest mismatch"
    )));
    assert!(!super::is_transport_error(&std::io::Error::from(
        std::io::ErrorKind::PermissionDenied
    )));
}
