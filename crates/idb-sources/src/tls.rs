//! rustls 설정. Go `crypto/tls` 의 세 가지 쓰임(InsecureSkipVerify·체인만 검증·이름까지 검증)을 맞춘다.
//! 인증서 루트는 Go 처럼 시스템 저장소를 쓴다.

use std::sync::{Arc, OnceLock};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::net::io_reason;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verify {
    /// 인증서를 검증하지 않는다(pgx sslmode=require·prefer). 핸드셰이크 서명은 본다
    None,
    /// 체인만 검증하고 호스트 이름은 보지 않는다(sslmode=verify-ca)
    Chain,
    /// 체인과 호스트 이름을 검증한다(sslmode=verify-full, Redis TLS)
    Full,
}

fn provider() -> Arc<CryptoProvider> {
    static P: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    P.get_or_init(|| Arc::new(rustls::crypto::ring::default_provider())).clone()
}

fn roots() -> Arc<RootCertStore> {
    static R: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    R.get_or_init(|| {
        let mut store = RootCertStore::empty();
        // 읽지 못한 인증서는 건너뛴다. 하나도 없으면 검증이 실패해 연결하지 않는다
        store.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        Arc::new(store)
    })
    .clone()
}

/// verify 에 맞는 TLS 설정이다. TLS 1.2 이상만 쓴다.
fn config(verify: Verify) -> Result<Arc<ClientConfig>, String> {
    let builder = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let webpki = || {
        WebPkiServerVerifier::builder_with_provider(roots(), provider())
            .build()
            .map_err(|e| e.to_string())
    };
    let cfg = match verify {
        Verify::Full => builder.with_webpki_verifier(webpki()?).with_no_client_auth(),
        Verify::Chain => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(ChainOnly(webpki()?)))
            .with_no_client_auth(),
        Verify::None => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider())))
            .with_no_client_auth(),
    };
    Ok(Arc::new(cfg))
}

/// tcp 위에서 TLS 핸드셰이크를 한다. server_name 은 SNI·검증 이름이다.
pub(crate) async fn handshake(tcp: TcpStream, server_name: &str, verify: Verify) -> Result<TlsStream<TcpStream>, String> {
    let name = ServerName::try_from(server_name.to_string()).map_err(|e| e.to_string())?;
    TlsConnector::from(config(verify)?).connect(name, tcp).await.map_err(|e| {
        match e.get_ref().and_then(|r| r.downcast_ref::<rustls::Error>()) {
            Some(te) => te.to_string(),
            None => io_reason(&e),
        }
    })
}

#[derive(Debug)]
struct NoVerify(Arc<CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// libpq verify-ca 처럼 체인은 검증하고 이름이 맞지 않는 것만 받는다.
#[derive(Debug)]
struct ChainOnly(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.0.verify_server_cert(end_entity, intermediates, server_name, ocsp, now) {
            Err(rustls::Error::InvalidCertificate(CertificateError::NotValidForName))
            | Err(rustls::Error::InvalidCertificate(CertificateError::NotValidForNameContext { .. })) => {
                Ok(ServerCertVerified::assertion())
            }
            r => r,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}
