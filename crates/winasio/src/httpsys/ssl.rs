// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Binding a TLS server certificate to an `ip:port` for HTTP.sys.
//!
//! HTTP.sys does not read a certificate from the application. Instead the
//! operating system keeps a machine-wide table, keyed by `ip:port`, that maps an
//! endpoint to a certificate thumbprint and the store the certificate lives in.
//! When a TLS `ClientHello` arrives for a listener on that endpoint, HTTP.sys
//! looks the endpoint up in this table and presents the certificate it finds.
//! `netsh http add sslcert` writes one row of that table; this module writes the
//! same row through the underlying API, [`HttpSetServiceConfiguration`], so a
//! test can provision HTTPS without shelling out.
//!
//! # D1. Why a guard type rather than two free functions
//!
//! The SSL certificate table is **global, persistent, and machine-wide**. A row
//! written here outlives the process that wrote it and is visible to every other
//! program on the machine until something deletes it — `netsh http show sslcert`
//! lists it, a second listener on the same endpoint inherits it. Leaking one is
//! therefore not a private resource leak but a change to the machine other
//! software can trip over. [`bind_ssl_certificate`] returns an
//! [`SslCertBinding`] whose [`Drop`] deletes the row, so the common path — bind,
//! use, drop — cannot leak it even on panic. The explicit [`SslCertBinding::unbind`]
//! exists for callers who want to observe the delete's error; it consumes the
//! guard and suppresses the drop-path delete so the row is never deleted twice.
//!
//! # D2. Why `ERROR_ACCESS_DENIED` is its own error case
//!
//! Writing this table requires administrator rights, and the failure a
//! non-elevated caller gets is `ERROR_ACCESS_DENIED` (5) — measured, the same
//! code `netsh http add sslcert` prints as "requires elevation". A test harness
//! needs to tell "you are not elevated, skip" apart from "the bind is genuinely
//! broken", so [`SslBindError::RequiresElevation`] is a distinct, matchable case
//! rather than being folded into [`SslBindError::Platform`]. Callers key on it to
//! decide whether to skip; see the crate's HTTPS integration tests.
//!
//! # Precondition (initialisation ordering)
//!
//! [`HttpSetServiceConfiguration`], [`HttpDeleteServiceConfiguration`] and
//! [`HttpQueryServiceConfiguration`] operate on the HTTP **configuration**
//! subsystem, which [`HttpInitialize`](super::HttpInitializer) starts with
//! `HTTP_INITIALIZE_CONFIG`. A live [`HttpInitializer`](super::HttpInitializer) —
//! held directly, or transitively through a
//! [`ServerSession`](super::ServerSession) — MUST therefore exist before any of
//! [`bind_ssl_certificate`], [`query_ssl_binding`] or a guard's drop runs. The
//! functions do not take an initializer by reference because the subsystem is
//! reference-counted per process and the table is addressed by endpoint, not by
//! handle; the obligation is the caller's to honour by construction order.
//!
//! # Rejected alternatives
//!
//! * **Shelling out to `netsh http add sslcert`.** It is the same API underneath,
//!   but it means parsing localised console output to recover the thumbprint and
//!   the elevation failure, and it adds a process launch to every test. The typed
//!   call reports `ERROR_ACCESS_DENIED` directly.
//! * **A free `unbind(endpoint)` with no guard.** An aborted or panicking test
//!   would then leave a machine-wide row behind. The guard makes cleanup the
//!   default rather than a thing the caller must remember (see D1).
//! * **Holding an `&HttpInitializer` in the guard.** The subsystem is
//!   process-reference-counted and the table is keyed by endpoint, so a borrow
//!   would buy no safety the ordering precondition does not already require, while
//!   forcing a lifetime onto every binding. Documented as an obligation instead.
//!
//! # Invariants and obligations
//!
//! * **One guard per endpoint.** Two [`SslCertBinding`]s for the same `ip:port`
//!   would each try to delete the row on drop; the second delete finds nothing.
//!   Bind an endpoint once.
//! * **The guard deletes exactly the endpoint it bound.** [`Drop`] rebuilds the
//!   same `ip:port` key it was constructed with, so it can never delete a row for
//!   a different endpoint.
//! * **A live initializer must outlive the binding.** See the precondition above:
//!   construct the [`HttpInitializer`](super::HttpInitializer) /
//!   [`ServerSession`](super::ServerSession) first and drop it last.

use std::ffi::c_void;
use std::net::SocketAddr;

