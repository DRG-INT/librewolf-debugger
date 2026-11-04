//! Local certificate authority for TLS interception.
//!
//! Capturing HTTPS means terminating TLS, which means a CA the browser trusts.
//! That is a real grant of power, not an implementation detail: anything
//! trusting this CA can have all of its traffic read. The CA key therefore
//! lives in one file, `0600`, and `lwdbg proxy ca uninstall` exists so the
//! grant can be withdrawn.

use anyhow::{Context, Result};
use dashmap::DashMap;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The CA, plus a memo table of leaf certs already minted for a host.
pub struct Ca {
    /// The PEM as stored on disk and installed into trust stores. Leaves are
    /// validated against *this*, so it is the copy we hand out.
    ca_cert_pem: String,
    ca_key_pem: String,
    /// Issuer handle for signing. Re-derived from the stored PEM: `rcgen` can
    /// only sign with a `Certificate` it constructed, and it reads just the DN,
    /// key-identifier method and key usages from it. Since the key pair and DN
    /// are unchanged, leaves still chain to the installed CA.
    ca_issuer: Certificate,
    ca_key: KeyPair,
    leaves: DashMap<String, Arc<CertifiedKey>>,
}

pub struct CertifiedKey {
    pub cert_pem: String,
    pub key_pem: String,
}

const CA_COMMON_NAME: &str = "lwdbg local debugging CA";

impl Ca {
    /// Load the CA from `dir`, generating it on first use.
    pub fn load_or_create(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        let cert_path = dir.join("ca-cert.pem");
        let key_path = dir.join("ca-key.pem");

        let (ca_cert_pem, ca_key_pem) = if cert_path.exists() && key_path.exists() {
            (
                std::fs::read_to_string(&cert_path)?,
                std::fs::read_to_string(&key_path)?,
            )
        } else {
            let (c, k) = Self::generate()?;
            std::fs::write(&cert_path, &c)?;
            std::fs::write(&key_path, &k)?;
            restrict_permissions(&key_path)?;
            (c, k)
        };

        let ca_key = KeyPair::from_pem(&ca_key_pem).context("parsing CA key")?;
        let params = CertificateParams::from_ca_cert_pem(&ca_cert_pem)
            .context("recovering CA params from stored PEM")?;
        let ca_issuer = params
            .self_signed(&ca_key)
            .context("rebuilding CA issuer handle")?;

        Ok(Self {
            ca_cert_pem,
            ca_key_pem,
            ca_issuer,
            ca_key,
            leaves: DashMap::new(),
        })
    }

    fn ca_params() -> Result<CertificateParams> {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, CA_COMMON_NAME);
        dn.push(DnType::OrganizationName, "lwdbg");

        let mut p = CertificateParams::default();
        p.distinguished_name = dn;
        // A path length of zero: this CA may sign leaves, never another CA.
        // It keeps a leaked key from being used to mint intermediates.
        p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        p.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        Ok(p)
    }

    fn generate() -> Result<(String, String)> {
        let key = KeyPair::generate().context("generating CA key")?;
        let params = Self::ca_params()?;
        let cert = params.self_signed(&key).context("self-signing CA")?;
        Ok((cert.pem(), key.serialize_pem()))
    }

    pub fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    pub fn ca_key_pem(&self) -> &str {
        &self.ca_key_pem
    }

    /// BLAKE3 of the DER-ish PEM body, used to tell whether an installed CA is
    /// the one this process holds.
    pub fn fingerprint(&self) -> String {
        blake3::hash(self.ca_cert_pem.as_bytes())
            .to_hex()
            .to_string()
    }

    /// Mint (and memoise) a leaf certificate for `host`.
    pub fn leaf_for(&self, host: &str) -> Result<Arc<CertifiedKey>> {
        if let Some(found) = self.leaves.get(host) {
            return Ok(found.clone());
        }

        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, host);

        let mut p = CertificateParams::default();
        p.distinguished_name = dn;
        p.subject_alt_names = vec![san_for(host)?];

        let leaf_key = KeyPair::generate().context("generating leaf key")?;
        let cert = p
            .signed_by(&leaf_key, &self.ca_issuer, &self.ca_key)
            .with_context(|| format!("signing leaf for {host}"))?;

        let ck = Arc::new(CertifiedKey {
            cert_pem: cert.pem(),
            key_pem: leaf_key.serialize_pem(),
        });
        self.leaves.insert(host.to_string(), ck.clone());
        Ok(ck)
    }
}

/// An IP literal must go in as an IP SAN; a DNS name as a DNS SAN. Browsers
/// reject a certificate that puts an address in a DNS SAN, so this is not a
/// cosmetic distinction.
fn san_for(host: &str) -> Result<SanType> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        Ok(SanType::IpAddress(ip))
    } else {
        Ok(SanType::DnsName(host.to_string().try_into().map_err(
            |_| anyhow::anyhow!("host {host} is not a valid DNS name"),
        )?))
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &PathBuf) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &PathBuf) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test directory. Tests run in parallel, so a shared path lets one
    /// test delete the CA another is mid-way through loading.
    fn tmpdir(tag: &str) -> PathBuf {
        // Test helper. The name carries the pid and the directory is
        // removed first; nothing secret is written here, and a test that
        // could not use a temp dir would need a fixture in the repo.
        // nosemgrep: rust.lang.security.temp-dir.temp-dir
        let d = std::env::temp_dir().join(format!("lwdbg-ca-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&d).ok();
        d
    }

    #[test]
    fn ca_is_stable_across_loads() {
        let d = tmpdir("stable");
        let a = Ca::load_or_create(&d).unwrap();
        let fa = a.fingerprint();
        drop(a);
        let b = Ca::load_or_create(&d).unwrap();
        assert_eq!(fa, b.fingerprint(), "reloading must not re-generate the CA");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn ca_key_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir("perms");
        let _ = Ca::load_or_create(&d).unwrap();
        let mode = std::fs::metadata(d.join("ca-key.pem"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "CA key must be 0600, was {mode:o}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn leaves_are_memoised_and_host_specific() {
        let d = tmpdir("leaves");
        let ca = Ca::load_or_create(&d).unwrap();
        let a1 = ca.leaf_for("example.com").unwrap();
        let a2 = ca.leaf_for("example.com").unwrap();
        let b = ca.leaf_for("other.example").unwrap();
        assert!(Arc::ptr_eq(&a1, &a2), "same host should hit the memo table");
        assert_ne!(a1.cert_pem, b.cert_pem);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn ip_hosts_get_an_ip_san_not_a_dns_san() {
        match san_for("127.0.0.1").unwrap() {
            SanType::IpAddress(_) => {}
            other => panic!("expected IP SAN, got {other:?}"),
        }
        match san_for("neptunium.gd").unwrap() {
            SanType::DnsName(_) => {}
            other => panic!("expected DNS SAN, got {other:?}"),
        }
    }
}
