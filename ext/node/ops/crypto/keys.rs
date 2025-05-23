// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use std::borrow::Cow;
use std::cell::RefCell;

use base64::Engine;
use deno_core::error::generic_error;
use deno_core::error::type_error;
use deno_core::error::AnyError;
use deno_core::op2;
use deno_core::serde_v8::BigInt as V8BigInt;
use deno_core::unsync::spawn_blocking;
use deno_core::GarbageCollected;
use deno_core::ToJsBuffer;
use ed25519_dalek::pkcs8::BitStringRef;
use elliptic_curve::JwkEcKey;
use num_bigint::BigInt;
use num_traits::FromPrimitive as _;
use pkcs8::DecodePrivateKey as _;
use pkcs8::Document;
use pkcs8::EncodePrivateKey as _;
use pkcs8::EncryptedPrivateKeyInfo;
use pkcs8::PrivateKeyInfo;
use pkcs8::SecretDocument;
use rand::thread_rng;
use rand::RngCore as _;
use rsa::pkcs1::DecodeRsaPrivateKey as _;
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::pkcs1::EncodeRsaPrivateKey as _;
use rsa::pkcs1::EncodeRsaPublicKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use rsa::RsaPublicKey;
use sec1::der::Tag;
use sec1::der::Writer as _;
use sec1::pem::PemLabel as _;
use sec1::DecodeEcPrivateKey as _;
use sec1::LineEnding;
use spki::der::asn1;
use spki::der::asn1::OctetStringRef;
use spki::der::AnyRef;
use spki::der::Decode as _;
use spki::der::Encode as _;
use spki::der::PemWriter;
use spki::der::Reader as _;
use spki::DecodePublicKey as _;
use spki::EncodePublicKey as _;
use spki::SubjectPublicKeyInfoRef;
use x509_parser::x509;

use super::dh;
use super::dh::DiffieHellmanGroup;
use super::digest::match_fixed_digest_with_oid;
use super::pkcs3;
use super::pkcs3::DhParameter;
use super::primes::Prime;

#[derive(Clone)]
pub enum KeyObjectHandle {
    AsymmetricPrivate(AsymmetricPrivateKey),
    AsymmetricPublic(AsymmetricPublicKey),
    Secret(Box<[u8]>),
}

impl GarbageCollected for KeyObjectHandle {}

#[derive(Clone)]
pub enum AsymmetricPrivateKey {
    Rsa(RsaPrivateKey),
    RsaPss(RsaPssPrivateKey),
    Dsa(dsa::SigningKey),
    Ec(EcPrivateKey),
    X25519(x25519_dalek::StaticSecret),
    Ed25519(ed25519_dalek::SigningKey),
    Dh(DhPrivateKey),
}

#[derive(Clone)]
pub struct RsaPssPrivateKey {
    pub key: RsaPrivateKey,
    pub details: Option<RsaPssDetails>,
}

#[derive(Clone, Copy)]
pub struct RsaPssDetails {
    pub hash_algorithm: RsaPssHashAlgorithm,
    pub mf1_hash_algorithm: RsaPssHashAlgorithm,
    pub salt_length: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RsaPssHashAlgorithm {
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Sha512_224,
    Sha512_256,
}

impl RsaPssHashAlgorithm {
    pub fn as_str(&self) -> &'static str {
        match self {
            RsaPssHashAlgorithm::Sha1 => "sha1",
            RsaPssHashAlgorithm::Sha224 => "sha224",
            RsaPssHashAlgorithm::Sha256 => "sha256",
            RsaPssHashAlgorithm::Sha384 => "sha384",
            RsaPssHashAlgorithm::Sha512 => "sha512",
            RsaPssHashAlgorithm::Sha512_224 => "sha512-224",
            RsaPssHashAlgorithm::Sha512_256 => "sha512-256",
        }
    }

    pub fn salt_length(&self) -> u32 {
        match self {
            RsaPssHashAlgorithm::Sha1 => 20,
            RsaPssHashAlgorithm::Sha224 | RsaPssHashAlgorithm::Sha512_224 => 28,
            RsaPssHashAlgorithm::Sha256 | RsaPssHashAlgorithm::Sha512_256 => 32,
            RsaPssHashAlgorithm::Sha384 => 48,
            RsaPssHashAlgorithm::Sha512 => 64,
        }
    }
}

#[derive(Clone)]
pub enum EcPrivateKey {
    P224(p224::SecretKey),
    P256(p256::SecretKey),
    P384(p384::SecretKey),
}

#[derive(Clone)]
pub struct DhPrivateKey {
    pub key: dh::PrivateKey,
    pub params: DhParameter,
}

#[derive(Clone)]
pub enum AsymmetricPublicKey {
    Rsa(rsa::RsaPublicKey),
    RsaPss(RsaPssPublicKey),
    Dsa(dsa::VerifyingKey),
    Ec(EcPublicKey),
    X25519(x25519_dalek::PublicKey),
    Ed25519(ed25519_dalek::VerifyingKey),
    Dh(DhPublicKey),
}

#[derive(Clone)]
pub struct RsaPssPublicKey {
    pub key: rsa::RsaPublicKey,
    pub details: Option<RsaPssDetails>,
}

#[derive(Clone)]
pub enum EcPublicKey {
    P224(p224::PublicKey),
    P256(p256::PublicKey),
    P384(p384::PublicKey),
}

#[derive(Clone)]
pub struct DhPublicKey {
    pub key: dh::PublicKey,
    pub params: DhParameter,
}

impl KeyObjectHandle {
    /// Returns the private key if the handle is an asymmetric private key.
    pub fn as_private_key(&self) -> Option<&AsymmetricPrivateKey> {
        match self {
            KeyObjectHandle::AsymmetricPrivate(key) => Some(key),
            _ => None,
        }
    }

    /// Returns the public key if the handle is an asymmetric public key. If it is
    /// a private key, it derives the public key from it and returns that.
    pub fn as_public_key(&self) -> Option<Cow<'_, AsymmetricPublicKey>> {
        match self {
            KeyObjectHandle::AsymmetricPrivate(key) => Some(Cow::Owned(key.to_public_key())),
            KeyObjectHandle::AsymmetricPublic(key) => Some(Cow::Borrowed(key)),
            _ => None,
        }
    }

    /// Returns the secret key if the handle is a secret key.
    pub fn as_secret_key(&self) -> Option<&[u8]> {
        match self {
            KeyObjectHandle::Secret(key) => Some(key),
            _ => None,
        }
    }
}

impl AsymmetricPrivateKey {
    /// Derives the public key from the private key.
    pub fn to_public_key(&self) -> AsymmetricPublicKey {
        match self {
            AsymmetricPrivateKey::Rsa(key) => AsymmetricPublicKey::Rsa(key.to_public_key()),
            AsymmetricPrivateKey::RsaPss(key) => AsymmetricPublicKey::RsaPss(key.to_public_key()),
            AsymmetricPrivateKey::Dsa(key) => AsymmetricPublicKey::Dsa(key.verifying_key().clone()),
            AsymmetricPrivateKey::Ec(key) => AsymmetricPublicKey::Ec(key.to_public_key()),
            AsymmetricPrivateKey::X25519(key) => {
                AsymmetricPublicKey::X25519(x25519_dalek::PublicKey::from(key))
            }
            AsymmetricPrivateKey::Ed25519(key) => AsymmetricPublicKey::Ed25519(key.verifying_key()),
            AsymmetricPrivateKey::Dh(_) => {
                panic!("cannot derive public key from DH private key")
            }
        }
    }
}

impl RsaPssPrivateKey {
    /// Derives the public key from the private key.
    pub fn to_public_key(&self) -> RsaPssPublicKey {
        RsaPssPublicKey {
            key: self.key.to_public_key(),
            details: self.details,
        }
    }
}

impl EcPublicKey {
    pub fn to_jwk(&self) -> Result<elliptic_curve::JwkEcKey, AnyError> {
        match self {
            EcPublicKey::P224(_) => Err(type_error("Unsupported JWK EC curve: P224")),
            EcPublicKey::P256(key) => Ok(key.to_jwk()),
            EcPublicKey::P384(key) => Ok(key.to_jwk()),
        }
    }
}

impl EcPrivateKey {
    /// Derives the public key from the private key.
    pub fn to_public_key(&self) -> EcPublicKey {
        match self {
            EcPrivateKey::P224(key) => EcPublicKey::P224(key.public_key()),
            EcPrivateKey::P256(key) => EcPublicKey::P256(key.public_key()),
            EcPrivateKey::P384(key) => EcPublicKey::P384(key.public_key()),
        }
    }
}

// https://oidref.com/
const ID_SHA1_OID: rsa::pkcs8::ObjectIdentifier =
    rsa::pkcs8::ObjectIdentifier::new_unwrap("1.3.14.3.2.26");
const ID_SHA224_OID: rsa::pkcs8::ObjectIdentifier =
    rsa::pkcs8::ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.4");
const ID_SHA256_OID: rsa::pkcs8::ObjectIdentifier =
    rsa::pkcs8::ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.1");
const ID_SHA384_OID: rsa::pkcs8::ObjectIdentifier =
    rsa::pkcs8::ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.2");
const ID_SHA512_OID: rsa::pkcs8::ObjectIdentifier =
    rsa::pkcs8::ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.3");
const ID_SHA512_224_OID: rsa::pkcs8::ObjectIdentifier =
    rsa::pkcs8::ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.5");
const ID_SHA512_256_OID: rsa::pkcs8::ObjectIdentifier =
    rsa::pkcs8::ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.6");

