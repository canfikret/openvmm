// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::ak_cert::TpmAkCertType;
use crate::Tpm;
use crate::TpmError;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use thiserror::Error;
use tpm_resources::TpmDeviceHandle;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;

pub struct TpmDeviceResolver;

declare_static_async_resolver! {
    TpmDeviceResolver,
    (ChipsetDeviceHandleKind, TpmDeviceHandle),
}

#[derive(Debug, Error)]
pub enum ResolveTpmError {
    #[error("error resolving ppi store")]
    ResolvePpiStore(#[source] ResolveError),
    #[error("error resolving nvram store")]
    ResolveNvramStore(#[source] ResolveError),
    #[error("error resolving get attestation report")]
    ResolveGetAttestationReport(#[source] ResolveError),
    #[error("error resolving request ak cert")]
    ResolveRequestAkCert(#[source] ResolveError),
    #[error("error creating tpm")]
    Tpm(#[source] TpmError),
    #[error(
        "invalid AK cert type: `get_attestation_report` is `Some`, `request_ak_cert` is `None`"
    )]
    InvalidAkCertType,
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, TpmDeviceHandle> for TpmDeviceResolver {
    type Error = ResolveTpmError;
    type Output = ResolvedChipsetDevice;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: TpmDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let ppi_store = resolver
            .resolve(resource.ppi_store, ())
            .await
            .map_err(ResolveTpmError::ResolvePpiStore)?;

        let nvram_store = resolver
            .resolve(resource.nvram_store, ())
            .await
            .map_err(ResolveTpmError::ResolveNvramStore)?;

        let get_attestation_report =
            if let Some(get_attestation_report) = resource.get_attestation_report {
                Some(
                    resolver
                        .resolve(get_attestation_report, &())
                        .await
                        .map_err(ResolveTpmError::ResolveGetAttestationReport)?
                        .0,
                )
            } else {
                None
            };

        let request_ak_cert = if let Some(request_ak_cert) = resource.request_ak_cert {
            Some(
                resolver
                    .resolve(request_ak_cert, &())
                    .await
                    .map_err(ResolveTpmError::ResolveRequestAkCert)?
                    .0,
            )
        } else {
            None
        };

        let ak_cert_type = match (get_attestation_report, request_ak_cert) {
            (Some(get_attestation_report), Some(request_ak_cert)) => {
                TpmAkCertType::HwAttested(get_attestation_report, request_ak_cert)
            }
            (None, Some(request_ak_cert)) => TpmAkCertType::Trusted(request_ak_cert),
            (Some(_), None) => Err(ResolveTpmError::InvalidAkCertType)?,
            (None, None) => TpmAkCertType::None,
        };

        // The TPM device doesn't need access to the entire API of `vmtime`, so
        // to make it easier to unit test / fuzz, only pass the TPM a small
        // callback it can use to obtain a monotonically increasing timestamp.
        let monotonic_timer = Box::new({
            let vmtime = input.vmtime.access("tpm");
            move || {
                const NUM_100NS_IN_SEC: u64 = 10 * 1000 * 1000;
                let n = vmtime.now().as_100ns();
                std::time::Duration::new(n / NUM_100NS_IN_SEC, (n % NUM_100NS_IN_SEC) as u32 * 100)
            }
        });

        let tpm = Tpm::new(
            resource.register_layout,
            input.encrypted_guest_memory.clone(),
            ppi_store.0,
            nvram_store.0,
            monotonic_timer,
            resource.refresh_tpm_seeds,
            input.is_restoring,
            ak_cert_type,
            resource.guest_secret_key,
        )
        .await
        .map_err(ResolveTpmError::Tpm)?;

        Ok(tpm.into())
    }
}
