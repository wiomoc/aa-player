use std::{net::Ipv4Addr, sync::Arc};

use log::error;
use rustls::{
    ClientConfig, RootCertStore,
    client::{
        UnbufferedClientConnection, WebPkiServerVerifier,
        danger::{ServerCertVerified, ServerCertVerifier},
    },
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject},
    unbuffered::{
        ConnectionState, EncodeError, EncryptError, InsufficientSizeError, UnbufferedStatus,
    },
    version::TLS12,
};

pub(crate) struct EncryptionManager {
    client_config: Arc<ClientConfig>,
}

pub(crate) struct EncryptedConnectionManager {
    conn: UnbufferedClientConnection,
    is_handshaking: bool,
}

impl EncryptionManager {
    pub(crate) fn new() -> Self {
        let (ca_cert, cert, key) = Self::load_cert_and_key();

        let builder = ClientConfig::builder_with_protocol_versions(&[&TLS12]);
        let provider = builder.crypto_provider().clone();
        let client_config = Arc::new(
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(CustomServerCertVerifier::new(
                    ca_cert, provider,
                )))
                .with_client_auth_cert(cert, key)
                .unwrap(),
        );
        EncryptionManager { client_config }
    }

    pub(crate) fn new_connection(&self) -> EncryptedConnectionManager {
        let conn = UnbufferedClientConnection::new(
            self.client_config.clone(),
            ServerName::IpAddress(Ipv4Addr::new(0, 0, 0, 0).into()),
        )
        .unwrap();

        EncryptedConnectionManager {
            conn,
            is_handshaking: true,
        }
    }

    fn load_cert_and_key() -> (
        CertificateDer<'static>,
        Vec<CertificateDer<'static>>,
        PrivateKeyDer<'static>,
    ) {
        let ca_cert_pem = include_str!("../pki/ca_cert.pem");
        let ca_cert = CertificateDer::pem_slice_iter(ca_cert_pem.as_bytes())
            .nth(0)
            .unwrap()
            .unwrap();

        let cert_pem = include_str!("../pki/cert.pem");
        let cert = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
            .map(|res| res.unwrap())
            .collect();

        let key_pem = include_str!("../pki/key.pem");
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).unwrap();

        (ca_cert, cert, key)
    }
}

impl EncryptedConnectionManager {
    pub(crate) fn process_handshake_message(
        &mut self,
        payload: &mut [u8],
    ) -> Result<Option<Vec<u8>>, ()> {
        assert!(self.is_handshaking);

        let mut position = 0usize;
        let mut outgoing_message: Option<Vec<u8>> = None;
        loop {
            let UnbufferedStatus { discard, state, .. } =
                self.conn.process_tls_records(&mut payload[position..]);
            position += discard;
            let state = state.map_err(|err| error!("couldn't process handshake {err:?}"))?;

            match state {
                ConnectionState::EncodeTlsData(mut encode_tls_data) => {
                    let initial_result = encode_tls_data.encode(&mut []);
                    let initial_err = initial_result.unwrap_err();
                    let message_size = match initial_err {
                        EncodeError::InsufficientSize(insufficient_size_error) => {
                            insufficient_size_error.required_size
                        }
                        EncodeError::AlreadyEncoded => unreachable!(),
                    };

                    let message = if let Some(ref mut outgoing_message) = outgoing_message {
                        let pos = outgoing_message.len();
                        outgoing_message.resize(pos + message_size, 0);
                        &mut outgoing_message[pos..]
                    } else {
                        outgoing_message = Some(vec![0u8; message_size]);
                        outgoing_message.as_mut().unwrap()
                    };
                    encode_tls_data
                        .encode(message)
                        .map_err(|err| error!("couldn't process handshake {err:?}"))?;
                }
                ConnectionState::TransmitTlsData(transmit_tls_data) => {
                    transmit_tls_data.done();
                    return Ok(outgoing_message);
                }
                ConnectionState::WriteTraffic(_write_traffic) => {
                    self.is_handshaking = false;
                    assert!(position == payload.len());
                    return Ok(None);
                }

                _ => return Ok(None),
            }
        }
    }

    pub(crate) fn encrypt_message(&mut self, message: &[u8]) -> Result<Vec<u8>, ()> {
        assert!(!self.is_handshaking);

        let estimated_tls_overhead_length = 40usize;
        let UnbufferedStatus { state, .. } = self.conn.process_tls_records(&mut []);
        let state = state.map_err(|err| error!("couldn't encrypt {err:?}"))?;
        match state {
            ConnectionState::WriteTraffic(mut write_traffic) => {
                let mut encrypted_message =
                    vec![0u8; message.len() + estimated_tls_overhead_length];
                loop {
                    match write_traffic.encrypt(message, &mut encrypted_message) {
                        Ok(size) => {
                            encrypted_message.truncate(size);
                            return Ok(encrypted_message);
                        }
                        Err(EncryptError::InsufficientSize(InsufficientSizeError {
                            required_size,
                        })) => {
                            encrypted_message.resize(required_size, 0);
                        }
                        Err(err) => {
                            error!("couldn't encrypt {err:?}");
                            return Err(());
                        }
                    }
                }
            }
            _ => todo!(),
        }
    }

    pub(crate) fn decrypt_message(&mut self, encrypted_message: &mut [u8]) -> Result<Vec<u8>, ()> {
        if self.is_handshaking {
            error!("trying to decrypt but handshaking not finshed");
            return Err(());
        }

        let mut position = 0usize;
        let mut message: Option<Vec<u8>> = None;
        loop {
            let UnbufferedStatus { discard, state, .. } = self
                .conn
                .process_tls_records(&mut encrypted_message[position..]);
            let state = state.map_err(|err| error!("couldn't decrypt {err:?}"))?;
            position += discard;
            match state {
                ConnectionState::ReadTraffic(mut read_traffic) => {
                    while let Some(record) = read_traffic.next_record() {
                        let record = record.map_err(|err| error!("couldn't decrypt {err:?}"))?;
                        position += record.discard;

                        if let Some(ref mut message) = message {
                            message.extend_from_slice(record.payload);
                        } else {
                            let mut message_vec = Vec::with_capacity(record.payload.len());
                            message_vec.extend_from_slice(record.payload);
                            message = Some(message_vec);
                        };
                    }
                }
                _ => {
                    break;
                }
            }
        }
        assert!(position == encrypted_message.len());
        message
            .ok_or(())
            .map_err(|err| error!("couldn't decrypt {err:?}"))
    }

    pub(crate) fn is_handshaking(&self) -> bool {
        self.is_handshaking
    }
}

#[derive(Debug)]
struct CustomServerCertVerifier(Arc<WebPkiServerVerifier>);

impl CustomServerCertVerifier {
    fn new(ca_cert: CertificateDer<'_>, provider: Arc<CryptoProvider>) -> Self {
        let mut root_store = RootCertStore::empty();
        let added = root_store.add_parsable_certificates([ca_cert]).0 == 1;
        assert!(added);
        CustomServerCertVerifier(
            WebPkiServerVerifier::builder_with_provider(Arc::new(root_store), provider)
                .build()
                .unwrap(),
        )
    }
}

impl ServerCertVerifier for CustomServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForNameContext { .. }
                | rustls::CertificateError::NotValidForName,
            )) => Ok(ServerCertVerified::assertion()),
            res => res,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}