const ID_MFG1: rsa::pkcs8::ObjectIdentifier =
    rsa::pkcs8::ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.8");
pub const ID_SECP224R1_OID: const_oid::ObjectIdentifier =
    const_oid::ObjectIdentifier::new_unwrap("1.3.132.0.33");
pub const ID_SECP256R1_OID: const_oid::ObjectIdentifier =
    const_oid::ObjectIdentifier::new_unwrap("1.2.840.10045.3.1.7");
pub const ID_SECP384R1_OID: const_oid::ObjectIdentifier =
    const_oid::ObjectIdentifier::new_unwrap("1.3.132.0.34");

pub const RSA_ENCRYPTION_OID: const_oid::ObjectIdentifier =
    const_oid::ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
pub const RSASSA_PSS_OID: const_oid::ObjectIdentifier =
    const_oid::ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.10");
pub const DSA_OID: const_oid::ObjectIdentifier =
    const_oid::ObjectIdentifier::new_unwrap("1.2.840.10040.4.1");
pub const EC_OID: const_oid::ObjectIdentifier =
    const_oid::ObjectIdentifier::new_unwrap("1.2.840.10045.2.1");
pub const X25519_OID: const_oid::ObjectIdentifier =
    const_oid::ObjectIdentifier::new_unwrap("1.3.101.110");
pub const ED25519_OID: const_oid::ObjectIdentifier =
    const_oid::ObjectIdentifier::new_unwrap("1.3.101.112");
pub const DH_KEY_AGREEMENT_OID: const_oid::ObjectIdentifier =
    const_oid::ObjectIdentifier::new_unwrap("1.2.840.113549.1.3.1");

// The parameters field associated with OID id-RSASSA-PSS
// Defined in RFC 3447, section A.2.3
//
// RSASSA-PSS-params ::= SEQUENCE {
//   hashAlgorithm      [0] HashAlgorithm    DEFAULT sha1,
//   maskGenAlgorithm   [1] MaskGenAlgorithm DEFAULT mgf1SHA1,
//   saltLength         [2] INTEGER          DEFAULT 20,
//   trailerField       [3] TrailerField     DEFAULT trailerFieldBC
// }
pub struct RsaPssParameters<'a> {
    pub hash_algorithm: Option<rsa::pkcs8::AlgorithmIdentifierRef<'a>>,
    pub mask_gen_algorithm: Option<rsa::pkcs8::AlgorithmIdentifierRef<'a>>,
    pub salt_length: Option<u32>,
}

// Context-specific tag number for hashAlgorithm.
const HASH_ALGORITHM_TAG: rsa::pkcs8::der::TagNumber = rsa::pkcs8::der::TagNumber::new(0);

// Context-specific tag number for maskGenAlgorithm.
const MASK_GEN_ALGORITHM_TAG: rsa::pkcs8::der::TagNumber = rsa::pkcs8::der::TagNumber::new(1);

// Context-specific tag number for saltLength.
const SALT_LENGTH_TAG: rsa::pkcs8::der::TagNumber = rsa::pkcs8::der::TagNumber::new(2);

impl<'a> TryFrom<rsa::pkcs8::der::asn1::AnyRef<'a>> for RsaPssParameters<'a> {
    type Error = rsa::pkcs8::der::Error;

    fn try_from(
        any: rsa::pkcs8::der::asn1::AnyRef<'a>,
    ) -> rsa::pkcs8::der::Result<RsaPssParameters> {
        any.sequence(|decoder| {
            let hash_algorithm = decoder
                .context_specific::<rsa::pkcs8::AlgorithmIdentifierRef>(
                    HASH_ALGORITHM_TAG,
                    pkcs8::der::TagMode::Explicit,
                )?
                .map(TryInto::try_into)
                .transpose()?;

            let mask_gen_algorithm = decoder
                .context_specific::<rsa::pkcs8::AlgorithmIdentifierRef>(
                    MASK_GEN_ALGORITHM_TAG,
                    pkcs8::der::TagMode::Explicit,
                )?
                .map(TryInto::try_into)
                .transpose()?;

            let salt_length = decoder
                .context_specific::<u32>(SALT_LENGTH_TAG, pkcs8::der::TagMode::Explicit)?
                .map(TryInto::try_into)
                .transpose()?;

            Ok(Self {
                hash_algorithm,
                mask_gen_algorithm,
                salt_length,
            })
        })
    }
}

impl KeyObjectHandle {
    pub fn new_asymmetric_private_key_from_js(
        key: &[u8],
        format: &str,
        typ: &str,
        passphrase: Option<&[u8]>,
    ) -> Result<KeyObjectHandle, AnyError> {
        let document = match format {
            "pem" => {
                let pem = std::str::from_utf8(key).map_err(|err| {
                    type_error(format!(
                        "invalid PEM private key: not valid utf8 starting at byte {}",
                        err.valid_up_to()
                    ))
                })?;

                if let Some(passphrase) = passphrase {
                    SecretDocument::from_pkcs8_encrypted_pem(pem, passphrase)
                        .map_err(|_| type_error("invalid encrypted PEM private key"))?
                } else {
                    let (label, doc) = SecretDocument::from_pem(pem)
                        .map_err(|_| type_error("invalid PEM private key"))?;

                    match label {
                        EncryptedPrivateKeyInfo::PEM_LABEL => {
                            return Err(type_error(
                                "encrypted private key requires a passphrase to decrypt",
                            ))
                        }
                        PrivateKeyInfo::PEM_LABEL => doc,
                        rsa::pkcs1::RsaPrivateKey::PEM_LABEL => {
                            SecretDocument::from_pkcs1_der(doc.as_bytes())
                                .map_err(|_| type_error("invalid PKCS#1 private key"))?
                        }
                        sec1::EcPrivateKey::PEM_LABEL => {
                            SecretDocument::from_sec1_der(doc.as_bytes())
                                .map_err(|_| type_error("invalid SEC1 private key"))?
                        }
                        _ => return Err(type_error(format!("unsupported PEM label: {}", label))),
                    }
                }
            }
            "der" => match typ {
                "pkcs8" => {
                    if let Some(passphrase) = passphrase {
                        SecretDocument::from_pkcs8_encrypted_der(key, passphrase)
                            .map_err(|_| type_error("invalid encrypted PKCS#8 private key"))?
                    } else {
                        SecretDocument::from_pkcs8_der(key)
                            .map_err(|_| type_error("invalid PKCS#8 private key"))?
                    }
                }
                "pkcs1" => {
                    if passphrase.is_some() {
                        return Err(type_error(
                            "PKCS#1 private key does not support encryption with passphrase",
                        ));
                    }
                    SecretDocument::from_pkcs1_der(key)
                        .map_err(|_| type_error("invalid PKCS#1 private key"))?
                }
                "sec1" => {
                    if passphrase.is_some() {
                        return Err(type_error(
                            "SEC1 private key does not support encryption with passphrase",
                        ));
                    }
                    SecretDocument::from_sec1_der(key)
                        .map_err(|_| type_error("invalid SEC1 private key"))?
                }
                _ => return Err(type_error(format!("unsupported key type: {}", typ))),
            },
            _ => return Err(type_error(format!("unsupported key format: {}", format))),
        };

        let pk_info = PrivateKeyInfo::try_from(document.as_bytes())
            .map_err(|_| type_error("invalid private key"))?;

        let alg = pk_info.algorithm.oid;
        let private_key = match alg {
            RSA_ENCRYPTION_OID => {
                let private_key = rsa::RsaPrivateKey::from_pkcs1_der(pk_info.private_key)
                    .map_err(|_| type_error("invalid PKCS#1 private key"))?;
                AsymmetricPrivateKey::Rsa(private_key)
            }
            RSASSA_PSS_OID => {
                let details = parse_rsa_pss_params(pk_info.algorithm.parameters)?;
                let private_key = rsa::RsaPrivateKey::from_pkcs1_der(pk_info.private_key)
                    .map_err(|_| type_error("invalid PKCS#1 private key"))?;
                AsymmetricPrivateKey::RsaPss(RsaPssPrivateKey {
                    key: private_key,
                    details,
                })
            }
            DSA_OID => {
                let private_key = dsa::SigningKey::try_from(pk_info)
                    .map_err(|_| type_error("invalid DSA private key"))?;
                AsymmetricPrivateKey::Dsa(private_key)
            }
            EC_OID => {
                let named_curve = pk_info
                    .algorithm
                    .parameters_oid()
                    .map_err(|_| type_error("malformed or missing named curve in ec parameters"))?;
                match named_curve {
                    ID_SECP224R1_OID => {
                        let secret_key = p224::SecretKey::from_sec1_der(pk_info.private_key)
                            .map_err(|_| type_error("invalid SEC1 private key"))?;
                        AsymmetricPrivateKey::Ec(EcPrivateKey::P224(secret_key))
                    }
                    ID_SECP256R1_OID => {
                        let secret_key = p256::SecretKey::from_sec1_der(pk_info.private_key)
                            .map_err(|_| type_error("invalid SEC1 private key"))?;
                        AsymmetricPrivateKey::Ec(EcPrivateKey::P256(secret_key))
                    }
                    ID_SECP384R1_OID => {
                        let secret_key = p384::SecretKey::from_sec1_der(pk_info.private_key)
                            .map_err(|_| type_error("invalid SEC1 private key"))?;
                        AsymmetricPrivateKey::Ec(EcPrivateKey::P384(secret_key))
                    }
                    _ => return Err(type_error("unsupported ec named curve")),
                }
            }
            X25519_OID => {
                let string_ref = OctetStringRef::from_der(pk_info.private_key)
                    .map_err(|_| type_error("invalid x25519 private key"))?;
                if string_ref.as_bytes().len() != 32 {
                    return Err(type_error("x25519 private key is the wrong length"));
                }
                let mut bytes = [0; 32];
                bytes.copy_from_slice(string_ref.as_bytes());
                AsymmetricPrivateKey::X25519(x25519_dalek::StaticSecret::from(bytes))
            }
            ED25519_OID => {
                let signing_key = ed25519_dalek::SigningKey::try_from(pk_info)
                    .map_err(|_| type_error("invalid Ed25519 private key"))?;
                AsymmetricPrivateKey::Ed25519(signing_key)
            }
            DH_KEY_AGREEMENT_OID => {
                let params = pk_info
                    .algorithm
                    .parameters
                    .ok_or_else(|| type_error("missing dh parameters"))?;
                let params = pkcs3::DhParameter::from_der(&params.to_der().unwrap())
                    .map_err(|_| type_error("malformed dh parameters"))?;
                AsymmetricPrivateKey::Dh(DhPrivateKey {
                    key: dh::PrivateKey::from_bytes(pk_info.private_key),
                    params,
                })
            }
            _ => return Err(type_error("unsupported private key oid")),
        };

        Ok(KeyObjectHandle::AsymmetricPrivate(private_key))
    }

