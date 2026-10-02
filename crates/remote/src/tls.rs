//! Device identity and TLS. Every install owns one self-signed certificate;
//! peers know each other by its SHA-256 fingerprint, learned once at pairing
//! and pinned afterwards. No CA, no hostname: Tailscale addresses change
//! and a worker has no DNS name to put in a certificate.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use hmac::{Hmac, Mac};
use rustls::client::danger::{
	HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use rustls::crypto::{
	verify_tls12_signature, verify_tls13_signature, CryptoProvider,
};
use rustls::pki_types::{
	CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime,
};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
	ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig,
	SignatureScheme,
};
use sha2::{Digest, Sha256};

use crate::RemoteError;

/// The name every certificate carries and every client asks for; identity
/// is the fingerprint, never the name.
pub const CERT_NAME: &str = "snip-sync";

const CERT_FILE: &str = "remote-identity.der";
const KEY_FILE: &str = "remote-identity.key";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
	pub fn of(cert: &[u8]) -> Self {
		Self(Sha256::digest(cert).into())
	}

	pub fn to_hex(&self) -> String {
		self.0.iter().map(|b| format!("{b:02x}")).collect()
	}

	pub fn from_hex(text: &str) -> Option<Self> {
		let text = text.trim();
		if text.len() != 64 {
			return None;
		}
		let mut out = [0u8; 32];
		for (i, byte) in out.iter_mut().enumerate() {
			*byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
		}
		Some(Self(out))
	}

	/// What a person compares on two screens: the first 8 bytes, grouped.
	pub fn short(&self) -> String {
		self.0[..8]
			.chunks(2)
			.map(|c| format!("{:02X}{:02X}", c[0], c[1]))
			.collect::<Vec<_>>()
			.join("-")
	}
}

impl fmt::Debug for Fingerprint {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "Fingerprint({})", self.short())
	}
}

impl fmt::Display for Fingerprint {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.to_hex())
	}
}

pub struct Identity {
	cert: CertificateDer<'static>,
	key: Vec<u8>,
	fingerprint: Fingerprint,
}

impl Identity {
	/// A fresh identity that lives only in memory.
	pub fn generate() -> Result<Self, RemoteError> {
		let certified =
			rcgen::generate_simple_self_signed(vec![CERT_NAME.to_string()])
				.map_err(|err| RemoteError::Cert(err.to_string()))?;
		let cert = certified.cert.der().clone();
		let key = certified.signing_key.serialize_der();
		Ok(Self::from_parts(cert, key))
	}

	fn from_parts(cert: CertificateDer<'static>, key: Vec<u8>) -> Self {
		let fingerprint = Fingerprint::of(&cert);
		Self {
			cert,
			key,
			fingerprint,
		}
	}

	/// The identity kept in `dir`, created on first use. Changing it
	/// unpairs this device from every peer, so a damaged file is an error
	/// rather than silently replaced.
	pub fn load_or_create(dir: &Path) -> Result<Self, RemoteError> {
		let cert_path = dir.join(CERT_FILE);
		let key_path = dir.join(KEY_FILE);
		if cert_path.exists() || key_path.exists() {
			let cert = fs::read(&cert_path)?;
			let key = fs::read(&key_path)?;
			// Parse once here so a damaged pair fails now, not mid-handshake.
			let id = Self::from_parts(CertificateDer::from(cert), key);
			server_config(&id)?;
			return Ok(id);
		}
		let id = Self::generate()?;
		fs::create_dir_all(dir)?;
		fs::write(&cert_path, id.cert.as_ref())?;
		write_private(&key_path, &id.key)?;
		Ok(id)
	}

	pub fn fingerprint(&self) -> Fingerprint {
		self.fingerprint
	}

	fn chain(&self) -> Vec<CertificateDer<'static>> {
		vec![self.cert.clone()]
	}

	fn private_key(&self) -> PrivateKeyDer<'static> {
		PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone()))
	}
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
	use std::io::Write;
	use std::os::unix::fs::OpenOptionsExt;
	fs::OpenOptions::new()
		.write(true)
		.create_new(true)
		.mode(0o600)
		.open(path)?
		.write_all(bytes)
}

// %APPDATA% is already per-user on Windows.
#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
	fs::write(path, bytes)
}

/// What a master proves at pairing: it knows the code the worker shows,
/// bound to both certificates of this very connection. A relay in the
/// middle sees other certificates, so a proof it forwards does not match.
pub fn pairing_proof(
	code: &str,
	worker: &Fingerprint,
	master: &Fingerprint,
) -> [u8; 32] {
	let mut mac =
		Hmac::<Sha256>::new_from_slice(normalize_code(code).as_bytes())
			.expect("HMAC takes a key of any length");
	mac.update(b"snip-sync pair v1\0");
	mac.update(&worker.0);
	mac.update(&master.0);
	mac.finalize().into_bytes().into()
}

/// Codes are shown grouped and read aloud; case, spaces and dashes do not
/// count.
pub fn normalize_code(code: &str) -> String {
	code.chars()
		.filter(|c| c.is_ascii_alphanumeric())
		.map(|c| c.to_ascii_uppercase())
		.collect()
}

fn provider() -> Arc<CryptoProvider> {
	Arc::new(rustls::crypto::ring::default_provider())
}

