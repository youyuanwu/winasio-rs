// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! A self-signed certificate for tests, built on the `windows` crate alone.
//!
//! HTTPS needs a server certificate. This module creates one — with a persisted
//! private key HTTP.sys can read — installs it into a system store, and, on drop,
//! removes both the certificate and its key container. It exists so the test
//! suite can provision TLS without OpenSSL, `schannel`, or `rcgen`: every step is
//! a direct call into `Win32_Security_Cryptography`, which is why the whole module
//! is gated behind the `test-util` feature and never enters the default build.
//!
//! # D1. Why the CNG key is created up front, named and persisted
//!
//! HTTP.sys presents the certificate itself, so it must be able to find the
//! *private* key later, by name, as a machine principal — an ephemeral key would
//! be gone by then. The key is therefore a named, persisted CNG key. It is also
//! created **before** [`CertCreateSelfSignCertificate`] rather than letting that
//! call generate one: measured, passing a `pKeyProvInfo` that names a container
//! whose key does not yet exist fails `NTE_BAD_KEYSET` (0x80090016) — the call
//! only auto-generates a key when `pKeyProvInfo` is null. So the sequence is
//! create-key, then self-sign against the key handle.
//!
//! # D2. Why a Subject Alternative Name is included
//!
//! The certificate is issued for `localhost`, and modern TLS stacks increasingly
//! reject a certificate that carries the name only in the legacy Common Name.
//! A `DNS:localhost` SAN is therefore added through `pExtensions`, so that a
//! client relaxing only "unknown certificate authority" — not host-name checking
//! — still validates the name. What relaxations are actually required is measured
//! by the end-to-end suite; the SAN is included so name validation is not the
//! thing that fails.
//!
//! # D3. Why the key scope must match the store
//!
//! A binding lives in `LocalMachine\My` and is served by HTTP.sys running as
//! SYSTEM; a key in the *current user's* profile is not readable there. So a
//! [`CertStore::LocalMachine`] certificate gets a **machine** key
//! (`NCRYPT_MACHINE_KEY_FLAG`) and its provider info carries
//! `CRYPT_MACHINE_KEYSET`, while a [`CertStore::CurrentUser`] certificate — used
//! by the always-on roundtrip test, which needs no elevation — gets a user key.
//! The two never share a container namespace, so cleanup scopes its key search
//! the same way.
//!
//! # Rejected alternatives
//!
//! * **OpenSSL / `schannel` / `rcgen`.** Excluded by decision: the workspace has
//!   no external runtime dependency and generates certificates through the
//!   platform directly. Adding any of them to produce a test certificate would
//!   reverse that for the whole crate.
//! * **Sweeping leftovers by querying the AppId.** HTTP.sys keys SSL
//!   configuration by `ip:port`, not by AppId, so there is no AppId-indexed query
//!   to enumerate. Cleanup is instead keyed by the things this crate owns and
//!   knows: its reserved port set, the store it installs into, and the
//!   `winasio-tls-` container-name prefix. That is deterministic where an AppId
//!   sweep would be guesswork.
//!
//! # Invariants and obligations
//!
//! * **Drop removes everything it created.** On both the normal and the
//!   panicking path, dropping a [`SelfSignedCert`] deletes the certificate from
//!   its store and deletes the CNG key container. A leaked key container or a
//!   leaked machine certificate is a machine-wide residue, so cleanup is not
//!   best-effort *for the instance's own state* — it always runs.
//! * **Each instance owns a unique container.** The container name embeds the
//!   process id and a per-process counter under the `winasio-tls-` prefix, so two
//!   concurrent certificates — in the same or different processes — never share a
//!   key.
//! * **A `LocalMachine` certificate requires elevation.** Creating a machine key
//!   and writing `LocalMachine\My` both need administrator rights; unelevated,
//!   [`SelfSignedCert::create`] fails. Callers gate on elevation (see the test
//!   suite's `is_elevated`).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::core::{Error, Result, PCSTR, PCWSTR, PSTR, PWSTR};
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::Cryptography::{
    szOID_RSA_SHA256RSA, szOID_SUBJECT_ALT_NAME2, CertAddCertificateContextToStore, CertCloseStore,
    CertCreateSelfSignCertificate, CertDeleteCertificateFromStore, CertEnumCertificatesInStore,
    CertFindCertificateInStore, CertFreeCertificateContext, CertGetCertificateContextProperty,
    CertOpenStore, CertStrToNameW, CryptEncodeObjectEx, NCryptCreatePersistedKey, NCryptDeleteKey,
    NCryptEnumKeys, NCryptFinalizeKey, NCryptFreeBuffer, NCryptFreeObject, NCryptKeyName,
    NCryptOpenKey, NCryptOpenStorageProvider, NCryptSetProperty, CERT_ALT_NAME_ENTRY,
    CERT_ALT_NAME_ENTRY_0, CERT_ALT_NAME_INFO, CERT_CONTEXT, CERT_CREATE_SELFSIGN_FLAGS,
    CERT_EXTENSION, CERT_EXTENSIONS, CERT_FIND_HASH, CERT_HASH_PROP_ID, CERT_KEY_PROV_INFO_PROP_ID,
    CERT_KEY_SPEC, CERT_OID_NAME_STR, CERT_OPEN_STORE_FLAGS, CERT_QUERY_ENCODING_TYPE,
    CERT_STORE_ADD_REPLACE_EXISTING, CERT_STORE_PROV_SYSTEM_REGISTRY_W,
    CERT_SYSTEM_STORE_CURRENT_USER, CERT_SYSTEM_STORE_LOCAL_MACHINE, CRYPT_ALGORITHM_IDENTIFIER,
    CRYPT_ENCODE_OBJECT_FLAGS, CRYPT_INTEGER_BLOB, CRYPT_KEY_FLAGS, CRYPT_KEY_PROV_INFO,
    CRYPT_MACHINE_KEYSET, HCERTSTORE, HCRYPTPROV_OR_NCRYPT_KEY_HANDLE, NCRYPT_FLAGS, NCRYPT_HANDLE,
    NCRYPT_KEY_HANDLE, NCRYPT_MACHINE_KEY_FLAG, NCRYPT_PROV_HANDLE, X509_ALTERNATE_NAME,
    X509_ASN_ENCODING,
};
use windows::Win32::Security::{GetSecurityDescriptorLength, PSECURITY_DESCRIPTOR};

