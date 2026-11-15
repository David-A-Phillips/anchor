//! Device identity and signer trust.
//!
//! Every device owns an Ed25519 key pair, kept as two small files in the keys directory:
//!
//! * `device.key` — the 32-byte secret key as 64 hex characters, owner-only (`0600`) on unix;
//! * `device.pub` — the 32-byte public key as 64 hex characters.
//!
//! The [`DeviceId`] is the first 16 bytes of the BLAKE3 hash of the public key, in hex. It is a
//! convenient name, not a credential: trust decisions are always made on the full public key.
//!
//! Trusted signers are kept in `trusted_devices.json` next to the keys ([`TrustedDevices`]) rather
//! than in SQLite, so that a relay-only server and offline tools (`anchor pack inspect`) can verify
//! packs without opening the database.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::base::{DeviceId, Error, Result, now_ms};

/// File name of the secret key inside the keys directory.
pub const SECRET_KEY_FILE: &str = "device.key";
/// File name of the public key inside the keys directory.
pub const PUBLIC_KEY_FILE: &str = "device.pub";
/// File name of the trusted device list inside the keys directory.
pub const TRUSTED_DEVICES_FILE: &str = "trusted_devices.json";

/// An Ed25519 public key. Serialises as 64 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    /// Wraps raw key bytes. The bytes are validated as a curve point when a signature is verified.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parses 64 hex characters.
    pub fn from_hex(text: &str) -> Result<Self> {
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(text.trim(), &mut bytes)
            .map_err(|err| Error::invalid(format!("invalid public key: {err}")))?;
        Ok(Self(bytes))
    }

    /// Raw key bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex, 64 characters.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The device id derived from this key: the first 16 bytes of its BLAKE3 hash, in hex.
    pub fn device_id(&self) -> DeviceId {
        let digest = blake3::hash(&self.0);
        DeviceId::new(hex::encode(&digest.as_bytes()[..16]))
    }

    /// Verifies that `signature` is a valid signature of `bytes` by this key.
    ///
    /// Uses strict verification (no malleable or small-order signatures), so a signature has
    /// exactly one accepted encoding. Fails with [`Error::Integrity`].
    pub fn verify(&self, bytes: &[u8], signature: &DeviceSignature) -> Result<()> {
        let key = VerifyingKey::from_bytes(&self.0)
            .map_err(|_| Error::integrity("public key is not a valid Ed25519 key"))?;
        let signature = ed25519_dalek::Signature::from_bytes(&signature.0);
        key.verify_strict(bytes, &signature)
            .map_err(|_| Error::integrity("signature verification failed"))
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.to_hex())
    }
}

impl Serialize for PublicKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        PublicKey::from_hex(&text).map_err(serde::de::Error::custom)
    }
}

/// An Ed25519 signature. Serialises as 128 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DeviceSignature([u8; 64]);

impl DeviceSignature {
    /// Length of a signature in bytes.
    pub const LEN: usize = 64;

    /// Wraps raw signature bytes.
    pub const fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }

    /// Raw signature bytes.
    pub const fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }
}

impl fmt::Debug for DeviceSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceSignature({})", hex::encode(&self.0[..6]))
    }
}

impl Serialize for DeviceSignature {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> Deserialize<'de> for DeviceSignature {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let mut bytes = [0u8; 64];
        hex::decode_to_slice(text.trim(), &mut bytes).map_err(serde::de::Error::custom)?;
        Ok(Self(bytes))
    }
}

/// Verifies `signature` over `bytes` against `public_key`. See [`PublicKey::verify`].
pub fn verify(public_key: &PublicKey, bytes: &[u8], signature: &DeviceSignature) -> Result<()> {
    public_key.verify(bytes, signature)
}

/// This device's signing identity.
pub struct DeviceIdentity {
    signing: SigningKey,
    public: PublicKey,
    device_id: DeviceId,
}

