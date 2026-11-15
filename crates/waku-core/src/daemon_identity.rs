//! The daemon's long-lived public identity.
//!
//! A client that paired with this daemon once remembers these public keys and
//! checks the signature on every later connection against them, which is what
//! stops a relay from substituting its own key. The private halves never leave
//! this process and are wiped on drop.
//!
//! Storage is a versioned JSON file rather than raw bytes so key rotation can
//! be added later as new fields without a format change. The file is written
//! atomically — a half-written key file would be as bad as a lost one — and a
//! file that exists but cannot be read is an error, never a reason to mint a
//! new identity: silently regenerating would invalidate every paired client.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

/// The on-disk shape. `version` is what makes rotation addable later: a reader
/// that meets a version it does not know refuses the file instead of guessing.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredIdentity {
    version: u32,
    /// Base64 of the 32-byte X25519 static secret.
    x25519_secret: String,
    /// Base64 of the 32-byte Ed25519 signing key seed.
    ed25519_secret: String,
}

const IDENTITY_VERSION: u32 = 1;
const IDENTITY_FILE_NAME: &str = "identity.key";

/// The daemon's identity: a key-agreement secret and a signing secret.
///
/// Both are held as the dalek secret types, which zero themselves on drop when
/// their `zeroize` feature is on (it is, for both crates in this workspace).
pub struct DaemonIdentity {
    x25519: StaticSecret,
    ed25519: SigningKey,
}

impl DaemonIdentity {
    /// Generate a fresh identity. Callers must persist it themselves.
    pub fn generate() -> Self {
        Self {
            x25519: StaticSecret::random(),
            ed25519: SigningKey::generate(&mut rand_core_os()),
        }
    }

    /// The public half a client stores after pairing.
    pub fn x25519_public(&self) -> X25519PublicKey {
        X25519PublicKey::from(&self.x25519)
    }

    pub fn ed25519_public(&self) -> ed25519_dalek::VerifyingKey {
        self.ed25519.verifying_key()
    }

    /// The key-agreement secret. Used once per connection against a client's
    /// ephemeral public key; the result is a session key, not this value.
    pub fn x25519_secret(&self) -> &StaticSecret {
        &self.x25519
    }

    pub fn ed25519_signing_key(&self) -> &SigningKey {
        &self.ed25519
    }

