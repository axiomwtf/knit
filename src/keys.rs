//! Ed25519 identity, stored like vfsx: ~/.config/knit/keys/identity.{priv,pub}

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct KeyPair {
    signing: SigningKey,
}

impl KeyPair {
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut seed);
        Self { signing: SigningKey::from_bytes(&seed) }
    }

    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self { signing: SigningKey::from_bytes(seed) }
    }

    pub fn seed(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }
}

pub fn verify(pubkey: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> Result<()> {
    let vk = VerifyingKey::from_bytes(pubkey).map_err(|e| anyhow!("bad public key: {e}"))?;
    vk.verify_strict(msg, &ed25519_dalek::Signature::from_bytes(sig))
        .map_err(|e| anyhow!("signature verification failed: {e}"))
}

pub struct KeyStorage {
    keys_dir: PathBuf,
}

impl KeyStorage {
    pub fn new() -> Result<Self> {
        let config_dir = dirs::config_dir().ok_or_else(|| anyhow!("Could not determine config directory"))?;
        Ok(Self { keys_dir: config_dir.join("knit").join("keys") })
    }

    pub fn with_path(path: PathBuf) -> Self {
        Self { keys_dir: path }
    }

    pub fn keys_dir(&self) -> &Path {
        &self.keys_dir
    }

    fn ensure_dir(&self) -> Result<()> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.keys_dir)
            .with_context(|| format!("Failed to create {}", self.keys_dir.display()))
    }

    pub fn pubkey_path(&self) -> PathBuf {
        self.keys_dir.join("identity.pub")
    }

    pub fn privkey_path(&self) -> PathBuf {
        self.keys_dir.join("identity.priv")
    }

    pub fn has_keypair(&self) -> bool {
        self.privkey_path().exists()
    }

    pub fn generate_keypair(&self) -> Result<KeyPair> {
        self.ensure_dir()?;
        let keypair = KeyPair::generate();
        let mut priv_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.privkey_path())
            .context("Failed to write private key")?;
        priv_file.set_permissions(fs::Permissions::from_mode(0o600))?;
        priv_file.write_all(&keypair.seed())?;
        priv_file.sync_all()?;
        fs::write(self.pubkey_path(), keypair.public_key_bytes()).context("Failed to write public key")?;
        Ok(keypair)
    }

    pub fn load_keypair(&self) -> Result<KeyPair> {
        let privkey_path = self.privkey_path();
        if !privkey_path.exists() {
            return self.generate_keypair();
        }
        let seed: [u8; 32] = fs::read(&privkey_path)
            .with_context(|| format!("Failed to read private key: {}", privkey_path.display()))?
            .try_into()
            .map_err(|b: Vec<u8>| anyhow!("Invalid private key length: expected 32 bytes, got {}", b.len()))?;
        let keypair = KeyPair::from_seed(&seed);
        if fs::read(self.pubkey_path()).ok().as_deref() != Some(&keypair.public_key_bytes()[..]) {
            fs::write(self.pubkey_path(), keypair.public_key_bytes()).context("Failed to write public key")?;
        }
        Ok(keypair)
    }

    /// Load only if a keypair exists; used by read-only commands so they
    /// don't silently mint a key.
    pub fn try_load_keypair(&self) -> Result<Option<KeyPair>> {
        if !self.has_keypair() {
            return Ok(None);
        }
        Ok(Some(self.load_keypair()?))
    }
}