    pub fn new_x509_public_key(
        spki: &x509::SubjectPublicKeyInfo,
    ) -> Result<KeyObjectHandle, AnyError> {
        use x509_parser::der_parser::asn1_rs::oid;
        use x509_parser::public_key::PublicKey;

        let key = match spki.parsed()? {
            PublicKey::RSA(key) => {
                let public_key = RsaPublicKey::new(
                    rsa::BigUint::from_bytes_be(key.modulus),
                    rsa::BigUint::from_bytes_be(key.exponent),
                )?;
                AsymmetricPublicKey::Rsa(public_key)
            }
            PublicKey::EC(point) => {
                let data = point.data();
                if let Some(params) = &spki.algorithm.parameters {
                    let curve_oid = params.as_oid()?;
                    const ID_SECP224R1: &[u8] = &oid!(raw 1.3.132.0.33);
                    const ID_SECP256R1: &[u8] = &oid!(raw 1.2.840.10045.3.1.7);
                    const ID_SECP384R1: &[u8] = &oid!(raw 1.3.132.0.34);

                    match curve_oid.as_bytes() {
                        ID_SECP224R1 => {
                            let public_key = p224::PublicKey::from_sec1_bytes(data)?;
                            AsymmetricPublicKey::Ec(EcPublicKey::P224(public_key))
                        }
                        ID_SECP256R1 => {
                            let public_key = p256::PublicKey::from_sec1_bytes(data)?;
                            AsymmetricPublicKey::Ec(EcPublicKey::P256(public_key))
                        }
                        ID_SECP384R1 => {
                            let public_key = p384::PublicKey::from_sec1_bytes(data)?;
                            AsymmetricPublicKey::Ec(EcPublicKey::P384(public_key))
                        }
                        _ => return Err(type_error("unsupported ec named curve")),
                    }
                } else {
                    return Err(type_error("missing ec parameters"));
                }
            }
            PublicKey::DSA(_) => {
                let verifying_key = dsa::VerifyingKey::from_public_key_der(spki.raw)
                    .map_err(|_| type_error("malformed DSS public key"))?;
                AsymmetricPublicKey::Dsa(verifying_key)
            }
            _ => return Err(type_error("unsupported x509 public key type")),
        };

        Ok(KeyObjectHandle::AsymmetricPublic(key))
    }

    pub fn new_rsa_jwk(jwk: RsaJwkKey, is_public: bool) -> Result<KeyObjectHandle, AnyError> {
        use base64::prelude::BASE64_URL_SAFE_NO_PAD;

        let n = BASE64_URL_SAFE_NO_PAD.decode(jwk.n.as_bytes())?;
        let e = BASE64_URL_SAFE_NO_PAD.decode(jwk.e.as_bytes())?;

        if is_public {
            let public_key = RsaPublicKey::new(
                rsa::BigUint::from_bytes_be(&n),
                rsa::BigUint::from_bytes_be(&e),
            )?;

            Ok(KeyObjectHandle::AsymmetricPublic(AsymmetricPublicKey::Rsa(
                public_key,
            )))
        } else {
            let d = BASE64_URL_SAFE_NO_PAD.decode(
                jwk.d
                    .ok_or_else(|| type_error("missing RSA private component"))?
                    .as_bytes(),
            )?;
            let p = BASE64_URL_SAFE_NO_PAD.decode(
                jwk.p
                    .ok_or_else(|| type_error("missing RSA private component"))?
                    .as_bytes(),
            )?;
            let q = BASE64_URL_SAFE_NO_PAD.decode(
                jwk.q
                    .ok_or_else(|| type_error("missing RSA private component"))?
                    .as_bytes(),
            )?;

            let mut private_key = RsaPrivateKey::from_components(
                rsa::BigUint::from_bytes_be(&n),
                rsa::BigUint::from_bytes_be(&e),
                rsa::BigUint::from_bytes_be(&d),
                vec![
                    rsa::BigUint::from_bytes_be(&p),
                    rsa::BigUint::from_bytes_be(&q),
                ],
            )?;
            private_key.precompute()?; // precompute CRT params

            Ok(KeyObjectHandle::AsymmetricPrivate(
                AsymmetricPrivateKey::Rsa(private_key),
            ))
        }
    }

    pub fn new_ec_jwk(jwk: &JwkEcKey, is_public: bool) -> Result<KeyObjectHandle, AnyError> {
        //  https://datatracker.ietf.org/doc/html/rfc7518#section-6.2.1.1
        let handle = match jwk.crv() {
            "P-256" if is_public => KeyObjectHandle::AsymmetricPublic(AsymmetricPublicKey::Ec(
                EcPublicKey::P256(p256::PublicKey::from_jwk(jwk)?),
            )),
            "P-256" => KeyObjectHandle::AsymmetricPrivate(AsymmetricPrivateKey::Ec(
                EcPrivateKey::P256(p256::SecretKey::from_jwk(jwk)?),
            )),
            "P-384" if is_public => KeyObjectHandle::AsymmetricPublic(AsymmetricPublicKey::Ec(
                EcPublicKey::P384(p384::PublicKey::from_jwk(jwk)?),
            )),
            "P-384" => KeyObjectHandle::AsymmetricPrivate(AsymmetricPrivateKey::Ec(
                EcPrivateKey::P384(p384::SecretKey::from_jwk(jwk)?),
            )),
            _ => {
                return Err(type_error(format!("unsupported curve: {}", jwk.crv())));
            }
        };

        Ok(handle)
    }

    pub fn new_ed_raw(
        curve: &str,
        data: &[u8],
        is_public: bool,
    ) -> Result<KeyObjectHandle, AnyError> {
        match curve {
            "Ed25519" => {
                let data = data
                    .try_into()
                    .map_err(|_| type_error("invalid Ed25519 key"))?;
                if !is_public {
                    Ok(KeyObjectHandle::AsymmetricPrivate(
                        AsymmetricPrivateKey::Ed25519(ed25519_dalek::SigningKey::from_bytes(data)),
                    ))
                } else {
                    Ok(KeyObjectHandle::AsymmetricPublic(
                        AsymmetricPublicKey::Ed25519(ed25519_dalek::VerifyingKey::from_bytes(
                            data,
                        )?),
                    ))
                }
            }
            "X25519" => {
                let data: [u8; 32] = data
                    .try_into()
                    .map_err(|_| type_error("invalid x25519 key"))?;
                if !is_public {
                    Ok(KeyObjectHandle::AsymmetricPrivate(
                        AsymmetricPrivateKey::X25519(x25519_dalek::StaticSecret::from(data)),
                    ))
                } else {
                    Ok(KeyObjectHandle::AsymmetricPublic(
                        AsymmetricPublicKey::X25519(x25519_dalek::PublicKey::from(data)),
                    ))
                }
            }
            _ => Err(type_error("unsupported curve")),
        }
    }