impl DeviceIdentity {
    /// Creates a fresh identity that is not persisted anywhere (tests, throw-away tools).
    pub fn generate() -> Self {
        let mut secret = [0u8; 32];
        rand::fill(&mut secret);
        Self::from_secret_bytes(&secret)
    }

    /// Builds the identity belonging to a 32-byte Ed25519 secret key.
    pub fn from_secret_bytes(secret: &[u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(secret);
        let public = PublicKey(signing.verifying_key().to_bytes());
        let device_id = public.device_id();
        Self {
            signing,
            public,
            device_id,
        }
    }

    /// Loads the identity stored in `keys_dir`, creating the directory and a new key pair when
    /// none exists yet.
    ///
    /// The secret key file is created with owner-only permissions on unix, and permissions that
    /// were loosened afterwards are tightened again on load. The public key file is rewritten if it
    /// is missing or does not match the secret key. Blocking (filesystem).
    pub fn load_or_create(keys_dir: &Path) -> Result<Self> {
        fs::create_dir_all(keys_dir)?;
        let secret_path = keys_dir.join(SECRET_KEY_FILE);
        if !secret_path.exists() {
            let identity = Self::generate();
            let text = hex::encode(identity.signing.to_bytes());
            match write_new_private(keys_dir, &secret_path, text.as_bytes()) {
                Ok(()) => {
                    identity.write_public(keys_dir)?;
                    return Ok(identity);
                }
                // Another process created the key between the check and the write: use theirs.
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err.into()),
            }
        }
        let text = fs::read_to_string(&secret_path)?;
        let mut secret = [0u8; 32];
        hex::decode_to_slice(text.trim(), &mut secret).map_err(|err| {
            Error::config(format!(
                "device key {} is malformed: {err}",
                secret_path.display()
            ))
        })?;
        restrict_permissions(&secret_path)?;
        let identity = Self::from_secret_bytes(&secret);
        let public_path = keys_dir.join(PUBLIC_KEY_FILE);
        let stored = fs::read_to_string(&public_path).ok();
        if stored.as_deref().map(str::trim) != Some(identity.public.to_hex().as_str()) {
            identity.write_public(keys_dir)?;
        }
        Ok(identity)
    }

    fn write_public(&self, keys_dir: &Path) -> Result<()> {
        write_atomic(
            &keys_dir.join(PUBLIC_KEY_FILE),
            format!("{}\n", self.public.to_hex()).as_bytes(),
        )
    }

    /// The device id derived from the public key (see [`PublicKey::device_id`]).
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// The public half of the key pair.
    pub fn public_key(&self) -> PublicKey {
        self.public
    }

    /// Signs `bytes` with the device key. Deterministic (RFC 8032).
    pub fn sign(&self, bytes: &[u8]) -> DeviceSignature {
        DeviceSignature(self.signing.sign(bytes).to_bytes())
    }
}

impl fmt::Debug for DeviceIdentity {
    // Never prints the secret key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("device_id", &self.device_id)
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

/// Creates `path` with owner-only permissions, failing with `AlreadyExists` if it is already there.
fn write_new_private(dir: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Temp files are created 0600 on unix, so the secret never exists with wider permissions.
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|err| err.error)?;
    Ok(())
}

/// Replaces `path` atomically (write to a sibling temp file, then rename).
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::invalid(format!("{} has no parent directory", path.display())))?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|err| Error::Io(err.error))?;
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// Which signers are accepted when verifying packs and peer greetings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrustPolicy {
    /// Accept any key whose signature verifies. For development and single-user setups only.
    AnyValid,
    /// Accept only these keys.
    Allowlist(BTreeSet<PublicKey>),
}

impl TrustPolicy {
    /// An allowlist of the given keys.
    pub fn allowlist(keys: impl IntoIterator<Item = PublicKey>) -> Self {
        Self::Allowlist(keys.into_iter().collect())
    }

