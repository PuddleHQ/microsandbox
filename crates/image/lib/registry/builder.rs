use std::sync::Arc;

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

        let settings = Arc::new(ClientSettings {
            protocol,
            extra_root_certificates,
        });
        let client = settings.client(Some(self.platform.clone()));

        Ok(Registry {
            client,
            settings,
            auth: self.auth,
            platform: self.platform,
            cache: self.cache,
        })
    }
}

/// What every registry client of one [`Registry`] is built from.
pub(crate) struct ClientSettings {
    protocol: ClientProtocol,
    extra_root_certificates: Vec<Certificate>,
}

impl ClientSettings {
    /// A client with its own connection pool. `platform` picks the manifest of a multi-platform
    /// index; a client that only fetches blobs needs none.
    pub(crate) fn client(&self, platform: Option<Platform>) -> Client {
        Client::new(ClientConfig {
            protocol: self.protocol.clone(),
            extra_root_certificates: self.extra_root_certificates.clone(),
            platform_resolver: platform.map(|platform| {
                Box::new(move |manifests: &[_]| resolve_platform_digest(manifests, &platform)) as _
            }),
            ..Default::default()
        })
    }
}
