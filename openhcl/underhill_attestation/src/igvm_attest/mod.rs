// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module helps preparing requests and parsing responses that are
//! sent to and received from the IGVm agent runs on the host via GET
//! `IGVM_ATTEST` host request.

use crate::protocol;
use crate::protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use crate::protocol::igvm_attest::get::IgvmAttestHashType;
use crate::protocol::igvm_attest::get::IgvmAttestReportType;
use crate::protocol::igvm_attest::get::IgvmAttestRequestType;
use base64_serde::base64_serde_type;
use tee_call::TeeType;
use thiserror::Error;
use zerocopy::AsBytes;
use zerocopy::FromZeroes;

pub mod ak_cert;
pub mod key_release;
pub mod wrapped_key;

base64_serde_type!(Base64Url, base64::engine::general_purpose::URL_SAFE_NO_PAD);

#[allow(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum Error {
    #[error("the type of the attestation report is invalid")]
    InvalidAttestationReportType,
    #[error(
        "the size of the attestation report {report_size} is invalid, expected {expected_size}"
    )]
    InvalidAttestationReportSize {
        report_size: usize,
        expected_size: usize,
    },
    #[error("AK cert response size is too small to parse")]
    AkCertResponseSizeTooSmall,
    #[error(
        "AK cert response size {specified_size} specified in the header is larger then the actual size {size}"
    )]
    AkCertResponseSizeMismatch { size: usize, specified_size: usize },
    #[error(
        "AK cert response header version {version} does match the expected version {expected_version}"
    )]
    AkCertResponseHeaderVersionMismatch { version: u32, expected_version: u32 },
}

/// Helper struct to create `IgvmAttestRequest` in raw bytes.
pub struct IgvmAttestRequestHelper {
    /// The request type.
    pub request_type: IgvmAttestRequestType,
    /// The report type.
    pub report_type: IgvmAttestReportType,
    /// Raw bytes of `RuntimeClaims`.
    pub runtime_claims: Vec<u8>,
    /// The hash of the `runtime_claims` to be included in the
    /// `report_data` field of the attestation report.
    pub runtime_claims_hash: [u8; tee_call::REPORT_DATA_SIZE],
    /// THe hash type of the `runtime_claims_hash`.
    pub hash_type: IgvmAttestHashType,
}

impl IgvmAttestRequestHelper {
    /// Prepare the data necessary for creating the `KEY_RELEASE` request.
    pub fn prepare_key_release_request(
        tee_type: TeeType,
        rsa_exponent: &[u8],
        rsa_modulus: &[u8],
        host_time: i64,
        attestation_vm_config: &AttestationVmConfig,
    ) -> Self {
        let report_type = match tee_type {
            TeeType::Snp => IgvmAttestReportType::SNP_VM_REPORT,
            TeeType::Tdx => IgvmAttestReportType::TDX_VM_REPORT,
        };

        let attestation_vm_config =
            attestation_vm_config_with_time(attestation_vm_config, host_time);
        let runtime_claims =
            protocol::igvm_attest::get::runtime_claims::RuntimeClaims::key_release_request_runtime_claims(rsa_exponent, rsa_modulus, &attestation_vm_config);
        let runtime_claims = runtime_claims_to_bytes(&runtime_claims);

        let hash_type = IgvmAttestHashType::SHA_256;
        let hash = crate::crypto::sha_256(runtime_claims.as_bytes());
        let mut runtime_claims_hash = [0u8; tee_call::REPORT_DATA_SIZE];
        runtime_claims_hash[0..hash.len()].copy_from_slice(&hash);

        Self {
            request_type: IgvmAttestRequestType::KEY_RELEASE_REQUEST,
            report_type,
            runtime_claims,
            runtime_claims_hash,
            hash_type,
        }
    }

    /// Prepare the data necessary for creating the `AK_CERT` request.
    pub fn prepare_ak_cert_request(
        tee_type: TeeType,
        ak_pub_exponent: &[u8],
        ak_pub_modulus: &[u8],
        ek_pub_exponent: &[u8],
        ek_pub_modulus: &[u8],
        attestation_vm_config: &AttestationVmConfig,
        guest_input: &[u8],
    ) -> Self {
        let report_type = match tee_type {
            TeeType::Snp => IgvmAttestReportType::SNP_VM_REPORT,
            TeeType::Tdx => IgvmAttestReportType::TDX_VM_REPORT,
        };

        let runtime_claims =
            protocol::igvm_attest::get::runtime_claims::RuntimeClaims::ak_cert_runtime_claims(
                ak_pub_exponent,
                ak_pub_modulus,
                ek_pub_exponent,
                ek_pub_modulus,
                attestation_vm_config,
                guest_input,
            );

        let runtime_claims = runtime_claims_to_bytes(&runtime_claims);

        let hash_type = IgvmAttestHashType::SHA_256;
        let hash = crate::crypto::sha_256(runtime_claims.as_bytes());
        let mut runtime_claims_hash = [0u8; tee_call::REPORT_DATA_SIZE];
        runtime_claims_hash[0..hash.len()].copy_from_slice(&hash);

        Self {
            request_type: IgvmAttestRequestType::AK_CERT_REQUEST,
            report_type,
            runtime_claims,
            runtime_claims_hash,
            hash_type,
        }
    }

    /// Create the request in raw bytes.
    pub fn create_request(&self, attestation_report: &[u8]) -> Result<Vec<u8>, Error> {
        create_request(
            self.request_type,
            &self.runtime_claims,
            attestation_report,
            self.report_type,
            self.hash_type,
        )
    }
}