    pub fn new_asymmetric_public_key_from_js(
        key: &[u8],
        format: &str,
        typ: &str,
        passphrase: Option<&[u8]>,
    ) -> Result<KeyObjectHandle, AnyError> {
        let document = match format {
            "pem" => {
                let pem = std::str::from_utf8(key).map_err(|err| {
                    type_error(format!(
                        "invalid PEM public key: not valid utf8 starting at byte {}",
                        err.valid_up_to()
                    ))
                })?;

                let (label, document) =
                    Document::from_pem(pem).map_err(|_| type_error("invalid PEM public key"))?;

                match label {
                    SubjectPublicKeyInfoRef::PEM_LABEL => document,
                    rsa::pkcs1::RsaPublicKey::PEM_LABEL => {
                        Document::from_pkcs1_der(document.as_bytes())
                            .map_err(|_| type_error("invalid PKCS#1 public key"))?
                    }
                    EncryptedPrivateKeyInfo::PEM_LABEL
                    | PrivateKeyInfo::PEM_LABEL
                    | sec1::EcPrivateKey::PEM_LABEL
                    | rsa::pkcs1::RsaPrivateKey::PEM_LABEL => {
                        let handle = KeyObjectHandle::new_asymmetric_private_key_from_js(
                            key, format, typ, passphrase,
                        )?;
                        match handle {
                            KeyObjectHandle::AsymmetricPrivate(private) => {
                                return Ok(KeyObjectHandle::AsymmetricPublic(
                                    private.to_public_key(),
                                ))
                            }
                            KeyObjectHandle::AsymmetricPublic(_) | KeyObjectHandle::Secret(_) => {
                                unreachable!()
                            }
                        }
                    }
                    // TODO: handle x509 certificates as public keys
                    _ => return Err(type_error(format!("unsupported PEM label: {}", label))),
                }
            }
            "der" => match typ {
                "pkcs1" => Document::from_pkcs1_der(key)
                    .map_err(|_| type_error("invalid PKCS#1 public key"))?,
                "spki" => Document::from_public_key_der(key)
                    .map_err(|_| type_error("invalid SPKI public key"))?,
                _ => return Err(type_error(format!("unsupported key type: {}", typ))),
            },
            _ => return Err(type_error(format!("unsupported key format: {}", format))),
        };

        let spki = SubjectPublicKeyInfoRef::try_from(document.as_bytes())?;

        let public_key =
            match spki.algorithm.oid {
                RSA_ENCRYPTION_OID => {
                    let public_key =
                        RsaPublicKey::from_pkcs1_der(spki.subject_public_key.as_bytes().unwrap())?;
                    AsymmetricPublicKey::Rsa(public_key)
                }
                RSASSA_PSS_OID => {
                    let details = parse_rsa_pss_params(spki.algorithm.parameters)?;
                    let public_key =
                        RsaPublicKey::from_pkcs1_der(spki.subject_public_key.as_bytes().unwrap())?;
                    AsymmetricPublicKey::RsaPss(RsaPssPublicKey {
                        key: public_key,
                        details,
                    })
                }
                DSA_OID => {
                    let verifying_key = dsa::VerifyingKey::try_from(spki)
                        .map_err(|_| type_error("malformed DSS public key"))?;
                    AsymmetricPublicKey::Dsa(verifying_key)
                }
                EC_OID => {
                    let named_curve = spki.algorithm.parameters_oid().map_err(|_| {
                        type_error("malformed or missing named curve in ec parameters")
                    })?;
                    let data = spki
                        .subject_public_key
                        .as_bytes()
                        .ok_or_else(|| type_error("malformed or missing public key in ec spki"))?;

                    match named_curve {
                        ID_SECP224R1_OID => {
                            let public_key = p224::PublicKey::from_sec1_bytes(data)?;
                            AsymmetricPublicKey::Ec(EcPublicKey::P224(public_key))
                        }
                        ID_SECP256R1_OID => {
                            let public_key = p256::PublicKey::from_sec1_bytes(data)?;
                            AsymmetricPublicKey::Ec(EcPublicKey::P256(public_key))
                        }
                        ID_SECP384R1_OID => {
                            let public_key = p384::PublicKey::from_sec1_bytes(data)?;
                            AsymmetricPublicKey::Ec(EcPublicKey::P384(public_key))
                        }
                        _ => return Err(type_error("unsupported ec named curve")),
                    }
                }
                X25519_OID => {
                    let mut bytes = [0; 32];
                    let data = spki.subject_public_key.as_bytes().ok_or_else(|| {
                        type_error("malformed or missing public key in x25519 spki")
                    })?;
                    if data.len() < 32 {
                        return Err(type_error("x25519 public key is too short"));
                    }
                    bytes.copy_from_slice(&data[0..32]);
                    AsymmetricPublicKey::X25519(x25519_dalek::PublicKey::from(bytes))
                }
                ED25519_OID => {
                    let verifying_key = ed25519_dalek::VerifyingKey::try_from(spki)
                        .map_err(|_| type_error("invalid Ed25519 private key"))?;
                    AsymmetricPublicKey::Ed25519(verifying_key)
                }
                DH_KEY_AGREEMENT_OID => {
                    let params = spki
                        .algorithm
                        .parameters
                        .ok_or_else(|| type_error("missing dh parameters"))?;
                    let params = pkcs3::DhParameter::from_der(&params.to_der().unwrap())
                        .map_err(|_| type_error("malformed dh parameters"))?;
                    let Some(subject_public_key) = spki.subject_public_key.as_bytes() else {
                        return Err(type_error("malformed or missing public key in dh spki"));
                    };
                    AsymmetricPublicKey::Dh(DhPublicKey {
                        key: dh::PublicKey::from_bytes(subject_public_key),
                        params,
                    })
                }
                _ => return Err(type_error("unsupported public key oid")),
            };

        Ok(KeyObjectHandle::AsymmetricPublic(public_key))
    }
}

fn parse_rsa_pss_params(
    parameters: Option<AnyRef<'_>>,
) -> Result<Option<RsaPssDetails>, deno_core::anyhow::Error> {
    let details = if let Some(parameters) = parameters {
        let params = RsaPssParameters::try_from(parameters)
            .map_err(|_| type_error("malformed pss private key parameters"))?;

        let hash_algorithm = match params.hash_algorithm.map(|k| k.oid) {
            Some(ID_SHA1_OID) => RsaPssHashAlgorithm::Sha1,
            Some(ID_SHA224_OID) => RsaPssHashAlgorithm::Sha224,
            Some(ID_SHA256_OID) => RsaPssHashAlgorithm::Sha256,
            Some(ID_SHA384_OID) => RsaPssHashAlgorithm::Sha384,
            Some(ID_SHA512_OID) => RsaPssHashAlgorithm::Sha512,
            Some(ID_SHA512_224_OID) => RsaPssHashAlgorithm::Sha512_224,
            Some(ID_SHA512_256_OID) => RsaPssHashAlgorithm::Sha512_256,
            None => RsaPssHashAlgorithm::Sha1,
            _ => return Err(type_error("unsupported pss hash algorithm")),
        };

        let mf1_hash_algorithm = match params.mask_gen_algorithm {
            Some(alg) => {
                if alg.oid != ID_MFG1 {
                    return Err(type_error("unsupported pss mask gen algorithm"));
                }
                let params = alg.parameters_oid().map_err(|_| {
                    type_error("malformed or missing pss mask gen algorithm parameters")
                })?;
                match params {
                    ID_SHA1_OID => RsaPssHashAlgorithm::Sha1,
                    ID_SHA224_OID => RsaPssHashAlgorithm::Sha224,
                    ID_SHA256_OID => RsaPssHashAlgorithm::Sha256,
                    ID_SHA384_OID => RsaPssHashAlgorithm::Sha384,
                    ID_SHA512_OID => RsaPssHashAlgorithm::Sha512,
                    ID_SHA512_224_OID => RsaPssHashAlgorithm::Sha512_224,
                    ID_SHA512_256_OID => RsaPssHashAlgorithm::Sha512_256,
                    _ => return Err(type_error("unsupported pss mask gen algorithm")),
                }
            }
            None => hash_algorithm,
        };

        let salt_length = params
            .salt_length
            .unwrap_or_else(|| hash_algorithm.salt_length());

        Some(RsaPssDetails {
            hash_algorithm,
            mf1_hash_algorithm,
            salt_length,
        })
    } else {
        None
    };
    Ok(details)
}

use base64::prelude::BASE64_URL_SAFE_NO_PAD;

fn bytes_to_b64(bytes: &[u8]) -> String {
    BASE64_URL_SAFE_NO_PAD.encode(bytes)
}

impl AsymmetricPublicKey {
    fn export_jwk(&self) -> Result<deno_core::serde_json::Value, AnyError> {
        match self {
            AsymmetricPublicKey::Ec(key) => {
                let jwk = key.to_jwk()?;
                Ok(deno_core::serde_json::json!(jwk))
            }
            AsymmetricPublicKey::X25519(key) => {
                let bytes = key.as_bytes();
                let jwk = deno_core::serde_json::json!({
                    "kty": "OKP",
                    "crv": "X25519",
                    "x": bytes_to_b64(bytes),
                });
                Ok(jwk)
            }
            AsymmetricPublicKey::Ed25519(key) => {
                let bytes = key.to_bytes();
                let jwk = deno_core::serde_json::json!({
                    "kty": "OKP",
                    "crv": "Ed25519",
                    "x": bytes_to_b64(&bytes),
                });
                Ok(jwk)
            }
            AsymmetricPublicKey::Rsa(key) => {
                let n = key.n();
                let e = key.e();

                let jwk = deno_core::serde_json::json!({
                    "kty": "RSA",
                    "n": bytes_to_b64(&n.to_bytes_be()),
                    "e": bytes_to_b64(&e.to_bytes_be()),
                });
                Ok(jwk)
            }
            AsymmetricPublicKey::RsaPss(key) => {
                let n = key.key.n();
                let e = key.key.e();

                let jwk = deno_core::serde_json::json!({
                    "kty": "RSA",
                    "n": bytes_to_b64(&n.to_bytes_be()),
                    "e": bytes_to_b64(&e.to_bytes_be()),
                });
                Ok(jwk)
            }
            _ => Err(type_error("jwk export not implemented for this key type")),
        }
    }