use super::THUMBPRINT_LEN;

/// The container-name prefix every key this module creates carries, so cleanup
/// can recognise its own containers among all of a provider's keys.
const CONTAINER_PREFIX: &str = "winasio-tls-";

/// The CNG storage provider the keys live in.
const PROVIDER: &str = "Microsoft Software Key Storage Provider";

/// The system store certificates are installed into, spelled as `CertOpenStore`
/// and HTTP.sys expect it.
const STORE_NAME: &str = "My";

/// `NCRYPT_OVERWRITE_KEY_FLAG` — replace a same-named container rather than fail.
const NCRYPT_OVERWRITE_KEY_FLAG: u32 = 0x0000_0080;

/// `CERT_NCRYPT_KEY_SPEC` — the key lives in CNG, not legacy CAPI.
const CERT_NCRYPT_KEY_SPEC: u32 = 0xFFFF_FFFF;

/// `CERT_ALT_NAME_DNS_NAME` — the `dwAltNameChoice` selecting `pwszDNSName`.
const CERT_ALT_NAME_DNS_NAME: u32 = 3;

/// Which system store a certificate is installed into.
///
/// The choice also fixes the key scope (see module docs D3) and, for cleanup,
/// which store and key namespace a sweep looks in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertStore {
    /// `CurrentUser\My`. Writable without elevation; the key is a user key.
    CurrentUser,
    /// `LocalMachine\My`. Requires elevation; the key is a machine key readable
    /// by HTTP.sys (running as SYSTEM).
    LocalMachine,
}

impl CertStore {
    /// The `CERT_SYSTEM_STORE_*` location flag for `CertOpenStore`.
    fn system_store_flag(self) -> u32 {
        match self {
            CertStore::CurrentUser => CERT_SYSTEM_STORE_CURRENT_USER,
            CertStore::LocalMachine => CERT_SYSTEM_STORE_LOCAL_MACHINE,
        }
    }

    /// Whether keys for this store are machine-scoped.
    fn is_machine(self) -> bool {
        matches!(self, CertStore::LocalMachine)
    }

    /// The extra `NCRYPT_*` flags a key in this store's scope needs.
    fn ncrypt_scope_flags(self) -> u32 {
        if self.is_machine() {
            NCRYPT_MACHINE_KEY_FLAG.0
        } else {
            0
        }
    }
}

/// Encode a `&str` as a NUL-terminated UTF-16 buffer.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A NUL-terminated ASCII copy of a `PCSTR` OID, owned so it can be handed to an
/// API wanting a mutable `PSTR`.
fn oid_bytes(oid: PCSTR) -> Vec<u8> {
    // SAFETY: `oid` is a static NUL-terminated string constant from `windows`.
    let mut bytes = unsafe { oid.as_bytes() }.to_vec();
    bytes.push(0);
    bytes
}