use windows::core::{Error, GUID, PWSTR};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER,
    ERROR_NOT_FOUND, NO_ERROR, WIN32_ERROR,
};
use windows::Win32::Networking::HttpServer::{
    HttpDeleteServiceConfiguration, HttpQueryServiceConfiguration, HttpServiceConfigQueryExact,
    HttpServiceConfigSSLCertInfo, HttpSetServiceConfiguration, HTTP_SERVICE_CONFIG_SSL_KEY,
    HTTP_SERVICE_CONFIG_SSL_PARAM, HTTP_SERVICE_CONFIG_SSL_QUERY, HTTP_SERVICE_CONFIG_SSL_SET,
};
use windows::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, IN6_ADDR, IN6_ADDR_0, IN_ADDR, IN_ADDR_0, IN_ADDR_0_0, SOCKADDR,
    SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_IN6_0, SOCKADDR_STORAGE,
};

use super::error::win32_code;

/// The number of bytes in a SHA-1 certificate thumbprint.
///
/// HTTP.sys keys the SSL table by this hash; `windows`'s `CERT_HASH_PROP_ID`
/// yields exactly these 20 bytes.
pub const THUMBPRINT_LEN: usize = 20;

/// A stable, crate-owned application identifier written into every SSL binding
/// this crate creates.
///
/// HTTP.sys stores an `AppId` GUID alongside each row purely as an ownership
/// marker — it does not affect matching, which is by `ip:port`. Using one fixed
/// value lets a cleanup sweep recognise rows this crate wrote (in addition to the
/// port range it owns) rather than deleting a row some other program installed.
/// It is a randomly generated, otherwise-meaningless constant.
pub const SSL_BINDING_APP_ID: GUID = GUID::from_u128(0xefdd8764_4bfd_49f6_903e_b0ef375d1941);

/// A failure setting up or tearing down an SSL certificate binding.
///
/// Deliberately **not** `#[non_exhaustive]`, matching `winasio-util`'s error
/// convention (see `winasio-tests/tests/util_error.rs`): the crate treats adding
/// a public error variant as a semver-major change and proves the closed set
/// with an exhaustive, wildcard-free match from the test crate, so that adding a
/// variant breaks that proof with `E0004` rather than passing silently. The
/// [`SslBindError::Platform`] arm is the extensibility escape hatch — a new
/// platform condition rides in there without a new variant.
#[derive(Debug)]
pub enum SslBindError {
    /// The operation was refused for lack of administrator rights
    /// (`ERROR_ACCESS_DENIED`). Writing the machine-wide SSL table needs
    /// elevation; a caller keys on this to skip rather than fail. See module
    /// docs D2.
    RequiresElevation,
    /// A binding already exists for this endpoint (`ERROR_ALREADY_EXISTS`). The
    /// SSL table holds at most one row per `ip:port`.
    AlreadyBound,
    /// Any other platform failure, carrying the underlying error.
    Platform(Error),
}

impl SslBindError {
    /// Classify a Win32 error returned by the HTTP configuration API.
    ///
    /// The `Http*ServiceConfiguration` functions return a Win32 code directly;
    /// this maps the two the caller can act on — elevation and already-bound —
    /// and leaves everything else opaque in [`SslBindError::Platform`].
    pub fn from_win32(err: Error) -> Self {
        match win32_code(&err) {
            Some(code) if code == ERROR_ACCESS_DENIED.0 => SslBindError::RequiresElevation,
            Some(code) if code == ERROR_ALREADY_EXISTS.0 => SslBindError::AlreadyBound,
            _ => SslBindError::Platform(err),
        }
    }
}

impl std::fmt::Display for SslBindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SslBindError::RequiresElevation => {
                write!(
                    f,
                    "binding an SSL certificate requires administrator rights"
                )
            }
            SslBindError::AlreadyBound => {
                write!(f, "an SSL certificate is already bound to this endpoint")
            }
            SslBindError::Platform(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SslBindError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SslBindError::Platform(e) => Some(e),
            _ => None,
        }
    }
}

/// Turn a Win32 code returned by an HTTP configuration function into a result.
fn check(code: u32) -> Result<(), SslBindError> {
    let err = WIN32_ERROR(code);
    if err == NO_ERROR {
        Ok(())
    } else {
        Err(SslBindError::from_win32(Error::from_hresult(
            err.to_hresult(),
        )))
    }
}