    fn export_der(&self, typ: &str) -> Result<Box<[u8]>, AnyError> {
        match typ {
            "pkcs1" => match self {
                AsymmetricPublicKey::Rsa(key) => {
                    let der = key
                        .to_pkcs1_der()
                        .map_err(|_| type_error("invalid RSA public key"))?
                        .into_vec()
                        .into_boxed_slice();
                    Ok(der)
                }
                _ => Err(type_error(
                    "exporting non-RSA public key as PKCS#1 is not supported",
                )),
            },
            "spki" => {
                let der = match self {
                    AsymmetricPublicKey::Rsa(key) => key
                        .to_public_key_der()
                        .map_err(|_| type_error("invalid RSA public key"))?
                        .into_vec()
                        .into_boxed_slice(),
                    AsymmetricPublicKey::RsaPss(_key) => {
                        return Err(generic_error(
                            "exporting RSA-PSS public key as SPKI is not supported yet",
                        ))
                    }
                    AsymmetricPublicKey::Dsa(key) => key
                        .to_public_key_der()
                        .map_err(|_| type_error("invalid DSA public key"))?
                        .into_vec()
                        .into_boxed_slice(),
                    AsymmetricPublicKey::Ec(key) => {
                        let (sec1, oid) = match key {
                            EcPublicKey::P224(key) => (key.to_sec1_bytes(), ID_SECP224R1_OID),
                            EcPublicKey::P256(key) => (key.to_sec1_bytes(), ID_SECP256R1_OID),
                            EcPublicKey::P384(key) => (key.to_sec1_bytes(), ID_SECP384R1_OID),
                        };

                        let spki = SubjectPublicKeyInfoRef {
                            algorithm: rsa::pkcs8::AlgorithmIdentifierRef {
                                oid: EC_OID,
                                parameters: Some(asn1::AnyRef::from(&oid)),
                            },
                            subject_public_key: BitStringRef::from_bytes(&sec1)
                                .map_err(|_| type_error("invalid EC public key"))?,
                        };

                        spki.to_der()
                            .map_err(|_| type_error("invalid EC public key"))?
                            .into_boxed_slice()
                    }
                    AsymmetricPublicKey::X25519(key) => {
                        let spki = SubjectPublicKeyInfoRef {
                            algorithm: rsa::pkcs8::AlgorithmIdentifierRef {
                                oid: X25519_OID,
                                parameters: None,
                            },
                            subject_public_key: BitStringRef::from_bytes(key.as_bytes())
                                .map_err(|_| type_error("invalid X25519 public key"))?,
                        };

                        spki.to_der()
                            .map_err(|_| type_error("invalid X25519 public key"))?
                            .into_boxed_slice()
                    }
                    AsymmetricPublicKey::Ed25519(key) => {
                        let spki = SubjectPublicKeyInfoRef {
                            algorithm: rsa::pkcs8::AlgorithmIdentifierRef {
                                oid: ED25519_OID,
                                parameters: None,
                            },
                            subject_public_key: BitStringRef::from_bytes(key.as_bytes())
                                .map_err(|_| type_error("invalid Ed25519 public key"))?,
                        };

                        spki.to_der()
                            .map_err(|_| type_error("invalid Ed25519 public key"))?
                            .into_boxed_slice()
                    }
                    AsymmetricPublicKey::Dh(key) => {
                        let public_key_bytes = key.key.clone().into_vec();
                        let params = key.params.to_der().unwrap();
                        let spki = SubjectPublicKeyInfoRef {
                            algorithm: rsa::pkcs8::AlgorithmIdentifierRef {
                                oid: DH_KEY_AGREEMENT_OID,
                                parameters: Some(AnyRef::new(Tag::Sequence, &params).unwrap()),
                            },
                            subject_public_key: BitStringRef::from_bytes(&public_key_bytes)
                                .map_err(|_| type_error("invalid DH public key"))?,
                        };
                        spki.to_der()
                            .map_err(|_| type_error("invalid DH public key"))?
                            .into_boxed_slice()
                    }
                };
                Ok(der)
            }
            _ => Err(type_error(format!("unsupported key type: {}", typ))),
        }
    }
}

impl AsymmetricPrivateKey {
    fn export_der(
        &self,
        typ: &str,
        // cipher: Option<&str>,
        // passphrase: Option<&str>,
    ) -> Result<Box<[u8]>, AnyError> {
        match typ {
            "pkcs1" => match self {
                AsymmetricPrivateKey::Rsa(key) => {
                    let der = key
                        .to_pkcs1_der()
                        .map_err(|_| type_error("invalid RSA private key"))?
                        .to_bytes()
                        .to_vec()
                        .into_boxed_slice();
                    Ok(der)
                }
                _ => Err(type_error(
                    "exporting non-RSA private key as PKCS#1 is not supported",
                )),
            },
            "sec1" => match self {
                AsymmetricPrivateKey::Ec(key) => {
                    let sec1 = match key {
                        EcPrivateKey::P224(key) => key.to_sec1_der(),
                        EcPrivateKey::P256(key) => key.to_sec1_der(),
                        EcPrivateKey::P384(key) => key.to_sec1_der(),
                    }
                    .map_err(|_| type_error("invalid EC private key"))?;
                    Ok(sec1.to_vec().into_boxed_slice())
                }
                _ => Err(type_error(
                    "exporting non-EC private key as SEC1 is not supported",
                )),
            },
            "pkcs8" => {
                let der = match self {
                    AsymmetricPrivateKey::Rsa(key) => {
                        let document = key
                            .to_pkcs8_der()
                            .map_err(|_| type_error("invalid RSA private key"))?;
                        document.to_bytes().to_vec().into_boxed_slice()
                    }
                    AsymmetricPrivateKey::RsaPss(_key) => {
                        return Err(generic_error(
                            "exporting RSA-PSS private key as PKCS#8 is not supported yet",
                        ))
                    }
                    AsymmetricPrivateKey::Dsa(key) => {
                        let document = key
                            .to_pkcs8_der()
                            .map_err(|_| type_error("invalid DSA private key"))?;
                        document.to_bytes().to_vec().into_boxed_slice()
                    }
                    AsymmetricPrivateKey::Ec(key) => {
                        let document = match key {
                            EcPrivateKey::P224(key) => key.to_pkcs8_der(),
                            EcPrivateKey::P256(key) => key.to_pkcs8_der(),
                            EcPrivateKey::P384(key) => key.to_pkcs8_der(),
                        }
                        .map_err(|_| type_error("invalid EC private key"))?;
                        document.to_bytes().to_vec().into_boxed_slice()
                    }
                    AsymmetricPrivateKey::X25519(key) => {
                        let private_key = OctetStringRef::new(key.as_bytes())
                            .map_err(|_| type_error("invalid X25519 private key"))?
                            .to_der()
                            .map_err(|_| type_error("invalid X25519 private key"))?;

                        let private_key = PrivateKeyInfo {
                            algorithm: rsa::pkcs8::AlgorithmIdentifierRef {
                                oid: X25519_OID,
                                parameters: None,
                            },
                            private_key: &private_key,
                            public_key: None,
                        };

                        let der = private_key
                            .to_der()
                            .map_err(|_| type_error("invalid X25519 private key"))?
                            .into_boxed_slice();
                        return Ok(der);
                    }
                    AsymmetricPrivateKey::Ed25519(key) => {
                        let private_key = OctetStringRef::new(key.as_bytes())
                            .map_err(|_| type_error("invalid Ed25519 private key"))?
                            .to_der()
                            .map_err(|_| type_error("invalid Ed25519 private key"))?;

                        let private_key = PrivateKeyInfo {
                            algorithm: rsa::pkcs8::AlgorithmIdentifierRef {
                                oid: ED25519_OID,
                                parameters: None,
                            },
                            private_key: &private_key,
                            public_key: None,
                        };

                        private_key
                            .to_der()
                            .map_err(|_| type_error("invalid ED25519 private key"))?
                            .into_boxed_slice()
                    }
                    AsymmetricPrivateKey::Dh(key) => {
                        let private_key = key.key.clone().into_vec();
                        let params = key.params.to_der().unwrap();
                        let private_key = PrivateKeyInfo {
                            algorithm: rsa::pkcs8::AlgorithmIdentifierRef {
                                oid: DH_KEY_AGREEMENT_OID,
                                parameters: Some(AnyRef::new(Tag::Sequence, &params).unwrap()),
                            },
                            private_key: &private_key,
                            public_key: None,
                        };

                        private_key
                            .to_der()
                            .map_err(|_| type_error("invalid DH private key"))?
                            .into_boxed_slice()
                    }
                };

                Ok(der)
            }
            _ => Err(type_error(format!("unsupported key type: {}", typ))),
        }
    }
}

#[op2]
#[cppgc]
pub fn op_node_create_private_key(
    #[buffer] key: &[u8],
    #[string] format: &str,
    #[string] typ: &str,
    #[buffer] passphrase: Option<&[u8]>,
) -> Result<KeyObjectHandle, AnyError> {
    KeyObjectHandle::new_asymmetric_private_key_from_js(key, format, typ, passphrase)
}

#[op2]
#[cppgc]
pub fn op_node_create_ed_raw(
    #[string] curve: &str,
    #[buffer] key: &[u8],
    is_public: bool,
) -> Result<KeyObjectHandle, AnyError> {
    KeyObjectHandle::new_ed_raw(curve, key, is_public)
}

#[derive(serde::Deserialize)]
pub struct RsaJwkKey {
    n: String,
    e: String,
    d: Option<String>,
    p: Option<String>,
    q: Option<String>,
}

#[op2]
#[cppgc]
pub fn op_node_create_rsa_jwk(
    #[serde] jwk: RsaJwkKey,
    is_public: bool,
) -> Result<KeyObjectHandle, AnyError> {
    KeyObjectHandle::new_rsa_jwk(jwk, is_public)
}