/// Whether a certificate with `thumbprint` is currently installed in `store`.
///
/// A `test-util` helper for verifying installation and, after a
/// [`SelfSignedCert`] drops, that its certificate is gone.
pub fn cert_present(thumbprint: &[u8; THUMBPRINT_LEN], store: CertStore) -> bool {
    let store_w = wide(STORE_NAME);
    // SAFETY: the store is opened and closed here; a found context is freed.
    unsafe {
        let Ok(hstore) = CertOpenStore(
            CERT_STORE_PROV_SYSTEM_REGISTRY_W,
            CERT_QUERY_ENCODING_TYPE(0),
            None,
            CERT_OPEN_STORE_FLAGS(store.system_store_flag()),
            Some(store_w.as_ptr() as *const c_void),
        ) else {
            return false;
        };
        let hash_blob = CRYPT_INTEGER_BLOB {
            cbData: THUMBPRINT_LEN as u32,
            pbData: thumbprint.as_ptr() as *mut u8,
        };
        let found = CertFindCertificateInStore(
            hstore,
            X509_ASN_ENCODING,
            0,
            CERT_FIND_HASH,
            Some(&hash_blob as *const _ as *const c_void),
            None,
        );
        let present = !found.is_null();
        if present {
            let _ = CertFreeCertificateContext(Some(found));
        }
        let _ = CertCloseStore(Some(hstore), 0);
        present
    }
}

/// Whether a CNG key container named `container` exists in `store`'s scope.
///
/// A `test-util` helper for verifying that a [`SelfSignedCert`]'s key container
/// is gone after drop.
pub fn key_container_present(container: &str, store: CertStore) -> bool {
    let provider_w = wide(PROVIDER);
    let container_w = wide(container);
    // SAFETY: provider and key handles are opened and freed here.
    unsafe {
        let mut prov = NCRYPT_PROV_HANDLE::default();
        if NCryptOpenStorageProvider(&mut prov, PCWSTR(provider_w.as_ptr()), 0).is_err() {
            return false;
        }
        let prov = ProvHandle(prov);
        let mut key = NCRYPT_KEY_HANDLE::default();
        let opened = NCryptOpenKey(
            prov.0,
            &mut key,
            PCWSTR(container_w.as_ptr()),
            CERT_KEY_SPEC(0),
            NCRYPT_FLAGS(store.ncrypt_scope_flags()),
        )
        .is_ok();
        if opened {
            let _ = NCryptFreeObject(NCRYPT_HANDLE(key.0));
        }
        opened
    }
}

/// A self-signed certificate installed in a system store, with cleanup on drop.
///
/// Created by [`SelfSignedCert::create`]. Holds the certificate context and the
/// name of the CNG key container backing it; dropping the value removes the
/// certificate from its store and deletes the key container (module docs
/// "Invariants and obligations").
pub struct SelfSignedCert {
    /// The created certificate context. Non-null for the lifetime of the value.
    cert: *mut CERT_CONTEXT,
    /// SHA-1 thumbprint, the identity HTTP.sys binds by.
    thumbprint: [u8; THUMBPRINT_LEN],
    /// The key container name (owned, for cleanup).
    container: String,
    /// The store the certificate was installed into.
    store: CertStore,
    /// The X.500 subject, for diagnostics.
    subject: String,
}