/// The worker's side: TLS 1.3, and every client must present a
/// certificate. Whether that certificate is trusted is decided per request
/// (an unknown one may only pair), so the TLS layer checks the signature
/// and leaves the fingerprint to the caller.
pub fn server_config(id: &Identity) -> Result<Arc<ServerConfig>, RemoteError> {
	let provider = provider();
	let config = ServerConfig::builder_with_provider(provider.clone())
		.with_protocol_versions(&[&rustls::version::TLS13])?
		.with_client_cert_verifier(Arc::new(AnyClientCert {
			provider: provider.clone(),
		}))
		.with_single_cert(id.chain(), id.private_key())?;
	Ok(Arc::new(config))
}

/// The certificate a client saw, for pairing with no pin yet.
#[derive(Debug, Default)]
pub struct SeenCert(Mutex<Option<Fingerprint>>);

impl SeenCert {
	pub fn get(&self) -> Option<Fingerprint> {
		*self.0.lock().unwrap_or_else(PoisonError::into_inner)
	}
}

/// The master's side. With `pin`, any other server certificate fails the
/// handshake; without, any is accepted and recorded in the returned
/// [`SeenCert`] (pairing only).
pub fn client_config(
	id: &Identity,
	pin: Option<Fingerprint>,
) -> Result<(Arc<ClientConfig>, Arc<SeenCert>), RemoteError> {
	let provider = provider();
	let seen = Arc::new(SeenCert::default());
	let config = ClientConfig::builder_with_provider(provider.clone())
		.with_protocol_versions(&[&rustls::version::TLS13])?
		.dangerous()
		.with_custom_certificate_verifier(Arc::new(PinnedServer {
			provider,
			pin,
			seen: seen.clone(),
		}))
		.with_client_auth_cert(id.chain(), id.private_key())?;
	Ok((Arc::new(config), seen))
}

pub fn server_name() -> ServerName<'static> {
	ServerName::try_from(CERT_NAME).expect("a valid DNS name")
}

#[derive(Debug)]
struct PinnedServer {
	provider: Arc<CryptoProvider>,
	pin: Option<Fingerprint>,
	seen: Arc<SeenCert>,
}

impl ServerCertVerifier for PinnedServer {
	fn verify_server_cert(
		&self,
		end_entity: &CertificateDer<'_>,
		_intermediates: &[CertificateDer<'_>],
		_server_name: &ServerName<'_>,
		_ocsp_response: &[u8],
		_now: UnixTime,
	) -> Result<ServerCertVerified, rustls::Error> {
		let got = Fingerprint::of(end_entity);
		*self.seen.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(got);
		match self.pin {
			Some(pin) if pin != got => Err(rustls::Error::General(format!(
				"worker certificate {} is not the paired {}",
				got.short(),
				pin.short()
			))),
			_ => Ok(ServerCertVerified::assertion()),
		}
	}

	fn verify_tls12_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, rustls::Error> {
		verify_tls12_signature(
			message,
			cert,
			dss,
			&self.provider.signature_verification_algorithms,
		)
	}

	fn verify_tls13_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, rustls::Error> {
		verify_tls13_signature(
			message,
			cert,
			dss,
			&self.provider.signature_verification_algorithms,
		)
	}

	fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
		self.provider
			.signature_verification_algorithms
			.supported_schemes()
	}
}

#[derive(Debug)]
struct AnyClientCert {
	provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for AnyClientCert {
	fn root_hint_subjects(&self) -> &[DistinguishedName] {
		&[]
	}

	fn verify_client_cert(
		&self,
		_end_entity: &CertificateDer<'_>,
		_intermediates: &[CertificateDer<'_>],
		_now: UnixTime,
	) -> Result<ClientCertVerified, rustls::Error> {
		Ok(ClientCertVerified::assertion())
	}

	fn verify_tls12_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, rustls::Error> {
		verify_tls12_signature(
			message,
			cert,
			dss,
			&self.provider.signature_verification_algorithms,
		)
	}

	fn verify_tls13_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, rustls::Error> {
		verify_tls13_signature(
			message,
			cert,
			dss,
			&self.provider.signature_verification_algorithms,
		)
	}

	fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
		self.provider
			.signature_verification_algorithms
			.supported_schemes()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn fingerprint_hex_round_trips() {
		let fp = Fingerprint::of(b"cert");
		assert_eq!(Fingerprint::from_hex(&fp.to_hex()), Some(fp));
		assert_eq!(Fingerprint::from_hex("zz"), None);
		assert_eq!(fp.short().len(), 19);
	}

	#[test]
	fn proof_binds_code_and_both_certificates() {
		let (w, m, other) = (
			Fingerprint::of(b"w"),
			Fingerprint::of(b"m"),
			Fingerprint::of(b"x"),
		);
		let proof = pairing_proof("ab12-cd34", &w, &m);
		assert_eq!(proof, pairing_proof(" AB12 CD34 ", &w, &m));
		assert_ne!(proof, pairing_proof("AB12CD35", &w, &m));
		assert_ne!(proof, pairing_proof("AB12CD34", &other, &m));
		assert_ne!(proof, pairing_proof("AB12CD34", &w, &other));
	}

	#[test]
	fn identity_persists_and_reloads_the_same_fingerprint() {
		let dir = tempfile::tempdir().unwrap();
		let a = Identity::load_or_create(dir.path()).unwrap();
		let b = Identity::load_or_create(dir.path()).unwrap();
		assert_eq!(a.fingerprint(), b.fingerprint());
		fs::write(dir.path().join(KEY_FILE), b"garbage").unwrap();
		assert!(Identity::load_or_create(dir.path()).is_err());
	}
}
