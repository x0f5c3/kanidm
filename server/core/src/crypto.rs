//! This module contains cryptographic setup code, a long with what policy
//! and ciphers we accept.

use openssl::ec::{EcGroup, EcKey};
use openssl::error::ErrorStack;
use openssl::nid::Nid;
use openssl::ssl::{SslAcceptor, SslAcceptorBuilder, SslFiletype, SslMethod};
use openssl::x509::{
    extension::{
        AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage,
        SubjectAlternativeName, SubjectKeyIdentifier,
    },
    X509NameBuilder, X509ReqBuilder, X509,
};
use openssl::{asn1, bn, hash, pkey};
use time::{OffsetDateTime, Duration};

use crate::config::Configuration;

use anyhow::{bail, Context};
use rcgen::{Certificate, CertificateParams, DnType, IsCa, SerialNumber};
use rustls::ServerConfig;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::ops::Add;
use std::path::Path;
use std::sync::Arc;

const CA_VALID_DAYS: u32 = 30;
const CERT_VALID_DAYS: u32 = 5;


pub fn read_key<P: Into<Path>>(p: P) -> anyhow::Result<rustls::PrivateKey> {
    let mut f = BufReader::new(File::open(p)?);
    let mut keys = rustls_pemfile::pkcs8_private_keys(&mut f)?;
    match keys.len() {
        0 => bail!("No PKCS8-encoded private key found in {p}"),
        1 => Ok(rustls::PrivateKey(keys.remove(0))),
        _ => bail!("More than one PKCS8-encoded private key found in {p}, those are the keys {keys:?}")
    }
}

/// From the server configuration, generate an OpenSSL acceptor that we can use
/// to build our sockets for https/ldaps.
pub fn setup_tls(config: &Configuration) -> Result<Option<ServerConfig>, ErrorStack> {
    match &config.tls_config {
        Some(tls_config) => {
            let chain_f = File::open(&tls_config.chain)?;
            let chain_pem: Vec<rustls::Certificate> = rustls_pemfile::certs(&mut BufReader::new(chain_f)).map(|rawcert| rawcert.into_iter().map(rustls::Certificate).collect())?;
            let k = read_key(&tls_config.key)?;
            let mut b = ServerConfig::builder().with_safe_defaults().with_no_client_auth().with_single_cert(chain_pem, k)?;
            Ok(Some(b))
            // let mut ssl_builder = SslAcceptor::mozilla_modern(SslMethod::tls())?;
            // ssl_builder.set_certificate_chain_file(&tls_config.chain)?;
            // ssl_builder.set_private_key_file(&tls_config.key, SslFiletype::PEM)?;
            // ssl_builder.check_private_key()?;
            // Ok(Some(ssl_builder))
        }
        None => Ok(None),
    }
}

fn get_group() -> Result<EcGroup, ErrorStack> {
    EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)
}

fn rustls_group() -> &'static rcgen::SignatureAlgorithm {
    &rcgen::PKCS_ECDSA_P256_SHA256
}

fn rustls_gen() -> anyhow::Result<rcgen::KeyPair> {
    rcgen::KeyPair::generate(rustls_group()).context("Cannot generate CA key")
}

pub(crate) struct CaHandle {
    key: pkey::PKey<pkey::Private>,
    cert: X509,
}

pub(crate) struct RustlsCaHandle {
    key: rcgen::KeyPair,
    cert: Certificate,
}

pub(crate) fn write_ca(
    key_ar: impl AsRef<Path>,
    cert_ar: impl AsRef<Path>,
    handle: &RustlsCaHandle,
) -> Result<(), ()> {
    let key_path: &Path = key_ar.as_ref();
    let cert_path: &Path = cert_ar.as_ref();

    let key_pem = handle.key.private_key_to_pem_pkcs8().map_err(|e| {
        error!(err = ?e, "Failed to convert key to PEM");
    })?;

    let cert_pem = handle.cert.map_err(|e| {
        error!(err = ?e, "Failed to convert cert to PEM");
    })?;

    File::create(key_path)
        .and_then(|mut file| file.write_all(&key_pem))
        .map_err(|e| {
            error!(err = ?e, "Failed to create {:?}", key_path);
        })?;

    File::create(cert_path)
        .and_then(|mut file| file.write_all(&cert_pem))
        .map_err(|e| {
            error!(err = ?e, "Failed to create {:?}", cert_path);
        })
}

pub(crate) fn build_name() -> rcgen::DistinguishedName {
    let mut res = rcgen::DistinguishedName::new();
    res.push(DnType::CountryName, "AU");
    res.push(DnType::StateOrProvinceName, "QLD");
    res.push(DnType::OrganizationName, "Kanidm");
    res.push(DnType::CommonName, "Kanidm Generated CA");
    res.push(
        DnType::OrganizationalUnitName,
        "Development and Evaluation - NOT FOR PRODUCTION",
    );
    res
}