impl SelfSignedCert {
    /// Create a self-signed certificate for `subject` (a full X.500 string such
    /// as `"CN=localhost"`) and install it into `store`.
    ///
    /// The certificate carries a `DNS:localhost` SAN and a persisted key scoped
    /// to the store (see module docs). Returns the installed certificate as a
    /// guard whose drop cleans up.
    ///
    /// # Errors
    ///
    /// The underlying `windows::core::Error` on any step. A
    /// [`CertStore::LocalMachine`] certificate needs elevation; unelevated, key
    /// creation or the store write fails.
    pub fn create(subject: &str, store: CertStore) -> Result<SelfSignedCert> {
        let container = unique_container_name();
        // SAFETY: each call's pointer arguments reference locals that outlive the
        // synchronous call; two-pass sizing is honoured; the key handle created
        // here is finalised before use and freed before return. Individual steps
        // are annotated inline.
        unsafe {
            // 1. Encode the subject as an X.500 name blob (two-pass sizing).
            let subject_w = wide(subject);
            let mut name_len = 0u32;
            CertStrToNameW(
                X509_ASN_ENCODING,
                PCWSTR(subject_w.as_ptr()),
                CERT_OID_NAME_STR,
                None,
                None,
                &mut name_len,
                None,
            )?;
            let mut name_blob = vec![0u8; name_len as usize];
            CertStrToNameW(
                X509_ASN_ENCODING,
                PCWSTR(subject_w.as_ptr()),
                CERT_OID_NAME_STR,
                None,
                Some(name_blob.as_mut_ptr()),
                &mut name_len,
                None,
            )?;
            let subject_blob = CRYPT_INTEGER_BLOB {
                cbData: name_len,
                pbData: name_blob.as_mut_ptr(),
            };

            // 2. Encode the DNS:localhost SAN extension (see D2) up front. This
            //    is pure in-memory work independent of the key, so doing it
            //    before the key container exists means an encode failure can
            //    never orphan a container. The buffers below must outlive the
            //    CertCreateSelfSignCertificate call.
            let dns_w = wide("localhost");
            let alt_entry = CERT_ALT_NAME_ENTRY {
                dwAltNameChoice: CERT_ALT_NAME_DNS_NAME,
                Anonymous: CERT_ALT_NAME_ENTRY_0 {
                    pwszDNSName: PWSTR(dns_w.as_ptr() as *mut u16),
                },
            };
            let alt_info = CERT_ALT_NAME_INFO {
                cAltEntry: 1,
                rgAltEntry: &alt_entry as *const _ as *mut _,
            };
            let mut san_len = 0u32;
            CryptEncodeObjectEx(
                X509_ASN_ENCODING,
                X509_ALTERNATE_NAME,
                &alt_info as *const _ as *const c_void,
                CRYPT_ENCODE_OBJECT_FLAGS(0),
                None,
                None,
                &mut san_len,
            )?;
            let mut san_encoded = vec![0u8; san_len as usize];
            CryptEncodeObjectEx(
                X509_ASN_ENCODING,
                X509_ALTERNATE_NAME,
                &alt_info as *const _ as *const c_void,
                CRYPT_ENCODE_OBJECT_FLAGS(0),
                None,
                Some(san_encoded.as_mut_ptr() as *mut c_void),
                &mut san_len,
            )?;
            let mut san_oid = oid_bytes(szOID_SUBJECT_ALT_NAME2);
            let san_ext = CERT_EXTENSION {
                pszObjId: PSTR(san_oid.as_mut_ptr()),
                fCritical: false.into(),
                Value: CRYPT_INTEGER_BLOB {
                    cbData: san_len,
                    pbData: san_encoded.as_mut_ptr(),
                },
            };
            let extensions = CERT_EXTENSIONS {
                cExtension: 1,
                rgExtension: &san_ext as *const _ as *mut _,
            };

            // 3. Create a named, persisted CNG key up front (see D1). Its scope
            //    matches the store (see D3). Everything past this point that can
            //    fail must roll the container back.
            let container_w = wide(&container);
            let provider_w = wide(PROVIDER);
            let mut prov = NCRYPT_PROV_HANDLE::default();
            NCryptOpenStorageProvider(&mut prov, PCWSTR(provider_w.as_ptr()), 0)?;
            let prov = ProvHandle(prov);

            let mut key = NCRYPT_KEY_HANDLE::default();
            let algo_w = wide("RSA");
            let create_flags = NCRYPT_FLAGS(NCRYPT_OVERWRITE_KEY_FLAG | store.ncrypt_scope_flags());
            NCryptCreatePersistedKey(
                prov.0,
                &mut key,
                PCWSTR(algo_w.as_ptr()),
                PCWSTR(container_w.as_ptr()),
                CERT_KEY_SPEC(0),
                create_flags,
            )?;
            let bits: u32 = 2048;
            let length_prop = wide("Length");
            // The persisted key handle (and, after finalize, its container) now
            // exists. Any failure before the handle is duplicated into the
            // cert's provider info must free the handle AND delete the container,
            // matching the rollback ladder used for every later step; a bare `?`
            // here would orphan machine-global CNG state.
            if let Err(e) = NCryptSetProperty(
                NCRYPT_HANDLE(key.0),
                PCWSTR(length_prop.as_ptr()),
                &bits.to_ne_bytes(),
                NCRYPT_FLAGS(0),
            ) {
                let _ = NCryptFreeObject(NCRYPT_HANDLE(key.0));
                delete_key_container(&container, store);
                return Err(e);
            }
            if let Err(e) = NCryptFinalizeKey(key, NCRYPT_FLAGS(0)) {
                let _ = NCryptFreeObject(NCRYPT_HANDLE(key.0));
                delete_key_container(&container, store);
                return Err(e);
            }

            // Grant the private key's DACL to the well-known service contexts.
            // HTTP.sys validates an SSL binding by acquiring the certificate's
            // private key; on a machine key whose default DACL does not admit
            // that context, `HttpSetServiceConfiguration` fails at bind time with
            // ERROR_NO_SUCH_LOGON_SESSION (0x80070520) — measured on an elevated
            // CI runner, invisible on an unelevated dev host that skips the bind.
            // Applied to both scopes so the unelevated CurrentUser round-trip
            // test exercises the same path.
            if let Err(e) = grant_key_access(NCRYPT_HANDLE(key.0)) {
                let _ = NCryptFreeObject(NCRYPT_HANDLE(key.0));
                delete_key_container(&container, store);
                return Err(e);
            }

            // 4. Provider info naming the container. For a machine key, set
            //    CRYPT_MACHINE_KEYSET so it resolves the machine-scoped container.
            let prov_info_flags = if store.is_machine() {
                CRYPT_MACHINE_KEYSET
            } else {
                CRYPT_KEY_FLAGS(0)
            };
            let key_info = CRYPT_KEY_PROV_INFO {
                pwszContainerName: PWSTR(container_w.as_ptr() as *mut u16),
                pwszProvName: PWSTR(provider_w.as_ptr() as *mut u16),
                dwProvType: 0,
                dwFlags: prov_info_flags,
                cProvParam: 0,
                rgProvParam: std::ptr::null_mut(),
                dwKeySpec: CERT_NCRYPT_KEY_SPEC,
            };

            // SHA-256 signature algorithm.
            let mut sig_oid = oid_bytes(szOID_RSA_SHA256RSA);
            let sig_algo = CRYPT_ALGORITHM_IDENTIFIER {
                pszObjId: PSTR(sig_oid.as_mut_ptr()),
                Parameters: CRYPT_INTEGER_BLOB::default(),
            };

            // Hand over the key handle we created; null return ⇒ thread error.
            let cert = CertCreateSelfSignCertificate(
                Some(HCRYPTPROV_OR_NCRYPT_KEY_HANDLE(key.0)),
                &subject_blob,
                CERT_CREATE_SELFSIGN_FLAGS(0),
                Some(&key_info),
                Some(&sig_algo),
                None,
                None,
                Some(&extensions),
            );
            // The key handle is duplicated into the cert's provider info; the
            // container persists, so the transient handle can be freed now.
            let _ = NCryptFreeObject(NCRYPT_HANDLE(key.0));
            if cert.is_null() {
                // Roll back the orphaned key container before returning.
                delete_key_container(&container, store);
                return Err(Error::from_thread());
            }

            // 5. Read the SHA-1 thumbprint. Done before store installation so a
            //    failure here only has to roll back the cert context and key
            //    container, not a store copy.
            let mut thumbprint = [0u8; THUMBPRINT_LEN];
            let mut hash_len = THUMBPRINT_LEN as u32;
            if let Err(e) = CertGetCertificateContextProperty(
                cert,
                CERT_HASH_PROP_ID,
                Some(thumbprint.as_mut_ptr() as *mut c_void),
                &mut hash_len,
            ) {
                let _ = CertFreeCertificateContext(Some(cert));
                delete_key_container(&container, store);
                return Err(e);
            }

            // 6. Install into the store.
            let store_w = wide(STORE_NAME);
            let hstore = match CertOpenStore(
                CERT_STORE_PROV_SYSTEM_REGISTRY_W,
                CERT_QUERY_ENCODING_TYPE(0),
                None,
                CERT_OPEN_STORE_FLAGS(store.system_store_flag()),
                Some(store_w.as_ptr() as *const c_void),
            ) {
                Ok(s) => s,
                Err(e) => {
                    let _ = CertFreeCertificateContext(Some(cert));
                    delete_key_container(&container, store);
                    return Err(e);
                }
            };
            let add = CertAddCertificateContextToStore(
                Some(hstore),
                cert,
                CERT_STORE_ADD_REPLACE_EXISTING,
                None,
            );
            let _ = CertCloseStore(Some(hstore), 0);
            if let Err(e) = add {
                let _ = CertFreeCertificateContext(Some(cert));
                delete_key_container(&container, store);
                return Err(e);
            }

            Ok(SelfSignedCert {
                cert,
                thumbprint,
                container,
                store,
                subject: subject.to_string(),
            })
        }
    }