    /// Read the identity at `path`, or create and persist one if absent.
    ///
    /// Only a missing file leads to generation. A file that exists but is
    /// unreadable — wrong permissions, truncated, an unknown version — is
    /// reported as an error, because replacing it would lock out every client
    /// that paired against the old keys.
    ///
    /// Two daemons starting for the first time at once both see `Missing`, and
    /// both generate. [`Self::store_new`] lets exactly one of them create the
    /// file; the other gets `AlreadyExists` and adopts what the winner wrote,
    /// so both processes end up holding the same identity and the key that
    /// reaches disk is the one a client will pair against next.
    pub fn load_or_generate(path: &Path) -> anyhow::Result<Self> {
        // A `.tmp` left by an older build that wrote through a temporary file
        // is a complete private key sitting at its default permissions. Drop
        // it before anything else looks at this directory.
        remove_orphaned_temporary(path);
        match Self::load(path) {
            Ok(identity) => return Ok(identity),
            Err(LoadError::Missing) => {}
            Err(LoadError::Unreadable(error)) => return Err(error),
        }
        let identity = Self::generate();
        match identity.store_new(path) {
            Ok(()) => Ok(identity),
            // Another process claimed the name first. Its identity is the one
            // that counts; adopting it keeps both daemons consistent.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Self::load(path).map_err(|error| match error {
                    LoadError::Missing => anyhow!(
                        "the daemon identity at {} disappeared while another process was creating it",
                        path.display()
                    ),
                    LoadError::Unreadable(error) => error,
                })
            }
            Err(error) => Err(anyhow!(error)
                .context(format!("could not write the daemon identity to {}", path.display()))),
        }
    }

    /// Read an existing identity without ever creating one.
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(LoadError::Missing);
            }
            Err(error) => {
                return Err(LoadError::Unreadable(
                    anyhow!(error).context(format!("could not read {}", path.display())),
                ));
            }
        };
        Self::from_bytes(&bytes)
            .with_context(|| format!("could not load the daemon identity at {}", path.display()))
            .map_err(LoadError::Unreadable)
    }

    fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        let stored: StoredIdentity =
            serde_json::from_slice(bytes).context("the daemon identity file is not valid JSON")?;
        if stored.version != IDENTITY_VERSION {
            bail!(
                "daemon identity version {} is not supported; this build reads version {IDENTITY_VERSION}",
                stored.version
            );
        }
        let x25519 = decode_secret(&stored.x25519_secret, "x25519")?;
        let ed25519 = decode_secret(&stored.ed25519_secret, "ed25519")?;
        Ok(Self {
            x25519: StaticSecret::from(x25519),
            ed25519: SigningKey::from_bytes(&ed25519),
        })
    }

    fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        // `to_bytes` hands back the raw secret; those buffers are zeroed once
        // the JSON is built so the copies do not outlive this call.
        //
        // Known residual: the base64 strings inside `stored`, and the `Vec<u8>`
        // `serde_json` produces from them, hold the key material on the heap
        // and are dropped without being wiped. Closing that would mean a
        // custom `Serialize` that writes into a caller-owned zeroing buffer, or
        // a `secrecy`-style wrapper — disproportionate for a buffer that lives
        // for microseconds and is never logged. Accepted deliberately; the
        // same is true of the reply's bytes after the write returns, which is
        // why the caller treats them as short-lived.
        let mut x25519 = self.x25519.to_bytes();
        let mut ed25519 = self.ed25519.to_bytes();
        let stored = StoredIdentity {
            version: IDENTITY_VERSION,
            x25519_secret: base64::engine::general_purpose::STANDARD.encode(x25519),
            ed25519_secret: base64::engine::general_purpose::STANDARD.encode(ed25519),
        };
        zeroize::Zeroize::zeroize(&mut x25519);
        zeroize::Zeroize::zeroize(&mut ed25519);
        Ok(serde_json::to_vec(&stored)?)
    }

    /// Write the identity, creating the parent directory with owner-only
    /// permissions.
    ///
    /// The target file is claimed with `create_new`, which is `O_EXCL` on
    /// unix and `CREATE_NEW` on Windows: the kernel decides one winner and
    /// every other writer gets `AlreadyExists` instead of silently replacing
    /// the winner's key. That replaces the old write-tmp-then-rename dance,
    /// whose rename was an unconditional overwrite — two daemons starting for
    /// the first time would leave the loser's key on disk while its process
    /// kept the winner's key in memory.
    ///
    /// Every other daemon learns it lost by reading this call's
    /// `AlreadyExists`, and `load_or_generate` answers that by reading the
    /// winner's identity instead of overwriting it.
    fn store_new(&self, path: &Path) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            crate::fs_ext::create_private_dir_all(parent)?;
        }
        let data = self.to_bytes().map_err(std::io::Error::other)?;
        // The mode is set at creation, not after the write: a crash between
        // the two must not leave a world-readable copy of the private key
        // behind, and there is no window where the file is more permissive
        // than it ends up.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        let written = (|| {
            use std::io::Write as _;
            file.write_all(&data)?;
            // Durable before the caller trusts the file: a crash right after
            // this returns must not leave a zero-length key.
            file.sync_all()
        })();
        // A failed write must not leave a partial file where a future
        // `load_or_generate` would read it as corrupt. Removing on error also
        // releases the name so a retry can claim it again.
        if let Err(error) = written {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
        Ok(())
    }
}

/// Remove the temporary file an older build left when it crashed mid-write.
///
/// That build wrote `identity.key.tmp` and renamed it into place, so a crash
/// between create and rename left a **complete, valid private key** on disk —
/// and if the crash came before the permission fix, at whatever mode the
/// process umask allowed. The current writer never creates this file, so its
/// presence means a leftover, never work in progress.
///
/// Only the exact legacy name is removed: a `.tmp` this function invented
/// could belong to something else, and the whole point is not to delete files
/// that are not ours.
fn remove_orphaned_temporary(path: &Path) {
    let legacy = path.with_extension("key.tmp");
    if legacy == path {
        // `with_extension` produced the same path — nothing to do, and
        // removing it would destroy the identity itself.
        return;
    }
    match std::fs::remove_file(&legacy) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) => {}
        // A file that cannot be removed (a directory in the way, or a
        // permission problem) is not worth failing startup over: the identity
        // itself is what matters, and the leftover is reported by the
        // verification checklist rather than blocking the daemon.
        Err(_) => {}
    }
}

