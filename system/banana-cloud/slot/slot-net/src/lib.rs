//! One HTTPS agent for everything slot does online: Mozilla's roots plus any
//! in `SSL_CERT_FILE`, and the proxy from the environment.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub use ureq;

const TIMEOUT: Duration = Duration::from_secs(30);
const STALL: Duration = Duration::from_secs(60);

pub fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| builder().timeout(TIMEOUT).build())
}

/// [`agent`] for transfers that may run for many minutes, such as a CD image:
/// it gives up only when the connection stalls, never on the total time.
pub fn transfer_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        builder()
            .timeout_connect(TIMEOUT)
            .timeout_read(STALL)
            .timeout_write(STALL)
            .build()
    })
}

fn builder() -> ureq::AgentBuilder {
    use rustls_pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if let Some(file) = std::env::var_os("SSL_CERT_FILE") {
        match rustls_pki_types::CertificateDer::pem_file_iter(&file) {
            Ok(certs) => {
                let (added, _) = roots.add_parsable_certificates(certs.flatten());
                eprintln!("slot: net: {added} extra certificates from SSL_CERT_FILE");
            }
            Err(e) => eprintln!("slot: net: SSL_CERT_FILE: {e}"),
        }
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    ureq::AgentBuilder::new()
        .tls_config(Arc::new(tls))
        .try_proxy_from_env(true)
}