    /// The certificate's SHA-1 thumbprint — the value bound to an endpoint.
    pub fn thumbprint(&self) -> [u8; THUMBPRINT_LEN] {
        self.thumbprint
    }

    /// The store name to pass to
    /// [`bind_ssl_certificate`](super::bind_ssl_certificate) for this
    /// certificate, spelled as HTTP.sys expects (`"MY"`).
    ///
    /// Returns `Some("MY")` only for a [`CertStore::LocalMachine`] certificate.
    /// HTTP.sys resolves the SSL table's store name in the machine namespace, so
    /// a [`CertStore::CurrentUser`] certificate cannot be bound and yields
    /// `None` rather than a name that would produce an unresolvable binding.
    pub fn store_name(&self) -> Option<&'static str> {
        matches!(self.store, CertStore::LocalMachine).then_some("MY")
    }

    /// The store the certificate is installed in.
    pub fn store(&self) -> CertStore {
        self.store
    }

    /// The X.500 subject, for diagnostics.
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// The CNG key container name, for diagnostics and cleanup checks.
    pub fn container(&self) -> &str {
        &self.container
    }

    /// Best-effort removal of this crate's leftover state before a test starts.
    ///
    /// Idempotent and non-failing: any error is ignored, because a leftover that
    /// cannot be removed should not fail the test that is trying to start clean.
    /// Scoped precisely to what this crate owns — the caller's `store`, the
    /// `winasio-tls-` container prefix, and the passed `ports` — so a concurrent
    /// test *process* using a different store cannot have its live certificate
    /// deleted (the process-local serialisation `Mutex` does not cross
    /// processes). `ports` is the reserved `ip:port` set to clear stale SSL
    /// bindings from (both the `0.0.0.0` and `[::]` families); pass an empty
    /// slice when the caller owns no bindings. Deleting bindings needs a live
    /// [`HttpInitializer`](super::HttpInitializer).
    pub fn sweep_leftovers(store: CertStore, ports: &[u16]) {
        // 1. Stale SSL bindings on the reserved endpoints (both families).
        for &port in ports {
            for ip in ["0.0.0.0", "[::]"] {
                if let Ok(endpoint) = format!("{ip}:{port}").parse() {
                    let _ = super::ssl::delete_binding(endpoint);
                }
            }
        }
        // 2. Certificates in the caller's store whose key container carries our
        //    prefix.
        sweep_store_certificates(store);
        // 3. Orphaned key containers with our prefix (a key created before a
        //    panic that aborted before cert install is invisible to step 2).
        sweep_key_containers(store);
    }
}