#[op2]
#[cppgc]
pub fn op_node_create_ec_jwk(
    #[serde] jwk: elliptic_curve::JwkEcKey,
    is_public: bool,
) -> Result<KeyObjectHandle, AnyError> {
    KeyObjectHandle::new_ec_jwk(&jwk, is_public)
}

#[op2]
#[cppgc]
pub fn op_node_create_public_key(
    #[buffer] key: &[u8],
    #[string] format: &str,
    #[string] typ: &str,
    #[buffer] passphrase: Option<&[u8]>,
) -> Result<KeyObjectHandle, AnyError> {
    KeyObjectHandle::new_asymmetric_public_key_from_js(key, format, typ, passphrase)
}

#[op2]
#[cppgc]
pub fn op_node_create_secret_key(#[buffer(copy)] key: Box<[u8]>) -> KeyObjectHandle {
    KeyObjectHandle::Secret(key)
}

#[op2]
#[string]
pub fn op_node_get_asymmetric_key_type(
    #[cppgc] handle: &KeyObjectHandle,
) -> Result<&'static str, AnyError> {
    match handle {
        KeyObjectHandle::AsymmetricPrivate(AsymmetricPrivateKey::Rsa(_))
        | KeyObjectHandle::AsymmetricPublic(AsymmetricPublicKey::Rsa(_)) => Ok("rsa"),
        KeyObjectHandle::AsymmetricPrivate(AsymmetricPrivateKey::RsaPss(_))
        | KeyObjectHandle::AsymmetricPublic(AsymmetricPublicKey::RsaPss(_)) => Ok("rsa-pss"),
        KeyObjectHandle::AsymmetricPrivate(AsymmetricPrivateKey::Dsa(_))
        | KeyObjectHandle::AsymmetricPublic(AsymmetricPublicKey::Dsa(_)) => Ok("dsa"),
        KeyObjectHandle::AsymmetricPrivate(AsymmetricPrivateKey::Ec(_))
        | KeyObjectHandle::AsymmetricPublic(AsymmetricPublicKey::Ec(_)) => Ok("ec"),
        KeyObjectHandle::AsymmetricPrivate(AsymmetricPrivateKey::X25519(_))
        | KeyObjectHandle::AsymmetricPublic(AsymmetricPublicKey::X25519(_)) => Ok("x25519"),
        KeyObjectHandle::AsymmetricPrivate(AsymmetricPrivateKey::Ed25519(_))
        | KeyObjectHandle::AsymmetricPublic(AsymmetricPublicKey::Ed25519(_)) => Ok("ed25519"),
        KeyObjectHandle::AsymmetricPrivate(AsymmetricPrivateKey::Dh(_))
        | KeyObjectHandle::AsymmetricPublic(AsymmetricPublicKey::Dh(_)) => Ok("dh"),
        KeyObjectHandle::Secret(_) => Err(type_error("symmetric key is not an asymmetric key")),
    }
}

#[derive(serde::Serialize)]
#[serde(untagged)]
pub enum AsymmetricKeyDetails {
    #[serde(rename_all = "camelCase")]
    Rsa {
        modulus_length: usize,
        public_exponent: V8BigInt,
    },
    #[serde(rename_all = "camelCase")]
    RsaPss {
        modulus_length: usize,
        public_exponent: V8BigInt,
        hash_algorithm: &'static str,
        mgf1_hash_algorithm: &'static str,
        salt_length: u32,
    },
    #[serde(rename = "rsaPss")]
    RsaPssBasic {
        modulus_length: usize,
        public_exponent: V8BigInt,
    },
    #[serde(rename_all = "camelCase")]
    Dsa {
        modulus_length: usize,
        divisor_length: usize,
    },
    #[serde(rename_all = "camelCase")]
    Ec {
        named_curve: &'static str,
    },
    X25519,
    Ed25519,
    Dh,
}

#[op2]
#[serde]
pub fn op_node_get_asymmetric_key_details(
    #[cppgc] handle: &KeyObjectHandle,
) -> Result<AsymmetricKeyDetails, AnyError> {
    match handle {
        KeyObjectHandle::AsymmetricPrivate(private_key) => match private_key {
            AsymmetricPrivateKey::Rsa(key) => {
                let modulus_length = key.n().bits();
                let public_exponent =
                    BigInt::from_bytes_be(num_bigint::Sign::Plus, &key.e().to_bytes_be());
                Ok(AsymmetricKeyDetails::Rsa {
                    modulus_length,
                    public_exponent: V8BigInt::from(public_exponent),
                })
            }
            AsymmetricPrivateKey::RsaPss(key) => {
                let modulus_length = key.key.n().bits();
                let public_exponent =
                    BigInt::from_bytes_be(num_bigint::Sign::Plus, &key.key.e().to_bytes_be());
                let public_exponent = V8BigInt::from(public_exponent);
                let details = match key.details {
                    Some(details) => AsymmetricKeyDetails::RsaPss {
                        modulus_length,
                        public_exponent,
                        hash_algorithm: details.hash_algorithm.as_str(),
                        mgf1_hash_algorithm: details.mf1_hash_algorithm.as_str(),
                        salt_length: details.salt_length,
                    },
                    None => AsymmetricKeyDetails::RsaPssBasic {
                        modulus_length,
                        public_exponent,
                    },
                };
                Ok(details)
            }
            AsymmetricPrivateKey::Dsa(key) => {
                let components = key.verifying_key().components();
                let modulus_length = components.p().bits();
                let divisor_length = components.q().bits();
                Ok(AsymmetricKeyDetails::Dsa {
                    modulus_length,
                    divisor_length,
                })
            }
            AsymmetricPrivateKey::Ec(key) => {
                let named_curve = match key {
                    EcPrivateKey::P224(_) => "p224",
                    EcPrivateKey::P256(_) => "p256",
                    EcPrivateKey::P384(_) => "p384",
                };
                Ok(AsymmetricKeyDetails::Ec { named_curve })
            }
            AsymmetricPrivateKey::X25519(_) => Ok(AsymmetricKeyDetails::X25519),
            AsymmetricPrivateKey::Ed25519(_) => Ok(AsymmetricKeyDetails::Ed25519),
            AsymmetricPrivateKey::Dh(_) => Ok(AsymmetricKeyDetails::Dh),
        },
        KeyObjectHandle::AsymmetricPublic(public_key) => match public_key {
            AsymmetricPublicKey::Rsa(key) => {
                let modulus_length = key.n().bits();
                let public_exponent =
                    BigInt::from_bytes_be(num_bigint::Sign::Plus, &key.e().to_bytes_be());
                Ok(AsymmetricKeyDetails::Rsa {
                    modulus_length,
                    public_exponent: V8BigInt::from(public_exponent),
                })
            }
            AsymmetricPublicKey::RsaPss(key) => {
                let modulus_length = key.key.n().bits();
                let public_exponent =
                    BigInt::from_bytes_be(num_bigint::Sign::Plus, &key.key.e().to_bytes_be());
                let public_exponent = V8BigInt::from(public_exponent);
                let details = match key.details {
                    Some(details) => AsymmetricKeyDetails::RsaPss {
                        modulus_length,
                        public_exponent,
                        hash_algorithm: details.hash_algorithm.as_str(),
                        mgf1_hash_algorithm: details.mf1_hash_algorithm.as_str(),
                        salt_length: details.salt_length,
                    },
                    None => AsymmetricKeyDetails::RsaPssBasic {
                        modulus_length,
                        public_exponent,
                    },
                };
                Ok(details)
            }
            AsymmetricPublicKey::Dsa(key) => {
                let components = key.components();
                let modulus_length = components.p().bits();
                let divisor_length = components.q().bits();
                Ok(AsymmetricKeyDetails::Dsa {
                    modulus_length,
                    divisor_length,
                })
            }
            AsymmetricPublicKey::Ec(key) => {
                let named_curve = match key {
                    EcPublicKey::P224(_) => "p224",
                    EcPublicKey::P256(_) => "p256",
                    EcPublicKey::P384(_) => "p384",
                };
                Ok(AsymmetricKeyDetails::Ec { named_curve })
            }
            AsymmetricPublicKey::X25519(_) => Ok(AsymmetricKeyDetails::X25519),
            AsymmetricPublicKey::Ed25519(_) => Ok(AsymmetricKeyDetails::Ed25519),
            AsymmetricPublicKey::Dh(_) => Ok(AsymmetricKeyDetails::Dh),
        },
        KeyObjectHandle::Secret(_) => Err(type_error("symmetric key is not an asymmetric key")),
    }
}

#[op2(fast)]
#[smi]
pub fn op_node_get_symmetric_key_size(
    #[cppgc] handle: &KeyObjectHandle,
) -> Result<usize, AnyError> {
    match handle {
        KeyObjectHandle::AsymmetricPrivate(_) => {
            Err(type_error("asymmetric key is not a symmetric key"))
        }
        KeyObjectHandle::AsymmetricPublic(_) => {
            Err(type_error("asymmetric key is not a symmetric key"))
        }
        KeyObjectHandle::Secret(key) => Ok(key.len() * 8),
    }
}