/// A `sockaddr` on the stack, holding an endpoint for the SSL table key.
///
/// HTTP.sys's key names the endpoint through a `*mut SOCKADDR`; this owns the
/// backing storage so a pointer to it stays valid for the duration of a call.
struct IpPort {
    storage: SOCKADDR_STORAGE,
}

impl IpPort {
    /// Encode a `std` socket address into `sockaddr` storage.
    fn new(addr: SocketAddr) -> Self {
        let mut storage = SOCKADDR_STORAGE::default();
        match addr {
            SocketAddr::V4(v4) => {
                let sin = SOCKADDR_IN {
                    sin_family: AF_INET,
                    // The endpoint port is stored in network byte order.
                    sin_port: v4.port().to_be(),
                    sin_addr: IN_ADDR {
                        S_un: IN_ADDR_0 {
                            S_un_b: {
                                let o = v4.ip().octets();
                                IN_ADDR_0_0 {
                                    s_b1: o[0],
                                    s_b2: o[1],
                                    s_b3: o[2],
                                    s_b4: o[3],
                                }
                            },
                        },
                    },
                    sin_zero: [0; 8],
                };
                // SAFETY: `SOCKADDR_STORAGE` is at least as large and as aligned
                // as `SOCKADDR_IN`; both are plain-data C structs. Mirrors the
                // established encoding in `crate::net::addr`.
                unsafe {
                    std::ptr::write_unaligned(
                        std::ptr::addr_of_mut!(storage).cast::<SOCKADDR_IN>(),
                        sin,
                    );
                }
            }
            SocketAddr::V6(v6) => {
                let sin6 = SOCKADDR_IN6 {
                    sin6_family: AF_INET6,
                    sin6_port: v6.port().to_be(),
                    sin6_flowinfo: v6.flowinfo().to_be(),
                    sin6_addr: IN6_ADDR {
                        u: IN6_ADDR_0 {
                            Byte: v6.ip().octets(),
                        },
                    },
                    Anonymous: SOCKADDR_IN6_0 {
                        sin6_scope_id: v6.scope_id(),
                    },
                };
                // SAFETY: as above, for the v6 layout.
                unsafe {
                    std::ptr::write_unaligned(
                        std::ptr::addr_of_mut!(storage).cast::<SOCKADDR_IN6>(),
                        sin6,
                    );
                }
            }
        }
        IpPort { storage }
    }

    /// A pointer to the stored `sockaddr` for the SSL table key.
    ///
    /// The API only reads through this pointer; the `*mut` in the key type is a
    /// C signature artefact, not a mutation.
    fn as_key_ptr(&self) -> *mut SOCKADDR {
        std::ptr::addr_of!(self.storage) as *mut SOCKADDR
    }
}

/// Bind a TLS server certificate to an `ip:port` in the machine-wide SSL table.
///
/// `endpoint` is the address HTTP.sys will present the certificate on — use a
/// wildcard address such as `0.0.0.0:PORT` or `[::]:PORT` to cover every local
/// interface. `thumbprint` is the certificate's SHA-1 hash — exactly
/// [`THUMBPRINT_LEN`] (20) bytes, enforced at the type level so a wrong-length
/// hash cannot be written into the machine-global table. `store_name` is the
/// system store the certificate lives in, spelled as HTTP.sys expects it (for
/// example `"MY"` for the personal store). `app_id` is an ownership marker;
/// pass [`SSL_BINDING_APP_ID`].
///
/// Returns an [`SslCertBinding`] guard that removes the row on drop. See the
/// module's precondition: a live
/// [`HttpInitializer`](super::HttpInitializer) must already exist.
///
/// # Errors
///
/// [`SslBindError::RequiresElevation`] when the process is not elevated,
/// [`SslBindError::AlreadyBound`] when a row already exists for `endpoint`, and
/// [`SslBindError::Platform`] otherwise.
///
/// ```no_run
/// use std::net::SocketAddr;
/// use winasio::httpsys::{bind_ssl_certificate, HttpInitializer, SSL_BINDING_APP_ID};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// // A live initializer must outlive the binding (see module docs).
/// let _http = HttpInitializer::new()?;
/// let endpoint: SocketAddr = "0.0.0.0:12480".parse()?;
/// let thumbprint = [0u8; 20]; // a real 20-byte SHA-1 hash
/// let binding = bind_ssl_certificate(endpoint, &thumbprint, "MY", SSL_BINDING_APP_ID)?;
/// // ... serve HTTPS ...
/// drop(binding); // removes the machine-wide row
/// # Ok(())
/// # }
/// ```
pub fn bind_ssl_certificate(
    endpoint: SocketAddr,
    thumbprint: &[u8; THUMBPRINT_LEN],
    store_name: &str,
    app_id: GUID,
) -> Result<SslCertBinding, SslBindError> {
    let ipport = IpPort::new(endpoint);
    let mut store_w: Vec<u16> = store_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // The API reads, but does not take ownership of, the hash bytes; a local
    // copy keeps them alive and mutable for the pointer the struct wants.
    let mut hash = thumbprint.to_vec();

    let set = HTTP_SERVICE_CONFIG_SSL_SET {
        KeyDesc: HTTP_SERVICE_CONFIG_SSL_KEY {
            pIpPort: ipport.as_key_ptr(),
        },
        ParamDesc: HTTP_SERVICE_CONFIG_SSL_PARAM {
            SslHashLength: hash.len() as u32,
            pSslHash: hash.as_mut_ptr() as *mut c_void,
            AppId: app_id,
            pSslCertStoreName: PWSTR(store_w.as_mut_ptr()),
            ..Default::default()
        },
    };

    // SAFETY: `set` and every buffer it points at (the sockaddr in `ipport`, the
    // hash in `hash`, the store name in `store_w`) outlive the synchronous call;
    // no overlapped is passed, so the API returns before any pointer is dropped.
    let code = unsafe {
        HttpSetServiceConfiguration(
            None,
            HttpServiceConfigSSLCertInfo,
            &set as *const _ as *const c_void,
            std::mem::size_of::<HTTP_SERVICE_CONFIG_SSL_SET>() as u32,
            None,
        )
    };
    check(code)?;
    Ok(SslCertBinding {
        endpoint,
        released: false,
    })
}

