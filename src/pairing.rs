//! HomeKit normal pairing (HKP=3) and authenticated control connections.
use airplay_core::Device;
use airplay_crypto::{chacha::ControlCipher, ed25519::IdentityKeyPair};
use airplay_pairing::{ControllerIdentity, PairSetup, PairVerify};
use airplay_rtsp::{RtspConnection, RtspMethod, RtspRequest};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

// Deliberately does not implement Debug: this contains a long-term secret.
#[derive(Serialize, Deserialize)]
pub struct Credentials {
    controller_id: String,
    seed: [u8; 32],
    receiver_key: [u8; 32],
}

pub struct CredentialStore(PathBuf);

impl CredentialStore {
    pub fn new(directory: PathBuf) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        ensure!(
            !fs::symlink_metadata(&directory)?.file_type().is_symlink(),
            "Credential directory must not be a symlink"
        );
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        Ok(Self(directory))
    }

    fn path(&self, device: &Device) -> PathBuf {
        self.0.join(format!(
            "{}.json",
            device.id.to_mac_string().replace(':', "")
        ))
    }

    pub fn load(&self, device: &Device) -> Result<Option<Credentials>> {
        match fs::read(self.path(device)) {
            Ok(data) => Ok(Some(
                serde_json::from_slice(&data).context("Invalid saved pairing identity")?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, device: &Device, credentials: &Credentials) -> Result<()> {
        write_private(&self.path(device), &serde_json::to_vec(credentials)?)
    }
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

pub struct ControlSession {
    pub transport: RtspConnection,
    pub device: Device,
    pub shared_secret: Option<[u8; 32]>,
    pub mirror_uri: Option<String>,
    pub stream_key: [u8; 16],
    pub stream_iv: [u8; 16],
    client_id: String,
}

impl ControlSession {
    pub async fn open(device: Device) -> Result<Self> {
        let addr = device
            .socket_addr()
            .context("Receiver has no usable address")?;
        let mut transport = RtspConnection::new(addr);
        tokio::time::timeout(Duration::from_secs(8), transport.connect())
            .await
            .context("Timed out connecting to receiver")??;
        let client_id = uuid::Uuid::new_v4().simple().to_string()[..12].to_owned();
        transport.add_session_header("User-Agent", "AirPlay/745.83");
        transport.add_session_header("X-Apple-Client-Name", "Spielab");
        transport.add_session_header("X-Apple-Device-ID", &client_id);
        let mut session = Self {
            transport,
            device,
            client_id,
            shared_secret: None,
            mirror_uri: None,
            stream_key: [0; 16],
            stream_iv: [0; 16],
        };
        session.request(RtspRequest::get_info()).await?;
        Ok(session)
    }

    pub async fn request(&mut self, request: RtspRequest) -> Result<Vec<u8>> {
        Ok(self
            .request_response(request)
            .await?
            .body
            .unwrap_or_default())
    }

    pub async fn request_response(
        &mut self,
        request: RtspRequest,
    ) -> Result<airplay_rtsp::RtspResponse> {
        let endpoint = request.uri.clone();
        let response = tokio::time::timeout(Duration::from_secs(12), self.transport.send(request))
            .await
            .with_context(|| format!("Receiver timed out at {endpoint}"))??;
        ensure!(
            (200..300).contains(&response.status_code),
            "Receiver rejected {endpoint}: status {}",
            response.status_code
        );
        Ok(response)
    }

    pub async fn stop_mirroring(&mut self) -> Result<()> {
        if let Some(uri) = self.mirror_uri.take() {
            tokio::time::timeout(
                Duration::from_secs(3),
                self.request(RtspRequest::teardown(uri)),
            )
            .await??;
        }
        Ok(())
    }

    pub async fn request_pin(&mut self) -> Result<()> {
        self.request(RtspRequest::new(RtspMethod::Post, "/pair-pin-start").body(Vec::new()))
            .await?;
        Ok(())
    }

    pub async fn pair(&mut self, pin: &str) -> Result<Credentials> {
        validate_pin(pin)?;
        let controller = ControllerIdentity::generate();
        let mut setup = PairSetup::new(pin);
        let m1 = setup.generate_m1()?;
        let m2 = self
            .request(RtspRequest::pair_setup(m1, &self.client_id, 3))
            .await?;
        setup.process_m2(&m2)?;
        let m3 = setup.generate_m3()?;
        let m4 = self
            .request(RtspRequest::pair_setup(m3, &self.client_id, 3))
            .await?;
        setup.process_m4(&m4)?;
        let m5 = setup.generate_m5_with_controller(&controller)?;
        let m6 = self
            .request(RtspRequest::pair_setup(m5, &self.client_id, 3))
            .await?;
        setup.process_m6(&m6)?;
        let credentials = Credentials {
            controller_id: controller.id().into(),
            seed: controller.keypair().seed(),
            receiver_key: setup
                .server_ltpk()
                .context("Receiver omitted its identity key")?,
        };
        self.verify(&credentials).await?;
        Ok(credentials)
    }

    pub async fn verify(&mut self, credentials: &Credentials) -> Result<()> {
        let controller = ControllerIdentity::with_id(
            IdentityKeyPair::from_seed(&credentials.seed),
            credentials.controller_id.clone(),
        );
        let mut verify = PairVerify::new_with_controller(&controller);
        verify.set_server_ltpk(credentials.receiver_key);
        let m1 = verify.generate_m1()?;
        let m2 = self.request(RtspRequest::pair_verify(m1, 3)).await?;
        verify.process_m2(&m2)?;
        let m3 = verify.generate_m3()?;
        let m4 = self.request(RtspRequest::pair_verify(m3, 3)).await?;
        let keys = verify.process_m4(&m4)?;
        self.shared_secret = verify.shared_secret();
        self.stream_key
            .copy_from_slice(&keys.write_key.as_bytes()[..16]);
        self.stream_iv
            .copy_from_slice(&keys.read_key.as_bytes()[..16]);
        self.transport.set_cipher(ControlCipher::new(
            *keys.write_key.as_bytes(),
            *keys.read_key.as_bytes(),
        ));
        // Only report an authenticated session after a successful encrypted round trip.
        self.request(RtspRequest::options()).await?;
        Ok(())
    }
}

pub fn validate_pin(pin: &str) -> Result<()> {
    if pin.len() != 4 || !pin.bytes().all(|b| b.is_ascii_digit()) {
        bail!("Enter the four digits displayed on Apple TV");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_accepts_leading_zero_and_rejects_unicode_or_whitespace() {
        assert!(validate_pin("0123").is_ok());
        for value in ["", "123", "12345", "１２３４", "12 3", "1234\n"] {
            assert!(validate_pin(value).is_err());
        }
    }

    #[test]
    fn credential_replacement_is_private_and_atomic() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("identity.json");
        write_private(&path, b"first").unwrap();
        write_private(&path, b"replacement").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