    /// `true` when `key` is an acceptable signer.
    pub fn trusts(&self, key: &PublicKey) -> bool {
        match self {
            Self::AnyValid => true,
            Self::Allowlist(keys) => keys.contains(key),
        }
    }

    /// Checks that `key` is an acceptable signer and that `signature` is its signature of `bytes`.
    /// Fails with [`Error::Integrity`] in both cases; the message tells them apart.
    pub fn verify(
        &self,
        key: &PublicKey,
        bytes: &[u8],
        signature: &DeviceSignature,
    ) -> Result<()> {
        if !self.trusts(key) {
            return Err(Error::integrity(format!(
                "signer {} (device {}) is not a trusted device",
                key,
                key.device_id()
            )));
        }
        key.verify(bytes, signature)
    }
}

/// One entry of the trusted device list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedDevice {
    /// The device's public key.
    pub public_key: PublicKey,
    /// Device id derived from the key; stored for readability, recomputed on load.
    pub device_id: DeviceId,
    /// Human-readable label ("Ana's laptop").
    pub label: String,
    /// When the device was added, milliseconds since the Unix epoch.
    pub added_ms: i64,
}

#[derive(Serialize, Deserialize)]
struct TrustedDevicesFile {
    version: u32,
    devices: Vec<TrustedDevice>,
}

/// The persisted allowlist of trusted signer keys (`trusted_devices.json` in the keys directory).
///
/// Every mutation is written through atomically. The store is a plain value: wrap it in a mutex to
/// share it, and reload it to observe changes made by another process. Blocking (filesystem).
#[derive(Debug)]
pub struct TrustedDevices {
    path: PathBuf,
    devices: BTreeMap<PublicKey, TrustedDevice>,
}