/// Delete the SSL table row for `endpoint`, if any.
///
/// Idempotent from the caller's view apart from the returned error: deleting a
/// nonexistent row reports a not-found platform error. Exposed to the crate so
/// the `test-util` cleanup sweep can remove stale bindings by endpoint.
pub(crate) fn delete_binding(endpoint: SocketAddr) -> Result<(), SslBindError> {
    let ipport = IpPort::new(endpoint);
    let set = HTTP_SERVICE_CONFIG_SSL_SET {
        KeyDesc: HTTP_SERVICE_CONFIG_SSL_KEY {
            pIpPort: ipport.as_key_ptr(),
        },
        ParamDesc: HTTP_SERVICE_CONFIG_SSL_PARAM::default(),
    };
    // SAFETY: the delete reads only `KeyDesc`, whose sockaddr in `ipport`
    // outlives this synchronous call; no overlapped is passed.
    let code = unsafe {
        HttpDeleteServiceConfiguration(
            None,
            HttpServiceConfigSSLCertInfo,
            &set as *const _ as *const c_void,
            std::mem::size_of::<HTTP_SERVICE_CONFIG_SSL_SET>() as u32,
            None,
        )
    };
    check(code)
}

/// The SHA-1 thumbprint currently bound to `endpoint`, or `None` if no binding
/// exists.
///
/// Makes a binding observable — used to confirm a bind took effect and, after a
/// guard drops, that its row is gone. See the module's precondition: a live
/// [`HttpInitializer`](super::HttpInitializer) must already exist.
///
/// # Errors
///
/// [`SslBindError::Platform`] on an unexpected platform failure. A missing
/// binding is `Ok(None)`, not an error.
pub fn query_ssl_binding(
    endpoint: SocketAddr,
) -> Result<Option<[u8; THUMBPRINT_LEN]>, SslBindError> {
    let ipport = IpPort::new(endpoint);
    let query = HTTP_SERVICE_CONFIG_SSL_QUERY {
        QueryDesc: HttpServiceConfigQueryExact,
        KeyDesc: HTTP_SERVICE_CONFIG_SSL_KEY {
            pIpPort: ipport.as_key_ptr(),
        },
        dwToken: 0,
    };
    let input = Some(&query as *const _ as *const c_void);
    let input_len = std::mem::size_of::<HTTP_SERVICE_CONFIG_SSL_QUERY>() as u32;

    // First pass: learn the size of the variable-length record (the struct plus
    // the appended hash and store-name bytes).
    let mut needed = 0u32;
    // SAFETY: `query` (and the sockaddr it points at) outlive the call; the
    // output is null with length 0, so nothing is written.
    let code = unsafe {
        HttpQueryServiceConfiguration(
            None,
            HttpServiceConfigSSLCertInfo,
            input,
            input_len,
            None,
            0,
            Some(&mut needed),
            None,
        )
    };
    match WIN32_ERROR(code) {
        // No binding for this endpoint.
        ERROR_FILE_NOT_FOUND | ERROR_NOT_FOUND => return Ok(None),
        // Expected: the buffer was too small, and `needed` now holds the size.
        ERROR_INSUFFICIENT_BUFFER => {}
        // A zero-length record is not something this API produces, but treat a
        // surprising success as "nothing to copy" rather than reading nowhere.
        NO_ERROR => return Ok(None),
        other => {
            return Err(SslBindError::from_win32(Error::from_hresult(
                other.to_hresult(),
            )))
        }
    }

    // Second pass: an 8-byte-aligned buffer so the record's internal pointers
    // (`pSslHash`, `pSslCertStoreName`) can be read through a well-aligned
    // `HTTP_SERVICE_CONFIG_SSL_SET`.
    let words = (needed as usize).div_ceil(std::mem::size_of::<u64>());
    let mut buf = vec![0u64; words.max(1)];
    // SAFETY: `query` outlives the call; `buf` provides `needed` writable,
    // 8-byte-aligned bytes for the output record.
    let code = unsafe {
        HttpQueryServiceConfiguration(
            None,
            HttpServiceConfigSSLCertInfo,
            input,
            input_len,
            Some(buf.as_mut_ptr() as *mut c_void),
            needed,
            Some(&mut needed),
            None,
        )
    };
    match WIN32_ERROR(code) {
        NO_ERROR => {}
        ERROR_FILE_NOT_FOUND | ERROR_NOT_FOUND => return Ok(None),
        other => {
            return Err(SslBindError::from_win32(Error::from_hresult(
                other.to_hresult(),
            )))
        }
    }

    // SAFETY: the API wrote a `HTTP_SERVICE_CONFIG_SSL_SET` at the start of
    // `buf`, which is 8-byte aligned and at least `needed` bytes; `pSslHash`
    // points inside the same buffer, which is still alive here.
    let set = unsafe { &*(buf.as_ptr() as *const HTTP_SERVICE_CONFIG_SSL_SET) };
    let hash_len = set.ParamDesc.SslHashLength as usize;
    if set.ParamDesc.pSslHash.is_null() || hash_len == 0 {
        return Ok(None);
    }
    let mut out = [0u8; THUMBPRINT_LEN];
    let n = hash_len.min(THUMBPRINT_LEN);
    // SAFETY: `pSslHash` points at `hash_len` readable bytes inside `buf`; we
    // copy at most `THUMBPRINT_LEN` of them into `out`.
    unsafe {
        std::ptr::copy_nonoverlapping(set.ParamDesc.pSslHash as *const u8, out.as_mut_ptr(), n);
    }
    Ok(Some(out))
}