pub(crate) fn build_rustls_ca() -> anyhow::Result<RustlsCaHandle> {
    let name = build_name();
    let key = rustls_gen()?;
    let mut params = CertificateParams::default();
    params.alg = rustls_group();
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.distinguished_name = name;
    params.key_pair = Some(key.clone());
    params.serial_number = Some(SerialNumber::from_slice(&[1]));
    let now = OffsetDateTime::now_local()?;
    let not_after = now.add(Duration::days(CA_VALID_DAYS as i64));
    params.not_before = now;
    params.not_after = not_after;
    let cert = Certificate::from_params(params)?;
    Ok(RustlsCaHandle {
        key,
        cert
    })

}

pub(crate) fn build_ca() -> Result<CaHandle, ErrorStack> {
    let ecgroup = get_group()?;
    let eckey = EcKey::generate(&ecgroup)?;
    let ca_key = pkey::PKey::from_ec_key(eckey)?;
    let mut x509_name = X509NameBuilder::new()?;

    x509_name.append_entry_by_text("C", "AU")?;
    x509_name.append_entry_by_text("ST", "QLD")?;
    x509_name.append_entry_by_text("O", "Kanidm")?;
    x509_name.append_entry_by_text("CN", "Kanidm Generated CA")?;
    x509_name.append_entry_by_text("OU", "Development and Evaluation - NOT FOR PRODUCTION")?;
    let x509_name = x509_name.build();

    let mut cert_builder = X509::builder()?;
    // Yes, 2 actually means 3 here ...
    cert_builder.set_version(2)?;

    let serial_number = bn::BigNum::from_u32(1).and_then(|serial| serial.to_asn1_integer())?;

    cert_builder.set_serial_number(&serial_number)?;
    cert_builder.set_subject_name(&x509_name)?;
    cert_builder.set_issuer_name(&x509_name)?;

    let not_before = asn1::Asn1Time::days_from_now(0)?;
    cert_builder.set_not_before(&not_before)?;
    let not_after = asn1::Asn1Time::days_from_now(CA_VALID_DAYS)?;
    cert_builder.set_not_after(&not_after)?;

    cert_builder.append_extension(BasicConstraints::new().critical().ca().pathlen(0).build()?)?;
    cert_builder.append_extension(
        KeyUsage::new()
            .critical()
            .key_cert_sign()
            .crl_sign()
            .build()?,
    )?;

    let subject_key_identifier =
        SubjectKeyIdentifier::new().build(&cert_builder.x509v3_context(None, None))?;
    cert_builder.append_extension(subject_key_identifier)?;

    cert_builder.set_pubkey(&ca_key)?;

    cert_builder.sign(&ca_key, hash::MessageDigest::sha256())?;
    let ca_cert = cert_builder.build();

    Ok(CaHandle {
        key: ca_key,
        cert: ca_cert,
    })
}

pub(crate) fn load_ca(
    ca_key_ar: impl AsRef<Path>,
    ca_cert_ar: impl AsRef<Path>,
) -> Result<CaHandle, ()> {
    let ca_key_path: &Path = ca_key_ar.as_ref();
    let ca_cert_path: &Path = ca_cert_ar.as_ref();

    let mut ca_key_pem = vec![];
    File::open(ca_key_path)
        .and_then(|mut file| file.read_to_end(&mut ca_key_pem))
        .map_err(|e| {
            error!(err = ?e, "Failed to read {:?}", ca_key_path);
        })?;

    let mut ca_cert_pem = vec![];
    File::open(ca_cert_path)
        .and_then(|mut file| file.read_to_end(&mut ca_cert_pem))
        .map_err(|e| {
            error!(err = ?e, "Failed to read {:?}", ca_cert_path);
        })?;

    let ca_key = pkey::PKey::private_key_from_pem(&ca_key_pem).map_err(|e| {
        error!(err = ?e, "Failed to convert PEM to key");
    })?;

    let ca_cert = X509::from_pem(&ca_cert_pem).map_err(|e| {
        error!(err = ?e, "Failed to convert PEM to cert");
    })?;

    Ok(CaHandle {
        key: ca_key,
        cert: ca_cert,
    })
}

pub(crate) struct CertHandle {
    key: pkey::PKey<pkey::Private>,
    cert: X509,
    chain: Vec<X509>,
}