#[op2]
#[cppgc]
pub fn op_node_generate_secret_key(#[smi] len: usize) -> KeyObjectHandle {
    let mut key = vec![0u8; len];
    thread_rng().fill_bytes(&mut key);
    KeyObjectHandle::Secret(key.into_boxed_slice())
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_secret_key_async(#[smi] len: usize) -> KeyObjectHandle {
    spawn_blocking(move || {
        let mut key = vec![0u8; len];
        thread_rng().fill_bytes(&mut key);
        KeyObjectHandle::Secret(key.into_boxed_slice())
    })
    .await
    .unwrap()
}

struct KeyObjectHandlePair {
    private_key: RefCell<Option<KeyObjectHandle>>,
    public_key: RefCell<Option<KeyObjectHandle>>,
}

impl GarbageCollected for KeyObjectHandlePair {}

impl KeyObjectHandlePair {
    pub fn new(private_key: AsymmetricPrivateKey, public_key: AsymmetricPublicKey) -> Self {
        Self {
            private_key: RefCell::new(Some(KeyObjectHandle::AsymmetricPrivate(private_key))),
            public_key: RefCell::new(Some(KeyObjectHandle::AsymmetricPublic(public_key))),
        }
    }
}

#[op2]
#[cppgc]
pub fn op_node_get_public_key_from_pair(
    #[cppgc] pair: &KeyObjectHandlePair,
) -> Option<KeyObjectHandle> {
    pair.public_key.borrow_mut().take()
}

#[op2]
#[cppgc]
pub fn op_node_get_private_key_from_pair(
    #[cppgc] pair: &KeyObjectHandlePair,
) -> Option<KeyObjectHandle> {
    pair.private_key.borrow_mut().take()
}

fn generate_rsa(modulus_length: usize, public_exponent: usize) -> KeyObjectHandlePair {
    let private_key = RsaPrivateKey::new_with_exp(
        &mut thread_rng(),
        modulus_length,
        &rsa::BigUint::from_usize(public_exponent).unwrap(),
    )
    .unwrap();

    let private_key = AsymmetricPrivateKey::Rsa(private_key);
    let public_key = private_key.to_public_key();

    KeyObjectHandlePair::new(private_key, public_key)
}

#[op2]
#[cppgc]
pub fn op_node_generate_rsa_key(
    #[smi] modulus_length: usize,
    #[smi] public_exponent: usize,
) -> KeyObjectHandlePair {
    generate_rsa(modulus_length, public_exponent)
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_rsa_key_async(
    #[smi] modulus_length: usize,
    #[smi] public_exponent: usize,
) -> KeyObjectHandlePair {
    spawn_blocking(move || generate_rsa(modulus_length, public_exponent))
        .await
        .unwrap()
}

fn generate_rsa_pss(
    modulus_length: usize,
    public_exponent: usize,
    hash_algorithm: Option<&str>,
    mf1_hash_algorithm: Option<&str>,
    salt_length: Option<u32>,
) -> Result<KeyObjectHandlePair, AnyError> {
    let key = RsaPrivateKey::new_with_exp(
        &mut thread_rng(),
        modulus_length,
        &rsa::BigUint::from_usize(public_exponent).unwrap(),
    )
    .unwrap();

    let details =
        if hash_algorithm.is_none() && mf1_hash_algorithm.is_none() && salt_length.is_none() {
            None
        } else {
            let hash_algorithm = hash_algorithm.unwrap_or("sha1");
            let mf1_hash_algorithm = mf1_hash_algorithm.unwrap_or(hash_algorithm);
            let hash_algorithm = match_fixed_digest_with_oid!(
              hash_algorithm,
              fn (algorithm: Option<RsaPssHashAlgorithm>) {
                algorithm.ok_or_else(|| type_error("digest not allowed for RSA-PSS keys: {}"))?
              },
              _ => {
                return Err(type_error(format!(
                  "digest not allowed for RSA-PSS keys: {}",
                  hash_algorithm
                )))
              }
            );
            let mf1_hash_algorithm = match_fixed_digest_with_oid!(
              mf1_hash_algorithm,
              fn (algorithm: Option<RsaPssHashAlgorithm>) {
                algorithm.ok_or_else(|| type_error("digest not allowed for RSA-PSS keys: {}"))?
              },
              _ => {
                return Err(type_error(format!(
                  "digest not allowed for RSA-PSS keys: {}",
                  mf1_hash_algorithm
                )))
              }
            );
            let salt_length = salt_length.unwrap_or_else(|| hash_algorithm.salt_length());

            Some(RsaPssDetails {
                hash_algorithm,
                mf1_hash_algorithm,
                salt_length,
            })
        };

    let private_key = AsymmetricPrivateKey::RsaPss(RsaPssPrivateKey { key, details });
    let public_key = private_key.to_public_key();

    Ok(KeyObjectHandlePair::new(private_key, public_key))
}

#[op2]
#[cppgc]
pub fn op_node_generate_rsa_pss_key(
    #[smi] modulus_length: usize,
    #[smi] public_exponent: usize,
    #[string] hash_algorithm: Option<String>, // todo: Option<&str> not supproted in ops yet
    #[string] mf1_hash_algorithm: Option<String>, // todo: Option<&str> not supproted in ops yet
    #[smi] salt_length: Option<u32>,
) -> Result<KeyObjectHandlePair, AnyError> {
    generate_rsa_pss(
        modulus_length,
        public_exponent,
        hash_algorithm.as_deref(),
        mf1_hash_algorithm.as_deref(),
        salt_length,
    )
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_rsa_pss_key_async(
    #[smi] modulus_length: usize,
    #[smi] public_exponent: usize,
    #[string] hash_algorithm: Option<String>, // todo: Option<&str> not supproted in ops yet
    #[string] mf1_hash_algorithm: Option<String>, // todo: Option<&str> not supproted in ops yet
    #[smi] salt_length: Option<u32>,
) -> Result<KeyObjectHandlePair, AnyError> {
    spawn_blocking(move || {
        generate_rsa_pss(
            modulus_length,
            public_exponent,
            hash_algorithm.as_deref(),
            mf1_hash_algorithm.as_deref(),
            salt_length,
        )
    })
    .await
    .unwrap()
}

fn dsa_generate(
    modulus_length: usize,
    divisor_length: usize,
) -> Result<KeyObjectHandlePair, AnyError> {
    let mut rng = rand::thread_rng();
    use dsa::Components;
    use dsa::KeySize;
    use dsa::SigningKey;

    let key_size = match (modulus_length, divisor_length) {
        #[allow(deprecated)]
        (1024, 160) => KeySize::DSA_1024_160,
        (2048, 224) => KeySize::DSA_2048_224,
        (2048, 256) => KeySize::DSA_2048_256,
        (3072, 256) => KeySize::DSA_3072_256,
        _ => {
            return Err(type_error(
                "Invalid modulusLength+divisorLength combination",
            ))
        }
    };
    let components = Components::generate(&mut rng, key_size);
    let signing_key = SigningKey::generate(&mut rng, components);
    let private_key = AsymmetricPrivateKey::Dsa(signing_key);
    let public_key = private_key.to_public_key();

    Ok(KeyObjectHandlePair::new(private_key, public_key))
}

#[op2]
#[cppgc]
pub fn op_node_generate_dsa_key(
    #[smi] modulus_length: usize,
    #[smi] divisor_length: usize,
) -> Result<KeyObjectHandlePair, AnyError> {
    dsa_generate(modulus_length, divisor_length)
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_dsa_key_async(
    #[smi] modulus_length: usize,
    #[smi] divisor_length: usize,
) -> Result<KeyObjectHandlePair, AnyError> {
    spawn_blocking(move || dsa_generate(modulus_length, divisor_length))
        .await
        .unwrap()
}

fn ec_generate(named_curve: &str) -> Result<KeyObjectHandlePair, AnyError> {
    let mut rng = rand::thread_rng();
    // TODO(@littledivy): Support public key point encoding.
    // Default is uncompressed.
    let private_key = match named_curve {
        "P-224" | "prime224v1" | "secp224r1" => {
            let key = p224::SecretKey::random(&mut rng);
            AsymmetricPrivateKey::Ec(EcPrivateKey::P224(key))
        }
        "P-256" | "prime256v1" | "secp256r1" => {
            let key = p256::SecretKey::random(&mut rng);
            AsymmetricPrivateKey::Ec(EcPrivateKey::P256(key))
        }
        "P-384" | "prime384v1" | "secp384r1" => {
            let key = p384::SecretKey::random(&mut rng);
            AsymmetricPrivateKey::Ec(EcPrivateKey::P384(key))
        }
        _ => {
            return Err(type_error(format!(
                "unsupported named curve: {}",
                named_curve
            )))
        }
    };
    let public_key = private_key.to_public_key();
    Ok(KeyObjectHandlePair::new(private_key, public_key))
}

#[op2]
#[cppgc]
pub fn op_node_generate_ec_key(
    #[string] named_curve: &str,
) -> Result<KeyObjectHandlePair, AnyError> {
    ec_generate(named_curve)
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_ec_key_async(
    #[string] named_curve: String,
) -> Result<KeyObjectHandlePair, AnyError> {
    spawn_blocking(move || ec_generate(&named_curve))
        .await
        .unwrap()
}

fn x25519_generate() -> KeyObjectHandlePair {
    let keypair = x25519_dalek::StaticSecret::random_from_rng(thread_rng());
    let private_key = AsymmetricPrivateKey::X25519(keypair);
    let public_key = private_key.to_public_key();
    KeyObjectHandlePair::new(private_key, public_key)
}

#[op2]
#[cppgc]
pub fn op_node_generate_x25519_key() -> KeyObjectHandlePair {
    x25519_generate()
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_x25519_key_async() -> KeyObjectHandlePair {
    spawn_blocking(x25519_generate).await.unwrap()
}

fn ed25519_generate() -> KeyObjectHandlePair {
    let keypair = ed25519_dalek::SigningKey::generate(&mut thread_rng());
    let private_key = AsymmetricPrivateKey::Ed25519(keypair);
    let public_key = private_key.to_public_key();
    KeyObjectHandlePair::new(private_key, public_key)
}

#[op2]
#[cppgc]
pub fn op_node_generate_ed25519_key() -> KeyObjectHandlePair {
    ed25519_generate()
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_ed25519_key_async() -> KeyObjectHandlePair {
    spawn_blocking(ed25519_generate).await.unwrap()
}

fn u32_slice_to_u8_slice(slice: &[u32]) -> &[u8] {
    // SAFETY: just reinterpreting the slice as u8
    unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u8, std::mem::size_of_val(slice)) }
}

fn dh_group_generate(group_name: &str) -> Result<KeyObjectHandlePair, AnyError> {
    let (dh, prime, generator) = match group_name {
        "modp5" => (
            dh::DiffieHellman::group::<dh::Modp1536>(),
            dh::Modp1536::MODULUS,
            dh::Modp1536::GENERATOR,
        ),
        "modp14" => (
            dh::DiffieHellman::group::<dh::Modp2048>(),
            dh::Modp2048::MODULUS,
            dh::Modp2048::GENERATOR,
        ),
        "modp15" => (
            dh::DiffieHellman::group::<dh::Modp3072>(),
            dh::Modp3072::MODULUS,
            dh::Modp3072::GENERATOR,
        ),
        "modp16" => (
            dh::DiffieHellman::group::<dh::Modp4096>(),
            dh::Modp4096::MODULUS,
            dh::Modp4096::GENERATOR,
        ),
        "modp17" => (
            dh::DiffieHellman::group::<dh::Modp6144>(),
            dh::Modp6144::MODULUS,
            dh::Modp6144::GENERATOR,
        ),
        "modp18" => (
            dh::DiffieHellman::group::<dh::Modp8192>(),
            dh::Modp8192::MODULUS,
            dh::Modp8192::GENERATOR,
        ),
        _ => return Err(type_error("Unsupported group name")),
    };
    let params = DhParameter {
        prime: asn1::Int::new(u32_slice_to_u8_slice(prime)).unwrap(),
        base: asn1::Int::new(generator.to_be_bytes().as_slice()).unwrap(),
        private_value_length: None,
    };
    Ok(KeyObjectHandlePair::new(
        AsymmetricPrivateKey::Dh(DhPrivateKey {
            key: dh.private_key,
            params: params.clone(),
        }),
        AsymmetricPublicKey::Dh(DhPublicKey {
            key: dh.public_key,
            params,
        }),
    ))
}

#[op2]
#[cppgc]
pub fn op_node_generate_dh_group_key(
    #[string] group_name: &str,
) -> Result<KeyObjectHandlePair, AnyError> {
    dh_group_generate(group_name)
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_dh_group_key_async(
    #[string] group_name: String,
) -> Result<KeyObjectHandlePair, AnyError> {
    spawn_blocking(move || dh_group_generate(&group_name))
        .await
        .unwrap()
}

fn dh_generate(
    prime: Option<&[u8]>,
    prime_len: usize,
    generator: usize,
) -> Result<KeyObjectHandlePair, AnyError> {
    let prime = prime
        .map(|p| p.into())
        .unwrap_or_else(|| Prime::generate(prime_len));
    let dh = dh::DiffieHellman::new(prime.clone(), generator);
    let params = DhParameter {
        prime: asn1::Int::new(&prime.0.to_bytes_be()).unwrap(),
        base: asn1::Int::new(generator.to_be_bytes().as_slice()).unwrap(),
        private_value_length: None,
    };
    Ok(KeyObjectHandlePair::new(
        AsymmetricPrivateKey::Dh(DhPrivateKey {
            key: dh.private_key,
            params: params.clone(),
        }),
        AsymmetricPublicKey::Dh(DhPublicKey {
            key: dh.public_key,
            params,
        }),
    ))
}

#[op2]
#[cppgc]
pub fn op_node_generate_dh_key(
    #[buffer] prime: Option<&[u8]>,
    #[smi] prime_len: usize,
    #[smi] generator: usize,
) -> Result<KeyObjectHandlePair, AnyError> {
    dh_generate(prime, prime_len, generator)
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_dh_key_async(
    #[buffer(copy)] prime: Option<Box<[u8]>>,
    #[smi] prime_len: usize,
    #[smi] generator: usize,
) -> Result<KeyObjectHandlePair, AnyError> {
    spawn_blocking(move || dh_generate(prime.as_deref(), prime_len, generator))
        .await
        .unwrap()
}

#[op2]
#[serde]
pub fn op_node_dh_keys_generate_and_export(
    #[buffer] prime: Option<&[u8]>,
    #[smi] prime_len: usize,
    #[smi] generator: usize,
) -> Result<(ToJsBuffer, ToJsBuffer), AnyError> {
    let prime = prime
        .map(|p| p.into())
        .unwrap_or_else(|| Prime::generate(prime_len));
    let dh = dh::DiffieHellman::new(prime, generator);
    let private_key = dh.private_key.into_vec().into_boxed_slice();
    let public_key = dh.public_key.into_vec().into_boxed_slice();
    Ok((private_key.into(), public_key.into()))
}

#[op2]
#[buffer]
pub fn op_node_export_secret_key(#[cppgc] handle: &KeyObjectHandle) -> Result<Box<[u8]>, AnyError> {
    let key = handle
        .as_secret_key()
        .ok_or_else(|| type_error("key is not a secret key"))?;
    Ok(key.to_vec().into_boxed_slice())
}

#[op2]
#[string]
pub fn op_node_export_secret_key_b64url(
    #[cppgc] handle: &KeyObjectHandle,
) -> Result<String, AnyError> {
    let key = handle
        .as_secret_key()
        .ok_or_else(|| type_error("key is not a secret key"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key))
}

#[op2]
#[serde]
pub fn op_node_export_public_key_jwk(
    #[cppgc] handle: &KeyObjectHandle,
) -> Result<deno_core::serde_json::Value, AnyError> {
    let public_key = handle
        .as_public_key()
        .ok_or_else(|| type_error("key is not an asymmetric public key"))?;

    public_key.export_jwk()
}

#[op2]
#[string]
pub fn op_node_export_public_key_pem(
    #[cppgc] handle: &KeyObjectHandle,
    #[string] typ: &str,
) -> Result<String, AnyError> {
    let public_key = handle
        .as_public_key()
        .ok_or_else(|| type_error("key is not an asymmetric public key"))?;
    let data = public_key.export_der(typ)?;

    let label = match typ {
        "pkcs1" => "RSA PUBLIC KEY",
        "spki" => "PUBLIC KEY",
        _ => unreachable!("export_der would have errored"),
    };

    let mut out = vec![0; 2048];
    let mut writer = PemWriter::new(label, LineEnding::LF, &mut out)?;
    writer.write(&data)?;
    let len = writer.finish()?;
    out.truncate(len);

    Ok(String::from_utf8(out).expect("invalid pem is not possible"))
}

#[op2]
#[buffer]
pub fn op_node_export_public_key_der(
    #[cppgc] handle: &KeyObjectHandle,
    #[string] typ: &str,
) -> Result<Box<[u8]>, AnyError> {
    let public_key = handle
        .as_public_key()
        .ok_or_else(|| type_error("key is not an asymmetric public key"))?;
    public_key.export_der(typ)
}

#[op2]
#[string]
pub fn op_node_export_private_key_pem(
    #[cppgc] handle: &KeyObjectHandle,
    #[string] typ: &str,
) -> Result<String, AnyError> {
    let private_key = handle
        .as_private_key()
        .ok_or_else(|| type_error("key is not an asymmetric private key"))?;
    let data = private_key.export_der(typ)?;

    let label = match typ {
        "pkcs1" => "RSA PRIVATE KEY",
        "pkcs8" => "PRIVATE KEY",
        "sec1" => "EC PRIVATE KEY",
        _ => unreachable!("export_der would have errored"),
    };

    let mut out = vec![0; 2048];
    let mut writer = PemWriter::new(label, LineEnding::LF, &mut out)?;
    writer.write(&data)?;
    let len = writer.finish()?;
    out.truncate(len);

    Ok(String::from_utf8(out).expect("invalid pem is not possible"))
}

#[op2]
#[buffer]
pub fn op_node_export_private_key_der(
    #[cppgc] handle: &KeyObjectHandle,
    #[string] typ: &str,
) -> Result<Box<[u8]>, AnyError> {
    let private_key = handle
        .as_private_key()
        .ok_or_else(|| type_error("key is not an asymmetric private key"))?;
    private_key.export_der(typ)
}

#[op2]
#[string]
pub fn op_node_key_type(#[cppgc] handle: &KeyObjectHandle) -> &'static str {
    match handle {
        KeyObjectHandle::AsymmetricPrivate(_) => "private",
        KeyObjectHandle::AsymmetricPublic(_) => "public",
        KeyObjectHandle::Secret(_) => "secret",
    }
}

#[op2]
#[cppgc]
pub fn op_node_derive_public_key_from_private_key(
    #[cppgc] handle: &KeyObjectHandle,
) -> Result<KeyObjectHandle, AnyError> {
    let Some(private_key) = handle.as_private_key() else {
        return Err(type_error("expected private key"));
    };

    Ok(KeyObjectHandle::AsymmetricPublic(
        private_key.to_public_key(),
    ))
}