impl TrustedDevices {
    /// Opens the list stored in `keys_dir`; a missing file is an empty list.
    pub fn open(keys_dir: &Path) -> Result<Self> {
        let path = keys_dir.join(TRUSTED_DEVICES_FILE);
        let mut devices = BTreeMap::new();
        match fs::read(&path) {
            Ok(bytes) => {
                let file: TrustedDevicesFile = serde_json::from_slice(&bytes).map_err(|err| {
                    Error::config(format!("{} is malformed: {err}", path.display()))
                })?;
                if file.version != 1 {
                    return Err(Error::config(format!(
                        "{} has unsupported version {}",
                        path.display(),
                        file.version
                    )));
                }
                for mut device in file.devices {
                    // The id is derived data: never trust the stored copy.
                    device.device_id = device.public_key.device_id();
                    devices.insert(device.public_key, device);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        Ok(Self { path, devices })
    }

    /// Trusts `key` under `label`; re-adding a key updates its label. Returns the stored entry.
    pub fn add(&mut self, key: PublicKey, label: impl Into<String>) -> Result<TrustedDevice> {
        let label = label.into();
        let added_ms = self.devices.get(&key).map_or_else(now_ms, |d| d.added_ms);
        let device = TrustedDevice {
            public_key: key,
            device_id: key.device_id(),
            label,
            added_ms,
        };
        self.devices.insert(key, device.clone());
        self.save()?;
        Ok(device)
    }

    /// Stops trusting `key`. Returns `false` when it was not in the list.
    pub fn remove(&mut self, key: &PublicKey) -> Result<bool> {
        let removed = self.devices.remove(key).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// All trusted devices, ordered by public key.
    pub fn list(&self) -> Vec<TrustedDevice> {
        self.devices.values().cloned().collect()
    }

    /// The entry for `key`, if trusted.
    pub fn get(&self, key: &PublicKey) -> Option<&TrustedDevice> {
        self.devices.get(key)
    }

    /// The allowlist policy made of the current entries.
    pub fn policy(&self) -> TrustPolicy {
        TrustPolicy::Allowlist(self.devices.keys().copied().collect())
    }

    fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = TrustedDevicesFile {
            version: 1,
            devices: self.devices.values().cloned().collect(),
        };
        write_atomic(&self.path, &serde_json::to_vec_pretty(&file)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_round_trips_and_is_private() {
        let dir = tempfile::tempdir().expect("tempdir");
        let keys = dir.path().join("keys");
        let created = DeviceIdentity::load_or_create(&keys).expect("create");
        let loaded = DeviceIdentity::load_or_create(&keys).expect("load");
        assert_eq!(created.public_key(), loaded.public_key());
        assert_eq!(created.device_id(), loaded.device_id());
        assert_eq!(created.device_id().as_str().len(), 32);
        assert_eq!(created.device_id(), &created.public_key().device_id());
        let stored = fs::read_to_string(keys.join(PUBLIC_KEY_FILE)).expect("public key file");
        assert_eq!(stored.trim(), created.public_key().to_hex());
        assert!(!format!("{created:?}").contains(&hex::encode(created.signing.to_bytes())));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let path = keys.join(SECRET_KEY_FILE);
            let mode = |p: &Path| fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
            DeviceIdentity::load_or_create(&keys).expect("reload");
            assert_eq!(mode(&path), 0o600);
        }

        // A missing public key file is restored; a corrupt secret is an error, not a new identity.
        fs::remove_file(keys.join(PUBLIC_KEY_FILE)).expect("remove");
        DeviceIdentity::load_or_create(&keys).expect("reload");
        assert!(keys.join(PUBLIC_KEY_FILE).exists());
        fs::write(keys.join(SECRET_KEY_FILE), "not hex").expect("write");
        assert!(DeviceIdentity::load_or_create(&keys).is_err());
    }

    #[test]
    fn signatures_verify_and_reject() {
        let alice = DeviceIdentity::generate();
        let bob = DeviceIdentity::generate();
        let signature = alice.sign(b"payload");
        verify(&alice.public_key(), b"payload", &signature).expect("valid");
        assert!(verify(&alice.public_key(), b"payloae", &signature).is_err());
        assert!(verify(&bob.public_key(), b"payload", &signature).is_err());
        let mut bytes = *signature.as_bytes();
        bytes[10] ^= 1;
        assert!(
            verify(
                &alice.public_key(),
                b"payload",
                &DeviceSignature::from_bytes(bytes)
            )
            .is_err()
        );
        let json = serde_json::to_string(&signature).expect("json");
        let back: DeviceSignature = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, signature);
    }

    #[test]
    fn trust_policy_and_persisted_allowlist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let alice = DeviceIdentity::generate();
        let bob = DeviceIdentity::generate();
        let signature = bob.sign(b"x");

        let mut trusted = TrustedDevices::open(dir.path()).expect("open");
        assert!(trusted.list().is_empty());
        trusted.add(alice.public_key(), "alice").expect("add");
        assert!(
            trusted
                .policy()
                .verify(&bob.public_key(), b"x", &signature)
                .is_err()
        );
        TrustPolicy::AnyValid
            .verify(&bob.public_key(), b"x", &signature)
            .expect("any valid signature");
        trusted.add(bob.public_key(), "bob").expect("add");
        trusted.add(bob.public_key(), "bob's laptop").expect("relabel");

        let mut reopened = TrustedDevices::open(dir.path()).expect("reopen");
        assert_eq!(reopened.list().len(), 2);
        let entry = reopened.get(&bob.public_key()).expect("bob");
        assert_eq!(entry.label, "bob's laptop");
        assert_eq!(&entry.device_id, bob.device_id());
        reopened
            .policy()
            .verify(&bob.public_key(), b"x", &signature)
            .expect("trusted");
        assert!(reopened.remove(&bob.public_key()).expect("remove"));
        assert!(!reopened.remove(&bob.public_key()).expect("remove again"));
        assert!(
            !TrustedDevices::open(dir.path())
                .expect("reopen")
                .policy()
                .trusts(&bob.public_key())
        );
    }
}