/// Why [`DaemonIdentity::load`] did not produce an identity.
///
/// The distinction is the whole point: only `Missing` may be answered by
/// generating a new key.
#[derive(Debug)]
pub enum LoadError {
    /// No file at that path. Safe to generate.
    Missing,
    /// A file is there but cannot be used. Never regenerate over it.
    Unreadable(anyhow::Error),
}

/// `~/.waku/identity.key`, beside the settings file the daemon already owns.
pub fn default_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".waku")
        .join(IDENTITY_FILE_NAME)
}

fn decode_secret(encoded: &str, label: &str) -> anyhow::Result<[u8; 32]> {
    let mut bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .with_context(|| format!("the daemon identity's {label} key is not valid base64"))?;
    let length = bytes.len();
    if length != 32 {
        zeroize::Zeroize::zeroize(&mut bytes);
        bail!("the daemon identity's {label} key is {length} bytes, expected 32");
    }
    let mut key = [0_u8; 32];
    key.copy_from_slice(&bytes);
    zeroize::Zeroize::zeroize(&mut bytes);
    Ok(key)
}

/// `OsRng` from the `rand_core` the dalek crates are built against (0.6).
/// Spelled out rather than re-exported so the version is visible at the call
/// site: a `rand_core` 0.9 `OsRng` is a different type and will not compile
/// here.
fn rand_core_os() -> impl rand_core::CryptoRngCore {
    rand_core::OsRng
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test gets its own directory under the system temp dir, removed on
    /// drop. Keys are never written outside it.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "waku-identity-{label}-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_generated_identity_round_trips_through_the_file() {
        let dir = TempDir::new("round-trip");
        let path = dir.join("identity.key");
        let generated = DaemonIdentity::load_or_generate(&path).unwrap();
        let expected_x25519 = generated.x25519_public();
        let expected_ed25519 = generated.ed25519_public();

        let loaded = DaemonIdentity::load_or_generate(&path).unwrap();

        assert_eq!(
            loaded.x25519_public().as_bytes(),
            expected_x25519.as_bytes()
        );
        assert_eq!(
            loaded.ed25519_public().as_bytes(),
            expected_ed25519.as_bytes()
        );
    }

    #[test]
    fn an_existing_identity_is_read_rather_than_replaced() {
        let dir = TempDir::new("existing");
        let path = dir.join("identity.key");
        let first = DaemonIdentity::load_or_generate(&path).unwrap();
        let first_public = first.ed25519_public().to_bytes();
        drop(first);

        // Loading twice must not mint a second identity.
        let second = DaemonIdentity::load_or_generate(&path).unwrap();
        assert_eq!(second.ed25519_public().to_bytes(), first_public);

        // And the file on disk still holds the first key.
        let stored = DaemonIdentity::load(&path).unwrap();
        assert_eq!(stored.ed25519_public().to_bytes(), first_public);
    }

    #[test]
    fn a_corrupt_file_is_an_error_and_is_left_in_place() {
        let dir = TempDir::new("corrupt");
        let path = dir.join("identity.key");
        std::fs::write(&path, b"{ not valid json").unwrap();
        let before = std::fs::read(&path).unwrap();

        let outcome = DaemonIdentity::load_or_generate(&path);

        assert!(
            outcome.is_err(),
            "a corrupt identity must never be replaced"
        );
        // The bytes are untouched, so a user can still recover them by hand.
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn an_unknown_version_is_refused() {
        let dir = TempDir::new("version");
        let path = dir.join("identity.key");
        std::fs::write(
            &path,
            br#"{"version":99,"x25519_secret":"AAAA","ed25519_secret":"AAAA"}"#,
        )
        .unwrap();

        let outcome = DaemonIdentity::load(&path);

        assert!(matches!(outcome, Err(LoadError::Unreadable(_))));
    }

    #[test]
    fn a_truncated_key_is_refused() {
        let dir = TempDir::new("short-key");
        let path = dir.join("identity.key");
        let short = base64::engine::general_purpose::STANDARD.encode([0_u8; 16]);
        std::fs::write(
            &path,
            format!(r#"{{"version":1,"x25519_secret":"{short}","ed25519_secret":"{short}"}}"#),
        )
        .unwrap();

        assert!(DaemonIdentity::load(&path).is_err());
    }

    #[test]
    fn a_missing_file_is_the_only_missing_outcome() {
        let dir = TempDir::new("missing");
        let path = dir.join("identity.key");

        assert!(matches!(
            DaemonIdentity::load(&path),
            Err(LoadError::Missing)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn the_identity_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = TempDir::new("mode");
        // A path whose parent does not exist yet, so `store` has to create the
        // directory itself — that is the branch that gives `~/.waku` its 0700.
        let nested = dir.0.join("nested");
        let path = nested.join("identity.key");
        DaemonIdentity::load_or_generate(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "identity.key must not be group or world readable"
        );

        let parent_mode = std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            parent_mode, 0o700,
            "the directory holding identity.key must be owner-only"
        );
    }

    #[test]
    fn generated_identities_differ() {
        let first = DaemonIdentity::generate();
        let second = DaemonIdentity::generate();
        assert_ne!(
            first.ed25519_public().to_bytes(),
            second.ed25519_public().to_bytes()
        );
        assert_ne!(
            first.x25519_public().as_bytes(),
            second.x25519_public().as_bytes()
        );
    }

    /// A crash of the old writer left a complete private key at
    /// `identity.key.tmp`. Loading must remove it rather than keep it next to
    /// the real key forever.
    #[test]
    fn an_orphaned_temporary_file_is_removed() {
        let dir = TempDir::new("orphan-tmp");
        let path = dir.join("identity.key");
        let legacy = path.with_extension("key.tmp");
        DaemonIdentity::load_or_generate(&path).unwrap();
        // Write it after the identity exists, which is exactly the state a
        // crash mid-replacement would leave behind.
        std::fs::write(&legacy, b"a complete private key left by a crash").unwrap();
        assert!(legacy.exists());

        let loaded = DaemonIdentity::load_or_generate(&path).unwrap();

        assert!(
            !legacy.exists(),
            "a leftover .tmp holding key material must not survive a load"
        );
        // The real identity is untouched by the cleanup.
        assert_eq!(
            loaded.ed25519_public().to_bytes(),
            DaemonIdentity::load(&path)
                .unwrap()
                .ed25519_public()
                .to_bytes()
        );
    }

    /// The loser of a first-start race must adopt the winner's identity rather
    /// than overwrite it.
    ///
    /// Reproducing the race with two threads would be flaky, so this stages the
    /// outcome instead: a file appears between the caller's "not found" check
    /// and its write attempt. That is the exact state `store_new`'s
    /// `create_new` rejects, and adopting is the only correct answer to it.
    #[test]
    fn a_lost_first_start_race_adopts_the_winner_identity() {
        let dir = TempDir::new("race");
        let path = dir.join("identity.key");
        // The "winner": whatever another process wrote first.
        let winner = DaemonIdentity::load_or_generate(&path).unwrap();
        let winner_public = winner.ed25519_public().to_bytes();

        // The "loser": generated, then told to store into a path that is now
        // taken. This is the code path `load_or_generate` takes when its own
        // `Missing` reading was already stale.
        let loser = DaemonIdentity::generate();
        assert_ne!(loser.ed25519_public().to_bytes(), winner_public);
        let conflict = loser.store_new(&path).unwrap_err();
        assert_eq!(conflict.kind(), std::io::ErrorKind::AlreadyExists);

        // `load_or_generate` on the same path returns the winner, not the
        // loser, and the file still holds the winner's key.
        let adopted = DaemonIdentity::load_or_generate(&path).unwrap();
        assert_eq!(adopted.ed25519_public().to_bytes(), winner_public);
        assert_eq!(
            DaemonIdentity::load(&path)
                .unwrap()
                .ed25519_public()
                .to_bytes(),
            winner_public,
            "the loser must not have replaced the winner's key on disk"
        );
    }

    /// `store_new` claims the name, so a second write to the same path fails
    /// instead of overwriting — the property the race test above depends on.
    #[test]
    fn storing_twice_never_overwrites() {
        let dir = TempDir::new("no-overwrite");
        let path = dir.join("identity.key");
        let first = DaemonIdentity::generate();
        first.store_new(&path).unwrap();
        let first_bytes = std::fs::read(&path).unwrap();

        let second = DaemonIdentity::generate();
        assert!(second.store_new(&path).is_err());

        assert_eq!(
            std::fs::read(&path).unwrap(),
            first_bytes,
            "a failed claim must leave the original file byte-for-byte intact"
        );
    }
}