/// An RAII guard for one SSL certificate binding.
///
/// Created by [`bind_ssl_certificate`]. Dropping it deletes the machine-wide SSL
/// table row for its endpoint; [`SslCertBinding::unbind`] does the same but
/// returns the delete's error. Because the binding is global state, letting the
/// guard drop is the reliable way to clean up — see the module's D1.
#[derive(Debug)]
pub struct SslCertBinding {
    endpoint: SocketAddr,
    /// Set once the row has been deleted (by [`SslCertBinding::unbind`]) so
    /// [`Drop`] does not delete it a second time.
    released: bool,
}

impl SslCertBinding {
    /// The endpoint this guard is bound to.
    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    /// Delete the binding now and report the outcome.
    ///
    /// Consumes the guard and suppresses the drop-path delete, so the row is
    /// removed exactly once. Use this when the delete's error matters; otherwise
    /// just let the guard drop.
    ///
    /// # Errors
    ///
    /// [`SslBindError::RequiresElevation`] or [`SslBindError::Platform`] if the
    /// delete fails.
    pub fn unbind(mut self) -> Result<(), SslBindError> {
        let result = delete_binding(self.endpoint);
        // Suppress the drop-path delete regardless of outcome: a failed delete
        // will not succeed on a second try, and reporting it once is enough.
        self.released = true;
        result
    }
}

impl Drop for SslCertBinding {
    fn drop(&mut self) {
        if !self.released {
            // Deliberately ignored, matching the crate's other release-on-drop
            // types: a panic here would abort during unwinding, and a failed
            // delete is not something a drop can act on.
            let _ = delete_binding(self.endpoint);
        }
    }
}