impl Drop for SelfSignedCert {
    fn drop(&mut self) {
        // SAFETY: `self.cert` is the non-null context created in `create`; the
        // store handle is opened and closed within this call. Errors are ignored
        // (a drop cannot act on them) but the deletions are attempted on every
        // path, including unwinding, so nothing this instance created leaks.
        unsafe {
            let store_w = wide(STORE_NAME);
            if let Ok(hstore) = CertOpenStore(
                CERT_STORE_PROV_SYSTEM_REGISTRY_W,
                CERT_QUERY_ENCODING_TYPE(0),
                None,
                CERT_OPEN_STORE_FLAGS(self.store.system_store_flag()),
                Some(store_w.as_ptr() as *const c_void),
            ) {
                delete_cert_by_thumbprint(hstore, &self.thumbprint);
                let _ = CertCloseStore(Some(hstore), 0);
            }
            let _ = CertFreeCertificateContext(Some(self.cert));
        }
        delete_key_container(&self.container, self.store);
    }
}

/// A CNG provider handle freed on drop.
struct ProvHandle(NCRYPT_PROV_HANDLE);

impl Drop for ProvHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a provider handle from `NCryptOpenStorageProvider`,
        // freed exactly once here.
        unsafe {
            let _ = NCryptFreeObject(NCRYPT_HANDLE(self.0 .0));
        }
    }
}

/// A unique container name, distinct across concurrent instances and processes.
fn unique_container_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{CONTAINER_PREFIX}{pid}-{n}-{nanos}")
}

