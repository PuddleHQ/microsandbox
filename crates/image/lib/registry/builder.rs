use oci_client::{
    Client,
    client::{Certificate, CertificateEncoding, ClientConfig, ClientProtocol},
};
use rustls_pki_types::{CertificateDer, pem::PemObject};

use microsandbox_types::RegistryAuth;

use crate::{
    cache::GlobalCache,
    error::{ImageError, ImageResult},
    platform::Platform,
};

use super::client::{Registry, resolve_platform_digest};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Builder for constructing a [`Registry`] client with optional auth and TLS settings.
pub struct RegistryBuilder {
    pub(super) platform: Platform,
    pub(super) cache: GlobalCache,
    pub(super) auth: oci_client::secrets::RegistryAuth,
    pub(super) insecure_registries: Vec<String>,
    pub(super) extra_ca_certs: Vec<Vec<u8>>,
    pub(super) proxy: Option<String>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl RegistryBuilder {
    /// Create a registry builder with anonymous authentication and default TLS settings.
    pub(crate) fn new(platform: Platform, cache: GlobalCache) -> Self {
        Self {
            platform,
            cache,
            auth: oci_client::secrets::RegistryAuth::Anonymous,
            insecure_registries: Vec::new(),
            extra_ca_certs: Vec::new(),
            proxy: None,
        }
    }

    /// Set authentication credentials for the registry.
    pub fn auth(mut self, auth: RegistryAuth) -> Self {
        self.auth = match auth {
            RegistryAuth::Anonymous => oci_client::secrets::RegistryAuth::Anonymous,
            RegistryAuth::Basic { username, password } => {
                oci_client::secrets::RegistryAuth::Basic(username, password)
            }
        };
        self
    }

    /// Add registries that should be accessed over plain HTTP instead of HTTPS.
    pub fn add_insecure_registries(mut self, registries: Vec<String>) -> Self {
        self.insecure_registries.extend(registries);
        self
    }

    /// Add PEM-encoded CA root certificates to trust.
    pub fn extra_ca_certs(mut self, certs: Vec<Vec<u8>>) -> Self {
        self.extra_ca_certs = certs;
        self
    }

    /// Send every registry request through this proxy.
    ///
    /// The URL may carry credentials (`http://user:token@127.0.0.1:3128`) and is used for both
    /// HTTPS (`CONNECT`) and plain HTTP registries. It is in effect instead of the process
    /// environment: `HTTP(S)_PROXY`, `ALL_PROXY` and `NO_PROXY` are not consulted for any
    /// request, so a caller can route pulls without putting the proxy (or its credentials) into
    /// the environment that child processes inherit. An unusable URL fails [`build`](Self::build)
    /// with [`ImageError::InvalidProxy`]; it never falls back to a direct connection.
    pub fn proxy(mut self, url: impl Into<String>) -> Self {
        self.proxy = Some(url.into());
        self
    }

    /// Build the registry client.
    ///
    /// Returns [`ImageError::InvalidCertificate`] if any PEM data in
    /// `extra_ca_certs` cannot be parsed as valid certificates.
    pub fn build(self) -> ImageResult<Registry> {
        let protocol = if self.insecure_registries.is_empty() {
            ClientProtocol::Https
        } else {
            ClientProtocol::HttpsExcept(self.insecure_registries)
        };

        let mut extra_root_certificates = Vec::new();
        for (i, pem_data) in self.extra_ca_certs.into_iter().enumerate() {
            let certs: Vec<_> = CertificateDer::pem_slice_iter(&pem_data)
                .collect::<Result<_, _>>()
                .map_err(|e| {
                    ImageError::InvalidCertificate(format!("entry {i}: failed to parse: {e}"))
                })?;

            if certs.is_empty() {
                return Err(ImageError::InvalidCertificate(format!(
                    "entry {i}: no certificates found in PEM data"
                )));
            }

            for cert in certs {
                extra_root_certificates.push(Certificate {
                    encoding: CertificateEncoding::Der,
                    data: cert.to_vec(),
                });
            }
        }

        // `Client::new` swallows a bad proxy and falls back to a default client, which would
        // read the environment or connect directly; validate here so a bad URL is an error.
        // The message names the failure, not the URL: it can hold credentials.
        if let Some(url) = &self.proxy {
            reqwest::Proxy::all(url)
                .map_err(|e| ImageError::InvalidProxy(e.without_url().to_string()))?;
        }

        let platform = self.platform.clone();
        let client = Client::new(ClientConfig {
            protocol,
            extra_root_certificates,
            https_proxy: self.proxy.clone(),
            http_proxy: self.proxy,
            platform_resolver: Some(Box::new(move |manifests| {
                resolve_platform_digest(manifests, &platform)
            })),
            ..Default::default()
        });

        Ok(Registry {
            client,
            auth: self.auth,
            platform: self.platform,
            cache: self.cache,
        })
    }
}