/// Create a request in raw bytes.
/// A request looks like:
///     `IgvmAttestRequest` in raw bytes | `runtime_claims` (raw bytes)
fn create_request(
    request_type: IgvmAttestRequestType,
    runtime_claims: &[u8],
    attestation_report: &[u8],
    report_type: IgvmAttestReportType,
    hash_type: IgvmAttestHashType,
) -> Result<Vec<u8>, Error> {
    use protocol::igvm_attest::get::IgvmAttestRequest;
    use protocol::igvm_attest::get::IgvmAttestRequestData;
    use protocol::igvm_attest::get::IgvmAttestRequestHeader;

    let expected_report_size = get_report_size(report_type)?;
    if attestation_report.len() != expected_report_size {
        Err(Error::InvalidAttestationReportSize {
            report_size: attestation_report.len(),
            expected_size: expected_report_size,
        })?
    }

    let report_size = size_of::<IgvmAttestRequest>() + runtime_claims.len();
    let user_data_size = size_of::<IgvmAttestRequestData>() + runtime_claims.len();
    let mut request = IgvmAttestRequest::new_zeroed();

    request.header = IgvmAttestRequestHeader::new(report_size as u32, request_type, 0);

    request.attestation_report[..attestation_report.len()].copy_from_slice(attestation_report);

    request.request_data = IgvmAttestRequestData::new(
        user_data_size as u32,
        report_type,
        hash_type,
        runtime_claims.len() as u32,
    );

    Ok([request.as_bytes(), runtime_claims].concat())
}

/// Get the expected size of the given report type.
fn get_report_size(report_type: IgvmAttestReportType) -> Result<usize, Error> {
    let size = match report_type {
        IgvmAttestReportType::VBS_VM_REPORT => protocol::igvm_attest::get::VBS_VM_REPORT_SIZE,
        IgvmAttestReportType::SNP_VM_REPORT => protocol::igvm_attest::get::SNP_VM_REPORT_SIZE,
        IgvmAttestReportType::TDX_VM_REPORT => protocol::igvm_attest::get::TDX_VM_REPORT_SIZE,
        _ => Err(Error::InvalidAttestationReportType)?,
    };

    Ok(size)
}

/// Helper function that returns the given config with the `current_time` set.
fn attestation_vm_config_with_time(
    vm_config: &AttestationVmConfig,
    host_epoch: i64,
) -> AttestationVmConfig {
    let mut vm_config = vm_config.clone();
    vm_config.current_time = Some(host_epoch);
    vm_config
}

/// Helper function that converts the `RuntimeClaims` to raw bytes.
fn runtime_claims_to_bytes(
    runtime_claims: &protocol::igvm_attest::get::runtime_claims::RuntimeClaims,
) -> Vec<u8> {
    let runtime_claims = serde_json::to_string(runtime_claims).expect("JSON serialization failed");
    runtime_claims.as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_request() {
        let result = create_request(
            IgvmAttestRequestType::AK_CERT_REQUEST,
            &[],
            &[0u8; protocol::igvm_attest::get::SNP_VM_REPORT_SIZE],
            IgvmAttestReportType::SNP_VM_REPORT,
            IgvmAttestHashType::SHA_256,
        );
        assert!(result.is_ok());

        let result = create_request(
            IgvmAttestRequestType::AK_CERT_REQUEST,
            &[],
            &[0u8; protocol::igvm_attest::get::SNP_VM_REPORT_SIZE + 1],
            IgvmAttestReportType::SNP_VM_REPORT,
            IgvmAttestHashType::SHA_256,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_transfer_key_jwk() {
        const EXPECTED_JWK: &str = r#"[{"kid":"HCLTransferKey","key_ops":["encrypt"],"kty":"RSA","e":"RVhQT05FTlQ","n":"TU9EVUxVUw"}]"#;

        let rsa_jwk = protocol::igvm_attest::get::runtime_claims::RsaJwk::get_transfer_key_jwks(
            b"EXPONENT",
            b"MODULUS",
        );

        let result = serde_json::to_string(&rsa_jwk);
        assert!(result.is_ok());

        let transfer_key_jwk = result.unwrap();
        assert_eq!(transfer_key_jwk, EXPECTED_JWK);
    }

    #[test]
    fn test_vm_configuration_no_time() {
        const EXPECTED_JWK: &str = r#"{"root-cert-thumbprint":"","console-enabled":false,"secure-boot":false,"tpm-enabled":false,"tpm-persisted":false,"vmUniqueId":""}"#;

        let attestation_vm_config = AttestationVmConfig {
            current_time: None,
            root_cert_thumbprint: String::new(),
            console_enabled: false,
            secure_boot: false,
            tpm_enabled: false,
            tpm_persisted: false,
            vm_unique_id: String::new(),
        };
        let result = serde_json::to_string(&attestation_vm_config);
        assert!(result.is_ok());

        let vm_config = result.unwrap();
        assert_eq!(vm_config, EXPECTED_JWK);
    }

    #[test]
    fn test_vm_configuration_with_time() {
        const EXPECTED_JWK: &str = r#"{"current-time":1691103220,"root-cert-thumbprint":"","console-enabled":false,"secure-boot":false,"tpm-enabled":false,"tpm-persisted":false,"vmUniqueId":""}"#;

        let attestation_vm_config = AttestationVmConfig {
            current_time: None,
            root_cert_thumbprint: String::new(),
            console_enabled: false,
            secure_boot: false,
            tpm_enabled: false,
            tpm_persisted: false,
            vm_unique_id: String::new(),
        };
        let attestation_vm_config =
            attestation_vm_config_with_time(&attestation_vm_config, 1691103220);
        let result = serde_json::to_string(&attestation_vm_config);
        assert!(result.is_ok());

        let vm_config = result.unwrap();
        assert_eq!(vm_config, EXPECTED_JWK);
    }
}