/// Grant the CNG key's private-key DACL to the well-known service contexts so
/// HTTP.sys can acquire the private key when it validates an SSL binding.
///
/// # Safety
///
/// `key` must be a valid, finalized CNG key handle.
unsafe fn grant_key_access(key: NCRYPT_HANDLE) -> Result<()> {
    // Grant GENERIC_ALL to Everyone (WD) and SYSTEM (SY). Everyone-full on an
    // ephemeral, drop-deleted self-signed test key is acceptable for a
    // `test-util` helper and guarantees both that HTTP.sys's validation context
    // can acquire the private key (fixing the bind-time
    // ERROR_NO_SUCH_LOGON_SESSION) and that the creating context retains the
    // delete rights cleanup needs. A protected DACL (`D:P`) so no inherited
    // deny entry can override it.
    let sddl = wide("D:P(A;;GA;;;WD)(A;;GA;;;SY)");
    let mut psd = PSECURITY_DESCRIPTOR::default();
    ConvertStringSecurityDescriptorToSecurityDescriptorW(
        PCWSTR(sddl.as_ptr()),
        SDDL_REVISION_1,
        &mut psd,
        None,
    )?;
    // The self-relative descriptor's bytes, passed verbatim to NCrypt.
    let len = GetSecurityDescriptorLength(psd) as usize;
    let bytes = std::slice::from_raw_parts(psd.0 as *const u8, len);
    // Property name "Security Descr"; the flags carry the SECURITY_INFORMATION
    // bits — DACL_SECURITY_INFORMATION (0x4).
    let prop = wide("Security Descr");
    let res = NCryptSetProperty(key, PCWSTR(prop.as_ptr()), bytes, NCRYPT_FLAGS(4));
    // The descriptor was LocalAlloc'd by the conversion; free it.
    let _ = LocalFree(Some(HLOCAL(psd.0)));
    res
}

/// Delete the CNG key container named `container` in `store`'s scope,
/// ignoring a miss.
fn delete_key_container(container: &str, store: CertStore) {
    let provider_w = wide(PROVIDER);
    let container_w = wide(container);
    // SAFETY: all pointers reference locals alive for the calls; handles are
    // opened and freed here; every result is deliberately ignored.
    unsafe {
        let mut prov = NCRYPT_PROV_HANDLE::default();
        if NCryptOpenStorageProvider(&mut prov, PCWSTR(provider_w.as_ptr()), 0).is_err() {
            return;
        }
        let prov = ProvHandle(prov);
        let mut key = NCRYPT_KEY_HANDLE::default();
        if NCryptOpenKey(
            prov.0,
            &mut key,
            PCWSTR(container_w.as_ptr()),
            CERT_KEY_SPEC(0),
            NCRYPT_FLAGS(store.ncrypt_scope_flags()),
        )
        .is_ok()
        {
            // NCryptDeleteKey frees the handle only on success; free it
            // ourselves on failure so a persistently-failing delete cannot leak
            // one handle per sweep iteration.
            if NCryptDeleteKey(key, 0).is_err() {
                let _ = NCryptFreeObject(NCRYPT_HANDLE(key.0));
            }
        }
    }
}

/// Delete the certificate matching `thumbprint` from an open store, ignoring a
/// miss.
///
/// # Safety
///
/// `hstore` must be an open certificate store handle.
unsafe fn delete_cert_by_thumbprint(hstore: HCERTSTORE, thumbprint: &[u8; THUMBPRINT_LEN]) {
    let hash_blob = CRYPT_INTEGER_BLOB {
        cbData: THUMBPRINT_LEN as u32,
        pbData: thumbprint.as_ptr() as *mut u8,
    };
    let found = CertFindCertificateInStore(
        hstore,
        X509_ASN_ENCODING,
        0,
        CERT_FIND_HASH,
        Some(&hash_blob as *const _ as *const c_void),
        None,
    );
    if !found.is_null() {
        // CertDeleteCertificateFromStore frees the context it is given.
        let _ = CertDeleteCertificateFromStore(found);
    }
}

/// Delete every certificate in `store` whose key container carries our prefix.
fn sweep_store_certificates(store: CertStore) {
    let store_w = wide(STORE_NAME);
    // SAFETY: the store is opened and closed here; enumeration follows the
    // documented `prev`-context protocol; all results are best-effort.
    unsafe {
        let Ok(hstore) = CertOpenStore(
            CERT_STORE_PROV_SYSTEM_REGISTRY_W,
            CERT_QUERY_ENCODING_TYPE(0),
            None,
            CERT_OPEN_STORE_FLAGS(store.system_store_flag()),
            Some(store_w.as_ptr() as *const c_void),
        ) else {
            return;
        };

        let prov_prop = CERT_KEY_PROV_INFO_PROP_ID;
        let mut victims: Vec<[u8; THUMBPRINT_LEN]> = Vec::new();
        let mut ctx = CertEnumCertificatesInStore(hstore, None);
        while !ctx.is_null() {
            if cert_container_has_prefix(ctx, prov_prop) {
                if let Some(tp) = cert_thumbprint(ctx) {
                    victims.push(tp);
                }
            }
            ctx = CertEnumCertificatesInStore(hstore, Some(ctx));
        }
        for tp in &victims {
            delete_cert_by_thumbprint(hstore, tp);
        }
        let _ = CertCloseStore(Some(hstore), 0);
    }
}