pub(crate) fn write_cert(
    key_ar: impl AsRef<Path>,
    chain_ar: impl AsRef<Path>,
    cert_ar: impl AsRef<Path>,
    handle: &CertHandle,
) -> Result<(), ()> {
    let key_path: &Path = key_ar.as_ref();
    let chain_path: &Path = chain_ar.as_ref();
    let cert_path: &Path = cert_ar.as_ref();

    let key_pem = handle.key.private_key_to_pem_pkcs8().map_err(|e| {
        error!(err = ?e, "Failed to convert key to PEM");
    })?;

    let cert_pem = handle.cert.to_pem().map_err(|e| {
        error!(err = ?e, "Failed to convert cert to PEM");
    })?;

    let mut chain_pem = cert_pem.clone();

    // Build the chain PEM.
    for ca_cert in &handle.chain {
        match ca_cert.to_pem() {
            Ok(c) => {
                chain_pem.extend_from_slice(&c);
            }
            Err(e) => {
                error!(err = ?e, "Failed to convert cert to PEM");
                return Err(());
            }
        }
    }

    File::create(key_path)
        .and_then(|mut file| file.write_all(&key_pem))
        .map_err(|e| {
            error!(err = ?e, "Failed to create {:?}", key_path);
        })?;

    File::create(chain_path)
        .and_then(|mut file| file.write_all(&chain_pem))
        .map_err(|e| {
            error!(err = ?e, "Failed to create {:?}", chain_path);
        })?;

    File::create(cert_path)
        .and_then(|mut file| file.write_all(&cert_pem))
        .map_err(|e| {
            error!(err = ?e, "Failed to create {:?}", cert_path);
        })
}

pub(crate) fn build_cert(
    domain_name: &str,
    ca_handle: &CaHandle,
) -> Result<CertHandle, ErrorStack> {
    let ecgroup = get_group()?;
    let eckey = EcKey::generate(&ecgroup)?;
    let int_key = pkey::PKey::from_ec_key(eckey)?;

    //
    let mut req_builder = X509ReqBuilder::new()?;
    req_builder.set_pubkey(&int_key)?;

    let mut x509_name = X509NameBuilder::new()?;
    x509_name.append_entry_by_text("C", "AU")?;
    x509_name.append_entry_by_text("ST", "QLD")?;
    x509_name.append_entry_by_text("O", "Kanidm")?;
    x509_name.append_entry_by_text("CN", domain_name)?;
    // Requirement of packed attestation.
    x509_name.append_entry_by_text("OU", "Development and Evaluation - NOT FOR PRODUCTION")?;
    let x509_name = x509_name.build();

    req_builder.set_subject_name(&x509_name)?;
    req_builder.sign(&int_key, hash::MessageDigest::sha256())?;
    let req = req_builder.build();
    // ==

    let mut cert_builder = X509::builder()?;
    // Yes, 2 actually means 3 here ...
    cert_builder.set_version(2)?;
    let serial_number = bn::BigNum::from_u32(2).and_then(|serial| serial.to_asn1_integer())?;

    cert_builder.set_pubkey(&int_key)?;

    cert_builder.set_serial_number(&serial_number)?;
    cert_builder.set_subject_name(req.subject_name())?;
    cert_builder.set_issuer_name(ca_handle.cert.subject_name())?;

    let not_before = asn1::Asn1Time::days_from_now(0)?;
    cert_builder.set_not_before(&not_before)?;
    let not_after = asn1::Asn1Time::days_from_now(CERT_VALID_DAYS)?;
    cert_builder.set_not_after(&not_after)?;

    cert_builder.append_extension(BasicConstraints::new().build()?)?;

    cert_builder.append_extension(
        KeyUsage::new()
            .critical()
            // .non_repudiation()
            .digital_signature()
            .key_encipherment()
            .build()?,
    )?;

    cert_builder.append_extension(
        ExtendedKeyUsage::new()
            // .critical()
            .server_auth()
            .build()?,
    )?;

    let subject_key_identifier = SubjectKeyIdentifier::new()
        .build(&cert_builder.x509v3_context(Some(&ca_handle.cert), None))?;
    cert_builder.append_extension(subject_key_identifier)?;

    let auth_key_identifier = AuthorityKeyIdentifier::new()
        .keyid(false)
        .issuer(false)
        .build(&cert_builder.x509v3_context(Some(&ca_handle.cert), None))?;
    cert_builder.append_extension(auth_key_identifier)?;

    let subject_alt_name = SubjectAlternativeName::new()
        .dns(domain_name)
        .build(&cert_builder.x509v3_context(Some(&ca_handle.cert), None))?;
    cert_builder.append_extension(subject_alt_name)?;

    cert_builder.sign(&ca_handle.key, hash::MessageDigest::sha256())?;
    let int_cert = cert_builder.build();

    Ok(CertHandle {
        key: int_key,
        cert: int_cert,
        chain: vec![ca_handle.cert.clone()],
    })
}