/// Whether a certificate's `CERT_KEY_PROV_INFO` names a container with our prefix.
///
/// # Safety
///
/// `ctx` must be a valid certificate context.
unsafe fn cert_container_has_prefix(ctx: *const CERT_CONTEXT, prop_id: u32) -> bool {
    let mut len = 0u32;
    if CertGetCertificateContextProperty(ctx, prop_id, None, &mut len).is_err() || len == 0 {
        return false;
    }
    // SAFETY: allocate an 8-byte-aligned backing store (`Vec<u64>`) and let the
    // OS write the record into it, so the `CRYPT_KEY_PROV_INFO` — whose leading
    // field is a `PWSTR` needing 8-byte alignment on x64, and whose string
    // fields point back *into* this same buffer — is read through a well-aligned
    // reference and its internal pointers stay valid while `aligned` lives. This
    // is the idiom `ssl.rs::query_ssl_binding` uses for `HTTP_SERVICE_CONFIG_SSL_SET`.
    let words = (len as usize).div_ceil(std::mem::size_of::<u64>());
    let mut aligned = vec![0u64; words.max(1)];
    if CertGetCertificateContextProperty(
        ctx,
        prop_id,
        Some(aligned.as_mut_ptr() as *mut c_void),
        &mut len,
    )
    .is_err()
        || (len as usize) < std::mem::size_of::<CRYPT_KEY_PROV_INFO>()
    {
        return false;
    }
    let info = &*(aligned.as_ptr() as *const CRYPT_KEY_PROV_INFO);
    if info.pwszContainerName.is_null() {
        return false;
    }
    let name = info.pwszContainerName.to_string().unwrap_or_default();
    name.starts_with(CONTAINER_PREFIX)
}

/// Read a certificate context's SHA-1 thumbprint.
///
/// # Safety
///
/// `ctx` must be a valid certificate context.
unsafe fn cert_thumbprint(ctx: *const CERT_CONTEXT) -> Option<[u8; THUMBPRINT_LEN]> {
    let mut tp = [0u8; THUMBPRINT_LEN];
    let mut len = THUMBPRINT_LEN as u32;
    CertGetCertificateContextProperty(
        ctx,
        CERT_HASH_PROP_ID,
        Some(tp.as_mut_ptr() as *mut c_void),
        &mut len,
    )
    .ok()?;
    Some(tp)
}

/// Delete every CNG key container in `store`'s scope whose name carries our
/// prefix — including orphans with no installed certificate.
fn sweep_key_containers(store: CertStore) {
    let provider_w = wide(PROVIDER);
    // SAFETY: the provider handle is opened and freed here; `NCryptEnumKeys`
    // yields buffers freed with `NCryptFreeBuffer`; enumeration ends on
    // `NTE_NO_MORE_ITEMS`. All results are best-effort.
    unsafe {
        let mut prov = NCRYPT_PROV_HANDLE::default();
        if NCryptOpenStorageProvider(&mut prov, PCWSTR(provider_w.as_ptr()), 0).is_err() {
            return;
        }
        let prov = ProvHandle(prov);
        let scope_flags = NCRYPT_FLAGS(store.ncrypt_scope_flags());

        let mut victims: Vec<String> = Vec::new();
        let mut enum_state: *mut c_void = std::ptr::null_mut();
        loop {
            let mut key_name: *mut NCryptKeyName = std::ptr::null_mut();
            let rc = NCryptEnumKeys(
                prov.0,
                PCWSTR::null(),
                &mut key_name,
                &mut enum_state,
                scope_flags,
            );
            // Any error — normally NTE_NO_MORE_ITEMS — ends enumeration.
            if rc.is_err() || key_name.is_null() {
                break;
            }
            let name = (*key_name).pszName.to_string().unwrap_or_default();
            if name.starts_with(CONTAINER_PREFIX) {
                victims.push(name);
            }
            let _ = NCryptFreeBuffer(key_name as *mut c_void);
        }
        if !enum_state.is_null() {
            let _ = NCryptFreeBuffer(enum_state);
        }
        // Drop the provider before deleting so each delete opens its own handle
        // in the right scope.
        drop(prov);
        for name in &victims {
            delete_key_container(name, store);
        }
    }
}
