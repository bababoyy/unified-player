use std::{
    fmt,
    fs::File,
    io::{BufReader, Read, Write},
    iter,
    time::Duration,
};

use age::{scrypt, stream::StreamWriter, Decryptor, Encryptor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::{
    comparison_store::validate_comparison_result_payload,
    model::{
        CaptureByteBucket, CaptureCompleteness, CaptureLimits, CapturePurpose, CaptureRecordKind,
        CaptureRecordV1, CaptureRef, EndpointRole, ExchangeRef, IncompleteReason, PrivateCaptureV1,
        ProviderClientKind, SafeArtifactReview, SafeOperationRef, SafeTerminalCategory,
        SensitiveBytes, TransportKind, CAPTURE_CONSENT_VERSION, CAPTURE_SCHEMA_VERSION,
        TERMINAL_RECORD_RESERVE_BYTES,
    },
    payload::{decode_fields, field, PrivatePayloadKind},
    security::CapturePassphrase,
};

const CONTAINER_MAGIC: &[u8; 16] = b"SPCAPTURE-V1\0\0\0\0";
const RECORD_FRAME: u8 = 1;
const MANIFEST_FRAME: u8 = 0xff;
const RECORD_FRAME_HEADER_BYTES: u64 = 1 + 2 + 8 + 1 + 8 + 1 + 1 + 1 + 1 + 1 + 4;
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const CHECKSUM_DOMAIN: &[u8] = b"unified-player-private-capture-record-stream-v1\0";

/// A capture writer whose expensive passphrase setup and record encryption happen while the
/// operation is active. Finalization only appends the authenticated manifest and commits the
/// already-encrypted partial artifact.
pub(super) struct EncryptedCaptureStream {
    writer: Option<StreamWriter<File>>,
    capture_ref: CaptureRef,
    limits: CaptureLimits,
    checksum: Sha256,
    record_count: u16,
    plaintext_bytes: u64,
    exchange_count: u16,
    previous_offset_ms: Option<u64>,
    encoded_bytes: u64,
    terminal: Option<SafeTerminalCategory>,
}

impl EncryptedCaptureStream {
    pub(super) fn begin(
        file: File,
        capture_ref: CaptureRef,
        passphrase: &CapturePassphrase,
        limits: CaptureLimits,
    ) -> Result<Self, VaultFormatError> {
        let limits = limits
            .validate()
            .map_err(|_| VaultFormatError::InvalidLimits)?;
        let encryptor = Encryptor::with_user_passphrase(passphrase.age_secret());
        let mut writer = encryptor
            .wrap_output(file)
            .map_err(|_| VaultFormatError::EncryptionFailed)?;
        writer
            .write_all(CONTAINER_MAGIC)
            .map_err(|_| VaultFormatError::EncryptionFailed)?;
        let mut checksum = Sha256::new();
        checksum.update(CHECKSUM_DOMAIN);
        Ok(Self {
            writer: Some(writer),
            capture_ref,
            limits,
            checksum,
            record_count: 0,
            plaintext_bytes: 0,
            exchange_count: 0,
            previous_offset_ms: None,
            encoded_bytes: u64::try_from(CONTAINER_MAGIC.len()).unwrap_or(u64::MAX),
            terminal: None,
        })
    }

    pub(super) fn write_record(
        &mut self,
        record: &CaptureRecordV1,
    ) -> Result<(), VaultFormatError> {
        let terminal_record = record.kind == CaptureRecordKind::TerminalOutcome;
        if self.terminal.is_some() {
            return Err(VaultFormatError::InvalidRecordStream);
        }
        let record_limit = if terminal_record {
            self.limits.record_capacity.saturating_add(1)
        } else {
            self.limits.record_capacity
        };
        if self.record_count >= record_limit
            || record.sequence != self.record_count
            || self
                .previous_offset_ms
                .is_some_and(|offset| record.monotonic_offset_ms < offset)
        {
            return Err(VaultFormatError::InvalidRecordStream);
        }
        let payload_bytes =
            u64::try_from(record.payload.len()).map_err(|_| VaultFormatError::RecordTooLarge)?;
        if payload_bytes > self.limits.record_bytes(record.kind) {
            return Err(VaultFormatError::RecordTooLarge);
        }
        if terminal_record && payload_bytes > TERMINAL_RECORD_RESERVE_BYTES {
            return Err(VaultFormatError::RecordTooLarge);
        }
        let terminal = if terminal_record {
            Some(terminal_from_record(record)?)
        } else {
            None
        };
        let next_plaintext = self
            .plaintext_bytes
            .checked_add(payload_bytes)
            .ok_or(VaultFormatError::PlaintextTooLarge)?;
        let plaintext_limit = if terminal_record {
            self.limits
                .plaintext_bytes
                .saturating_add(TERMINAL_RECORD_RESERVE_BYTES)
        } else {
            self.limits.plaintext_bytes
        };
        if next_plaintext > plaintext_limit {
            return Err(VaultFormatError::PlaintextTooLarge);
        }
        let next_exchange_count = if record.kind.starts_provider_exchange() {
            self.exchange_count
                .checked_add(1)
                .ok_or(VaultFormatError::InvalidRecordStream)?
        } else {
            self.exchange_count
        };
        if next_exchange_count > self.limits.exchange_capacity {
            return Err(VaultFormatError::InvalidRecordStream);
        }
        let next_encoded = self
            .encoded_bytes
            .checked_add(RECORD_FRAME_HEADER_BYTES)
            .and_then(|bytes| bytes.checked_add(payload_bytes))
            .ok_or(VaultFormatError::PlaintextTooLarge)?;
        if next_encoded > self.limits.encrypted_artifact_bytes {
            return Err(VaultFormatError::ArtifactTooLarge);
        }

        let writer = self
            .writer
            .as_mut()
            .ok_or(VaultFormatError::EncryptionFailed)?;
        write_record_frame(writer, record)?;
        update_record_checksum(&mut self.checksum, record);
        self.record_count = self.record_count.saturating_add(1);
        self.plaintext_bytes = next_plaintext;
        self.exchange_count = next_exchange_count;
        self.previous_offset_ms = Some(record.monotonic_offset_ms);
        self.encoded_bytes = next_encoded;
        self.terminal = terminal;
        Ok(())
    }

    pub(super) fn finish(
        mut self,
        capture: &PrivateCaptureV1,
    ) -> Result<WrittenArtifact, VaultFormatError> {
        if capture.capture_ref != self.capture_ref {
            return Err(VaultFormatError::ReferenceMismatch);
        }
        if self
            .terminal
            .is_some_and(|terminal| terminal != capture.terminal_category)
        {
            return Err(VaultFormatError::InvalidRecordStream);
        }
        validate_capture_summary(capture, self.record_count, self.exchange_count)?;
        let checksum: [u8; 32] = self.checksum.clone().finalize().into();
        let manifest = ManifestDto::from_private(
            capture,
            self.limits,
            checksum,
            self.record_count,
            self.plaintext_bytes,
        )?;
        let manifest = Zeroizing::new(
            serde_json::to_vec(&manifest).map_err(|_| VaultFormatError::EncodingFailed)?,
        );
        if manifest.is_empty() || manifest.len() > MAX_MANIFEST_BYTES {
            return Err(VaultFormatError::ManifestTooLarge);
        }
        let final_plaintext_bytes = self
            .encoded_bytes
            .checked_add(5)
            .and_then(|bytes| bytes.checked_add(u64::try_from(manifest.len()).ok()?))
            .ok_or(VaultFormatError::PlaintextTooLarge)?;
        if final_plaintext_bytes > self.limits.encrypted_artifact_bytes {
            return Err(VaultFormatError::ArtifactTooLarge);
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or(VaultFormatError::EncryptionFailed)?;
        write_manifest_frame(writer, &manifest)?;
        let writer = self
            .writer
            .take()
            .ok_or(VaultFormatError::EncryptionFailed)?;
        let mut file = writer
            .finish()
            .map_err(|_| VaultFormatError::EncryptionFailed)?;
        file.flush()
            .map_err(|_| VaultFormatError::PersistenceFailed)?;
        file.sync_all()
            .map_err(|_| VaultFormatError::PersistenceFailed)?;
        let encrypted_bytes = file
            .metadata()
            .map_err(|_| VaultFormatError::PersistenceFailed)?
            .len();
        if encrypted_bytes > self.limits.encrypted_artifact_bytes {
            return Err(VaultFormatError::ArtifactTooLarge);
        }
        Ok(WrittenArtifact {
            capture_ref: self.capture_ref,
            encrypted_bytes,
        })
    }
}

pub(super) fn write_encrypted(
    file: File,
    capture: &PrivateCaptureV1,
    passphrase: &CapturePassphrase,
    limits: CaptureLimits,
) -> Result<WrittenArtifact, VaultFormatError> {
    let limits = limits
        .validate()
        .map_err(|_| VaultFormatError::InvalidLimits)?;
    validate_private_capture(capture, limits)?;
    let encryptor = Encryptor::with_user_passphrase(passphrase.age_secret());
    let mut writer = encryptor
        .wrap_output(file)
        .map_err(|_| VaultFormatError::EncryptionFailed)?;
    write_plaintext_container(&mut writer, capture, limits, None, None)?;
    let mut file = writer
        .finish()
        .map_err(|_| VaultFormatError::EncryptionFailed)?;
    file.flush()
        .map_err(|_| VaultFormatError::PersistenceFailed)?;
    file.sync_all()
        .map_err(|_| VaultFormatError::PersistenceFailed)?;
    let encrypted_bytes = file
        .metadata()
        .map_err(|_| VaultFormatError::PersistenceFailed)?
        .len();
    if encrypted_bytes > limits.encrypted_artifact_bytes {
        return Err(VaultFormatError::ArtifactTooLarge);
    }
    Ok(WrittenArtifact {
        capture_ref: capture.capture_ref,
        encrypted_bytes,
    })
}

pub(super) fn read_encrypted(
    file: File,
    expected_ref: CaptureRef,
    passphrase: &CapturePassphrase,
    limits: CaptureLimits,
) -> Result<PrivateCaptureV1, VaultFormatError> {
    read_encrypted_artifact(file, expected_ref, passphrase, limits)
        .map(DecryptedCaptureArtifact::into_capture)
}

pub(super) fn read_encrypted_artifact(
    file: File,
    expected_ref: CaptureRef,
    passphrase: &CapturePassphrase,
    limits: CaptureLimits,
) -> Result<DecryptedCaptureArtifact, VaultFormatError> {
    let limits = limits
        .validate()
        .map_err(|_| VaultFormatError::InvalidLimits)?;
    let encrypted_bytes = file
        .metadata()
        .map_err(|_| VaultFormatError::PersistenceFailed)?
        .len();
    if encrypted_bytes == 0 || encrypted_bytes > limits.encrypted_artifact_bytes {
        return Err(VaultFormatError::ArtifactTooLarge);
    }
    let decryptor = Decryptor::new_buffered(BufReader::new(file))
        .map_err(|_| VaultFormatError::DecryptionFailed)?;
    if !decryptor.is_scrypt() {
        return Err(VaultFormatError::UnsupportedRecipient);
    }
    let identity = scrypt::Identity::new(passphrase.age_secret());
    let mut reader = decryptor
        .decrypt(iter::once(&identity as _))
        .map_err(|_| VaultFormatError::DecryptionFailed)?;
    read_plaintext_container(&mut reader, expected_ref, limits)
}

fn write_plaintext_container(
    writer: &mut impl Write,
    capture: &PrivateCaptureV1,
    limits: CaptureLimits,
    checksum_override: Option<[u8; 32]>,
    schema_override: Option<u16>,
) -> Result<(), VaultFormatError> {
    writer
        .write_all(CONTAINER_MAGIC)
        .map_err(|_| VaultFormatError::EncryptionFailed)?;
    for record in &capture.records {
        write_record_frame(writer, record)?;
    }

    let mut manifest = ManifestDto::from_private(
        capture,
        limits,
        checksum_override.unwrap_or_else(|| record_stream_checksum(&capture.records)),
        u16::try_from(capture.records.len()).map_err(|_| VaultFormatError::InvalidRecordStream)?,
        capture.records.iter().try_fold(0_u64, |total, record| {
            total
                .checked_add(
                    u64::try_from(record.payload.len())
                        .map_err(|_| VaultFormatError::PlaintextTooLarge)?,
                )
                .ok_or(VaultFormatError::PlaintextTooLarge)
        })?,
    )?;
    if let Some(schema_version) = schema_override {
        manifest.schema_version = schema_version;
    }
    let manifest = Zeroizing::new(
        serde_json::to_vec(&manifest).map_err(|_| VaultFormatError::EncodingFailed)?,
    );
    if manifest.len() > MAX_MANIFEST_BYTES {
        return Err(VaultFormatError::ManifestTooLarge);
    }
    write_manifest_frame(writer, &manifest)?;

    let encoded_bytes = u64::try_from(CONTAINER_MAGIC.len())
        .ok()
        .and_then(|bytes| {
            u64::try_from(capture.records.len())
                .ok()?
                .checked_mul(RECORD_FRAME_HEADER_BYTES)?
                .checked_add(bytes)
        })
        .and_then(|bytes| {
            capture.records.iter().try_fold(bytes, |total, record| {
                total.checked_add(u64::try_from(record.payload.len()).ok()?)
            })
        })
        .and_then(|bytes| bytes.checked_add(5))
        .and_then(|bytes| bytes.checked_add(u64::try_from(manifest.len()).ok()?))
        .ok_or(VaultFormatError::PlaintextTooLarge)?;
    if encoded_bytes
        > limits
            .plaintext_bytes
            .checked_add(super::model::MAX_CONTAINER_OVERHEAD_BYTES)
            .ok_or(VaultFormatError::PlaintextTooLarge)?
    {
        return Err(VaultFormatError::PlaintextTooLarge);
    }
    Ok(())
}

fn write_record_frame(
    writer: &mut impl Write,
    record: &CaptureRecordV1,
) -> Result<(), VaultFormatError> {
    let payload_len =
        u32::try_from(record.payload.len()).map_err(|_| VaultFormatError::RecordTooLarge)?;
    writer
        .write_all(&[RECORD_FRAME])
        .and_then(|()| writer.write_all(&record.sequence.to_be_bytes()))
        .and_then(|()| writer.write_all(&record.monotonic_offset_ms.to_be_bytes()))
        .and_then(|()| writer.write_all(&[u8::from(record.exchange_ref.is_some())]))
        .and_then(|()| {
            writer.write_all(
                &record
                    .exchange_ref
                    .map_or([0; 8], super::model::ExchangeRef::bytes),
            )
        })
        .and_then(|()| writer.write_all(&[endpoint_role_tag(record.endpoint_role)]))
        .and_then(|()| writer.write_all(&[provider_client_tag(record.client_kind)]))
        .and_then(|()| writer.write_all(&[transport_kind_tag(record.transport_kind)]))
        .and_then(|()| writer.write_all(&[record.attempt]))
        .and_then(|()| writer.write_all(&[record_kind_tag(record.kind)]))
        .and_then(|()| writer.write_all(&payload_len.to_be_bytes()))
        .and_then(|()| writer.write_all(record.payload.expose()))
        .map_err(|_| VaultFormatError::EncryptionFailed)
}

fn write_manifest_frame(writer: &mut impl Write, manifest: &[u8]) -> Result<(), VaultFormatError> {
    let manifest_len =
        u32::try_from(manifest.len()).map_err(|_| VaultFormatError::ManifestTooLarge)?;
    writer
        .write_all(&[MANIFEST_FRAME])
        .and_then(|()| writer.write_all(&manifest_len.to_be_bytes()))
        .and_then(|()| writer.write_all(manifest))
        .map_err(|_| VaultFormatError::EncryptionFailed)
}

fn read_plaintext_container(
    reader: &mut impl Read,
    expected_ref: CaptureRef,
    limits: CaptureLimits,
) -> Result<DecryptedCaptureArtifact, VaultFormatError> {
    let mut magic = [0_u8; CONTAINER_MAGIC.len()];
    read_exact_private(reader, &mut magic)?;
    if &magic != CONTAINER_MAGIC {
        return Err(VaultFormatError::InvalidContainer);
    }

    let mut records = Vec::new();
    let mut payload_bytes = 0_u64;
    loop {
        let frame = read_u8(reader)?;
        match frame {
            RECORD_FRAME => {
                let sequence = read_u16(reader)?;
                let monotonic_offset_ms = read_u64(reader)?;
                let exchange_present = read_u8(reader)?;
                if exchange_present > 1 {
                    return Err(VaultFormatError::InvalidRecordStream);
                }
                let mut exchange_bytes = [0_u8; 8];
                read_exact_private(reader, &mut exchange_bytes)?;
                let exchange_ref =
                    (exchange_present == 1).then(|| ExchangeRef::from_bytes(exchange_bytes));
                if exchange_ref.is_none() && exchange_bytes != [0; 8] {
                    return Err(VaultFormatError::InvalidRecordStream);
                }
                let endpoint_role = endpoint_role_from_tag(read_u8(reader)?)?;
                let client_kind = provider_client_from_tag(read_u8(reader)?)?;
                let transport_kind = transport_kind_from_tag(read_u8(reader)?)?;
                let attempt = read_u8(reader)?;
                let kind = record_kind_from_tag(read_u8(reader)?)?;
                let evidence_capacity = usize::from(limits.record_capacity);
                if records.len() > evidence_capacity
                    || (records.len() == evidence_capacity
                        && kind != CaptureRecordKind::TerminalOutcome)
                {
                    return Err(VaultFormatError::InvalidRecordStream);
                }
                let payload_len = u64::from(read_u32(reader)?);
                if payload_len > limits.record_bytes(kind)
                    || (kind == CaptureRecordKind::TerminalOutcome
                        && payload_len > TERMINAL_RECORD_RESERVE_BYTES)
                {
                    return Err(VaultFormatError::RecordTooLarge);
                }
                payload_bytes = payload_bytes
                    .checked_add(payload_len)
                    .ok_or(VaultFormatError::PlaintextTooLarge)?;
                let plaintext_limit = if kind == CaptureRecordKind::TerminalOutcome {
                    limits
                        .plaintext_bytes
                        .saturating_add(TERMINAL_RECORD_RESERVE_BYTES)
                } else {
                    limits.plaintext_bytes
                };
                if payload_bytes > plaintext_limit {
                    return Err(VaultFormatError::PlaintextTooLarge);
                }
                let payload_len =
                    usize::try_from(payload_len).map_err(|_| VaultFormatError::RecordTooLarge)?;
                let mut payload = SensitiveBytes::with_capacity(payload_len);
                payload.expose_mut().resize(payload_len, 0);
                read_exact_private(reader, payload.expose_mut())?;
                records.push(CaptureRecordV1 {
                    sequence,
                    monotonic_offset_ms,
                    exchange_ref,
                    endpoint_role,
                    client_kind,
                    transport_kind,
                    attempt,
                    kind,
                    payload,
                });
            }
            MANIFEST_FRAME => {
                let manifest_len = usize::try_from(read_u32(reader)?)
                    .map_err(|_| VaultFormatError::ManifestTooLarge)?;
                if manifest_len == 0 || manifest_len > MAX_MANIFEST_BYTES {
                    return Err(VaultFormatError::ManifestTooLarge);
                }
                let mut bytes = Zeroizing::new(vec![0_u8; manifest_len]);
                read_exact_private(reader, &mut bytes)?;
                let manifest: ManifestDto = serde_json::from_slice(&bytes)
                    .map_err(|_| VaultFormatError::InvalidContainer)?;
                let mut trailing = [0_u8; 1];
                if reader
                    .read(&mut trailing)
                    .map_err(|_| VaultFormatError::DecryptionFailed)?
                    != 0
                {
                    return Err(VaultFormatError::InvalidContainer);
                }
                return manifest.into_artifact(expected_ref, records, limits);
            }
            _ => return Err(VaultFormatError::InvalidContainer),
        }
    }
}

fn read_exact_private(reader: &mut impl Read, bytes: &mut [u8]) -> Result<(), VaultFormatError> {
    reader
        .read_exact(bytes)
        .map_err(|_| VaultFormatError::DecryptionFailed)
}

fn read_u8(reader: &mut impl Read) -> Result<u8, VaultFormatError> {
    let mut bytes = [0_u8; 1];
    read_exact_private(reader, &mut bytes)?;
    Ok(bytes[0])
}

fn read_u16(reader: &mut impl Read) -> Result<u16, VaultFormatError> {
    let mut bytes = [0_u8; 2];
    read_exact_private(reader, &mut bytes)?;
    Ok(u16::from_be_bytes(bytes))
}

fn read_u32(reader: &mut impl Read) -> Result<u32, VaultFormatError> {
    let mut bytes = [0_u8; 4];
    read_exact_private(reader, &mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> Result<u64, VaultFormatError> {
    let mut bytes = [0_u8; 8];
    read_exact_private(reader, &mut bytes)?;
    Ok(u64::from_be_bytes(bytes))
}

pub(super) fn safe_review(capture: &PrivateCaptureV1) -> SafeArtifactReview {
    let plaintext_bytes = capture
        .records
        .iter()
        .try_fold(0_u64, |total, record| {
            total.checked_add(u64::try_from(record.payload.len()).ok()?)
        })
        .unwrap_or(u64::MAX);
    SafeArtifactReview {
        schema_version: CAPTURE_SCHEMA_VERSION,
        capture_ref: capture.capture_ref.safe(),
        record_count: u16::try_from(capture.records.len()).unwrap_or(u16::MAX),
        byte_bucket: CaptureByteBucket::from_bytes(plaintext_bytes),
        completeness: capture.completeness,
        terminal_category: capture.terminal_category,
        checksum_valid: true,
    }
}

pub(super) struct WrittenArtifact {
    pub(super) capture_ref: CaptureRef,
    pub(super) encrypted_bytes: u64,
}

impl fmt::Debug for WrittenArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WrittenArtifact")
            .field("capture_ref", &self.capture_ref.safe())
            .field("encrypted_bytes", &self.encrypted_bytes)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VaultFormatError {
    ArtifactTooLarge,
    ChecksumMismatch,
    DecryptionFailed,
    EncodingFailed,
    EncryptionFailed,
    InvalidContainer,
    InvalidLimits,
    InvalidRecordStream,
    ManifestTooLarge,
    MissingRequiredEvidence,
    PersistenceFailed,
    PlaintextTooLarge,
    RecordTooLarge,
    ReferenceMismatch,
    UnsupportedRecipient,
    UnsupportedSchema,
}

impl fmt::Display for VaultFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ArtifactTooLarge => "encrypted private capture exceeds its size limit",
            Self::ChecksumMismatch => "private capture checksum verification failed",
            Self::DecryptionFailed => "private capture could not be decrypted",
            Self::EncodingFailed => "private capture encoding failed",
            Self::EncryptionFailed => "private capture encryption failed",
            Self::InvalidContainer => "private capture container is invalid",
            Self::InvalidLimits => "private capture limits are invalid",
            Self::InvalidRecordStream => "private capture record stream is invalid",
            Self::ManifestTooLarge => "private capture manifest exceeds its size limit",
            Self::MissingRequiredEvidence => "private capture is missing a provider exchange",
            Self::PersistenceFailed => "private capture persistence failed",
            Self::PlaintextTooLarge => "decrypted private capture exceeds its size limit",
            Self::RecordTooLarge => "private capture record exceeds its typed size limit",
            Self::ReferenceMismatch => "private capture reference does not match its artifact",
            Self::UnsupportedRecipient => "private capture encryption mode is unsupported",
            Self::UnsupportedSchema => "private capture schema is unsupported",
        })
    }
}

impl std::error::Error for VaultFormatError {}

pub(super) struct DecryptedCaptureArtifact {
    capture: PrivateCaptureV1,
    manifest: PrivateManifestFacts,
}

impl DecryptedCaptureArtifact {
    pub(super) const fn capture(&self) -> &PrivateCaptureV1 {
        &self.capture
    }

    pub(super) const fn manifest(&self) -> &PrivateManifestFacts {
        &self.manifest
    }

    fn into_capture(self) -> PrivateCaptureV1 {
        self.capture
    }

    #[cfg(test)]
    pub(super) fn from_test_parts(
        capture: PrivateCaptureV1,
        manifest: PrivateManifestFacts,
    ) -> Self {
        Self { capture, manifest }
    }
}

impl fmt::Debug for DecryptedCaptureArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DecryptedCaptureArtifact([private])")
    }
}

pub(super) struct PrivateManifestFacts {
    pub(super) schema_version: u16,
    pub(super) consent_version: u16,
    pub(super) application_version: String,
    pub(super) source_revision: String,
    pub(super) source_dirty: PrivateBuildDirty,
    pub(super) platform_os: String,
    pub(super) platform_arch: String,
    pub(super) record_count: u16,
    pub(super) plaintext_byte_count: u64,
    pub(super) checksum_valid: bool,
    pub(super) limits: PrivateManifestLimits,
}

impl fmt::Debug for PrivateManifestFacts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateManifestFacts")
            .field("schema_version", &self.schema_version)
            .field("consent_version", &self.consent_version)
            .field("record_count", &self.record_count)
            .field("plaintext_byte_count", &self.plaintext_byte_count)
            .field("checksum_valid", &self.checksum_valid)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PrivateBuildDirty {
    True,
    False,
    Unknown,
}

impl PrivateBuildDirty {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::True => "true",
            Self::False => "false",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PrivateManifestLimits {
    pub(super) arm_deadline_ms: u64,
    pub(super) operation_deadline_ms: u64,
    pub(super) record_capacity: u16,
    pub(super) exchange_capacity: u16,
    pub(super) player_request_bytes: u64,
    pub(super) player_response_bytes: u64,
    pub(super) transport_error_bytes: u64,
    pub(super) plaintext_bytes: u64,
    pub(super) encrypted_artifact_bytes: u64,
    pub(super) retained_artifacts: u16,
    pub(super) total_storage_bytes: u64,
    pub(super) retention_ms: u64,
    pub(super) writer_finalization_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestDto {
    schema_version: u16,
    consent_version: u16,
    application_version: String,
    source_revision: String,
    source_dirty: BuildDirtyDto,
    platform_os: String,
    platform_arch: String,
    trigger: TriggerDto,
    capture_ref: String,
    created_unix_ms: u64,
    completed_unix_ms: u64,
    purpose: PurposeDto,
    operation_ref: String,
    completeness: CompletenessDto,
    incomplete_reasons: Vec<ReasonDto>,
    dropped_records: u16,
    record_count: u16,
    plaintext_byte_count: u64,
    terminal_category: TerminalDto,
    record_stream_checksum: String,
    limits: LimitsDto,
    credential_values_present: bool,
}

impl ManifestDto {
    fn from_private(
        capture: &PrivateCaptureV1,
        limits: CaptureLimits,
        checksum: [u8; 32],
        record_count: u16,
        plaintext_byte_count: u64,
    ) -> Result<Self, VaultFormatError> {
        Ok(Self {
            schema_version: CAPTURE_SCHEMA_VERSION,
            consent_version: CAPTURE_CONSENT_VERSION,
            application_version: env!("CARGO_PKG_VERSION").to_owned(),
            source_revision: option_env!("UNIFIED_PLAYER_GIT_REVISION")
                .unwrap_or("unknown")
                .to_owned(),
            source_dirty: BuildDirtyDto::current(),
            platform_os: std::env::consts::OS.to_owned(),
            platform_arch: std::env::consts::ARCH.to_owned(),
            trigger: TriggerDto::from(capture.purpose),
            capture_ref: capture.capture_ref.file_stem(),
            created_unix_ms: capture.created_unix_ms,
            completed_unix_ms: capture.completed_unix_ms,
            purpose: PurposeDto::from(capture.purpose),
            operation_ref: capture.operation_ref.to_string(),
            completeness: CompletenessDto::try_from(capture.completeness)?,
            incomplete_reasons: capture
                .incomplete_reasons
                .iter()
                .copied()
                .map(ReasonDto::from)
                .collect(),
            dropped_records: capture.dropped_records,
            record_count,
            plaintext_byte_count,
            terminal_category: TerminalDto::from(capture.terminal_category),
            record_stream_checksum: encode_hex(&checksum),
            limits: LimitsDto::from(limits),
            credential_values_present: capture.credential_values_present,
        })
    }

    fn into_artifact(
        self,
        expected_ref: CaptureRef,
        records: Vec<CaptureRecordV1>,
        limits: CaptureLimits,
    ) -> Result<DecryptedCaptureArtifact, VaultFormatError> {
        let manifest = PrivateManifestFacts {
            schema_version: self.schema_version,
            consent_version: self.consent_version,
            application_version: self.application_version.clone(),
            source_revision: self.source_revision.clone(),
            source_dirty: self.source_dirty.into(),
            platform_os: self.platform_os.clone(),
            platform_arch: self.platform_arch.clone(),
            record_count: self.record_count,
            plaintext_byte_count: self.plaintext_byte_count,
            checksum_valid: true,
            limits: PrivateManifestLimits::from(&self.limits),
        };
        let capture = self.into_private(expected_ref, records, limits)?;
        Ok(DecryptedCaptureArtifact { capture, manifest })
    }

    fn into_private(
        self,
        expected_ref: CaptureRef,
        records: Vec<CaptureRecordV1>,
        limits: CaptureLimits,
    ) -> Result<PrivateCaptureV1, VaultFormatError> {
        if self.schema_version != CAPTURE_SCHEMA_VERSION
            || self.consent_version != CAPTURE_CONSENT_VERSION
        {
            return Err(VaultFormatError::UnsupportedSchema);
        }
        if !valid_manifest_token(&self.application_version, 64, true)
            || !valid_source_revision(&self.source_revision)
            || !valid_manifest_token(&self.platform_os, 32, false)
            || !valid_manifest_token(&self.platform_arch, 32, false)
            || self.trigger != TriggerDto::from(CapturePurpose::from(self.purpose))
        {
            return Err(VaultFormatError::InvalidContainer);
        }
        self.limits.validate_against(limits)?;
        let capture_ref = CaptureRef::from_file_stem(&self.capture_ref)
            .map_err(|_| VaultFormatError::InvalidContainer)?;
        if capture_ref != expected_ref {
            return Err(VaultFormatError::ReferenceMismatch);
        }
        if usize::from(self.record_count) != records.len()
            || self.record_count > limits.record_capacity.saturating_add(1)
            || self.completed_unix_ms < self.created_unix_ms
        {
            return Err(VaultFormatError::InvalidRecordStream);
        }
        let plaintext_bytes = records.iter().try_fold(0_u64, |total, record| {
            total
                .checked_add(
                    u64::try_from(record.payload.len())
                        .map_err(|_| VaultFormatError::PlaintextTooLarge)?,
                )
                .ok_or(VaultFormatError::PlaintextTooLarge)
        })?;
        if plaintext_bytes != self.plaintext_byte_count
            || plaintext_bytes
                > limits
                    .plaintext_bytes
                    .saturating_add(TERMINAL_RECORD_RESERVE_BYTES)
        {
            return Err(VaultFormatError::PlaintextTooLarge);
        }
        let expected_checksum = encode_hex(&record_stream_checksum(&records));
        if !constant_time_text_eq(
            expected_checksum.as_bytes(),
            self.record_stream_checksum.as_bytes(),
        ) {
            return Err(VaultFormatError::ChecksumMismatch);
        }
        let completeness = self.completeness.into();
        let incomplete_reasons = self
            .incomplete_reasons
            .into_iter()
            .map(IncompleteReason::from)
            .collect::<Vec<_>>();
        let capture = PrivateCaptureV1 {
            capture_ref,
            created_unix_ms: self.created_unix_ms,
            completed_unix_ms: self.completed_unix_ms,
            purpose: self.purpose.into(),
            operation_ref: SafeOperationRef::from_hex(&self.operation_ref)
                .map_err(|_| VaultFormatError::InvalidContainer)?,
            records,
            completeness,
            incomplete_reasons,
            dropped_records: self.dropped_records,
            terminal_category: self.terminal_category.into(),
            credential_values_present: self.credential_values_present,
        };
        validate_private_capture(&capture, limits)?;
        Ok(capture)
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BuildDirtyDto {
    True,
    False,
    Unknown,
}

impl BuildDirtyDto {
    fn current() -> Self {
        match option_env!("UNIFIED_PLAYER_GIT_DIRTY") {
            Some("true") => Self::True,
            Some("false") => Self::False,
            _ => Self::Unknown,
        }
    }
}

impl From<BuildDirtyDto> for PrivateBuildDirty {
    fn from(value: BuildDirtyDto) -> Self {
        match value {
            BuildDirtyDto::True => Self::True,
            BuildDirtyDto::False => Self::False,
            BuildDirtyDto::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TriggerDto {
    ManualForeground,
    Prefetch,
    Resume,
    Probe,
    Replay,
    Comparison,
}

impl From<CapturePurpose> for TriggerDto {
    fn from(value: CapturePurpose) -> Self {
        match value {
            CapturePurpose::InteractivePlayback => Self::ManualForeground,
            CapturePurpose::Prefetch => Self::Prefetch,
            CapturePurpose::Resume => Self::Resume,
            CapturePurpose::Probe => Self::Probe,
            CapturePurpose::Replay => Self::Replay,
            CapturePurpose::Comparison => Self::Comparison,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsDto {
    arm_deadline_ms: u64,
    operation_deadline_ms: u64,
    record_capacity: u16,
    exchange_capacity: u16,
    player_request_bytes: u64,
    player_response_bytes: u64,
    transport_error_bytes: u64,
    plaintext_bytes: u64,
    encrypted_artifact_bytes: u64,
    retained_artifacts: u16,
    total_storage_bytes: u64,
    retention_ms: u64,
    writer_finalization_ms: u64,
}

impl From<CaptureLimits> for LimitsDto {
    fn from(value: CaptureLimits) -> Self {
        Self {
            arm_deadline_ms: duration_ms(value.arm_deadline),
            operation_deadline_ms: duration_ms(value.operation_deadline),
            record_capacity: value.record_capacity,
            exchange_capacity: value.exchange_capacity,
            player_request_bytes: value.player_request_bytes,
            player_response_bytes: value.player_response_bytes,
            transport_error_bytes: value.transport_error_bytes,
            plaintext_bytes: value.plaintext_bytes,
            encrypted_artifact_bytes: value.encrypted_artifact_bytes,
            retained_artifacts: value.retained_artifacts,
            total_storage_bytes: value.total_storage_bytes,
            retention_ms: duration_ms(value.retention),
            writer_finalization_ms: duration_ms(value.writer_finalization),
        }
    }
}

impl LimitsDto {
    fn validate_against(&self, maximum: CaptureLimits) -> Result<(), VaultFormatError> {
        if self.arm_deadline_ms == 0
            || self.operation_deadline_ms == 0
            || self.record_capacity == 0
            || self.exchange_capacity == 0
            || self.player_request_bytes == 0
            || self.player_response_bytes == 0
            || self.transport_error_bytes == 0
            || self.plaintext_bytes == 0
            || self.encrypted_artifact_bytes == 0
            || self.retained_artifacts == 0
            || self.total_storage_bytes == 0
            || self.retention_ms == 0
            || self.writer_finalization_ms == 0
            || self.record_capacity > maximum.record_capacity
            || self.exchange_capacity > maximum.exchange_capacity
            || self.player_request_bytes > maximum.player_request_bytes
            || self.player_response_bytes > maximum.player_response_bytes
            || self.transport_error_bytes > maximum.transport_error_bytes
            || self.plaintext_bytes > maximum.plaintext_bytes
            || self.encrypted_artifact_bytes > maximum.encrypted_artifact_bytes
        {
            return Err(VaultFormatError::InvalidLimits);
        }
        Ok(())
    }
}

impl From<&LimitsDto> for PrivateManifestLimits {
    fn from(value: &LimitsDto) -> Self {
        Self {
            arm_deadline_ms: value.arm_deadline_ms,
            operation_deadline_ms: value.operation_deadline_ms,
            record_capacity: value.record_capacity,
            exchange_capacity: value.exchange_capacity,
            player_request_bytes: value.player_request_bytes,
            player_response_bytes: value.player_response_bytes,
            transport_error_bytes: value.transport_error_bytes,
            plaintext_bytes: value.plaintext_bytes,
            encrypted_artifact_bytes: value.encrypted_artifact_bytes,
            retained_artifacts: value.retained_artifacts,
            total_storage_bytes: value.total_storage_bytes,
            retention_ms: value.retention_ms,
            writer_finalization_ms: value.writer_finalization_ms,
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn valid_manifest_token(value: &str, maximum_len: usize, allow_dot: bool) -> bool {
    !value.is_empty()
        && value.len() <= maximum_len
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_')
                || (allow_dot && byte == b'.')
        })
}

fn valid_source_revision(value: &str) -> bool {
    value == "unknown"
        || (!value.is_empty()
            && value.len() <= 64
            && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PurposeDto {
    // Schema v1 readers intentionally accept every older purpose and reject
    // purpose tags they do not know; adding a purpose preserves old-artifact
    // readability without weakening that closed-world forward boundary.
    InteractivePlayback,
    Prefetch,
    Resume,
    Probe,
    Replay,
    Comparison,
}

impl From<CapturePurpose> for PurposeDto {
    fn from(value: CapturePurpose) -> Self {
        match value {
            CapturePurpose::InteractivePlayback => Self::InteractivePlayback,
            CapturePurpose::Prefetch => Self::Prefetch,
            CapturePurpose::Resume => Self::Resume,
            CapturePurpose::Probe => Self::Probe,
            CapturePurpose::Replay => Self::Replay,
            CapturePurpose::Comparison => Self::Comparison,
        }
    }
}

impl From<PurposeDto> for CapturePurpose {
    fn from(value: PurposeDto) -> Self {
        match value {
            PurposeDto::InteractivePlayback => Self::InteractivePlayback,
            PurposeDto::Prefetch => Self::Prefetch,
            PurposeDto::Resume => Self::Resume,
            PurposeDto::Probe => Self::Probe,
            PurposeDto::Replay => Self::Replay,
            PurposeDto::Comparison => Self::Comparison,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CompletenessDto {
    Complete,
    Incomplete,
}

impl TryFrom<CaptureCompleteness> for CompletenessDto {
    type Error = VaultFormatError;

    fn try_from(value: CaptureCompleteness) -> Result<Self, Self::Error> {
        match value {
            CaptureCompleteness::Pending => Err(VaultFormatError::InvalidContainer),
            CaptureCompleteness::Complete => Ok(Self::Complete),
            CaptureCompleteness::Incomplete => Ok(Self::Incomplete),
        }
    }
}

impl From<CompletenessDto> for CaptureCompleteness {
    fn from(value: CompletenessDto) -> Self {
        match value {
            CompletenessDto::Complete => Self::Complete,
            CompletenessDto::Incomplete => Self::Incomplete,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReasonDto {
    RecordCapacity,
    ExchangeCapacity,
    RecordSize,
    QueueCapacity,
    PlaintextBudget,
    OperationDeadline,
    FinalizationDeadline,
    MissingProviderExchange,
    WriterUnavailable,
    Abandoned,
    Shutdown,
    Persistence,
    Truncated,
}

impl From<IncompleteReason> for ReasonDto {
    fn from(value: IncompleteReason) -> Self {
        match value {
            IncompleteReason::RecordCapacity => Self::RecordCapacity,
            IncompleteReason::ExchangeCapacity => Self::ExchangeCapacity,
            IncompleteReason::RecordSize => Self::RecordSize,
            IncompleteReason::QueueCapacity => Self::QueueCapacity,
            IncompleteReason::PlaintextBudget => Self::PlaintextBudget,
            IncompleteReason::OperationDeadline => Self::OperationDeadline,
            IncompleteReason::FinalizationDeadline => Self::FinalizationDeadline,
            IncompleteReason::MissingProviderExchange => Self::MissingProviderExchange,
            IncompleteReason::WriterUnavailable => Self::WriterUnavailable,
            IncompleteReason::Abandoned => Self::Abandoned,
            IncompleteReason::Shutdown => Self::Shutdown,
            IncompleteReason::Persistence => Self::Persistence,
            IncompleteReason::Truncated => Self::Truncated,
        }
    }
}

impl From<ReasonDto> for IncompleteReason {
    fn from(value: ReasonDto) -> Self {
        match value {
            ReasonDto::RecordCapacity => Self::RecordCapacity,
            ReasonDto::ExchangeCapacity => Self::ExchangeCapacity,
            ReasonDto::RecordSize => Self::RecordSize,
            ReasonDto::QueueCapacity => Self::QueueCapacity,
            ReasonDto::PlaintextBudget => Self::PlaintextBudget,
            ReasonDto::OperationDeadline => Self::OperationDeadline,
            ReasonDto::FinalizationDeadline => Self::FinalizationDeadline,
            ReasonDto::MissingProviderExchange => Self::MissingProviderExchange,
            ReasonDto::WriterUnavailable => Self::WriterUnavailable,
            ReasonDto::Abandoned => Self::Abandoned,
            ReasonDto::Shutdown => Self::Shutdown,
            ReasonDto::Persistence => Self::Persistence,
            ReasonDto::Truncated => Self::Truncated,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalDto {
    Success,
    Failed,
    Cancelled,
    Superseded,
    TimedOut,
    Panicked,
}

impl From<SafeTerminalCategory> for TerminalDto {
    fn from(value: SafeTerminalCategory) -> Self {
        match value {
            SafeTerminalCategory::Success => Self::Success,
            SafeTerminalCategory::Failed => Self::Failed,
            SafeTerminalCategory::Cancelled => Self::Cancelled,
            SafeTerminalCategory::Superseded => Self::Superseded,
            SafeTerminalCategory::TimedOut => Self::TimedOut,
            SafeTerminalCategory::Panicked => Self::Panicked,
        }
    }
}

impl From<TerminalDto> for SafeTerminalCategory {
    fn from(value: TerminalDto) -> Self {
        match value {
            TerminalDto::Success => Self::Success,
            TerminalDto::Failed => Self::Failed,
            TerminalDto::Cancelled => Self::Cancelled,
            TerminalDto::Superseded => Self::Superseded,
            TerminalDto::TimedOut => Self::TimedOut,
            TerminalDto::Panicked => Self::Panicked,
        }
    }
}

fn validate_private_capture(
    capture: &PrivateCaptureV1,
    limits: CaptureLimits,
) -> Result<(), VaultFormatError> {
    if capture.completed_unix_ms < capture.created_unix_ms
        || capture.records.len() > usize::from(limits.record_capacity).saturating_add(1)
        || capture.completeness == CaptureCompleteness::Pending
        || (capture.completeness == CaptureCompleteness::Complete
            && (!capture.incomplete_reasons.is_empty() || capture.dropped_records != 0))
        || (capture.completeness == CaptureCompleteness::Incomplete
            && capture.incomplete_reasons.is_empty())
    {
        return Err(VaultFormatError::InvalidContainer);
    }
    for (index, reason) in capture.incomplete_reasons.iter().enumerate() {
        if capture.incomplete_reasons[..index].contains(reason) {
            return Err(VaultFormatError::InvalidContainer);
        }
    }

    let mut total = 0_u64;
    let mut evidence_total = 0_u64;
    let mut exchanges = 0_u16;
    let mut previous_offset = None;
    let mut terminal = None;
    let mut replay_results = 0_u8;
    for (index, record) in capture.records.iter().enumerate() {
        if usize::from(record.sequence) != index {
            return Err(VaultFormatError::InvalidRecordStream);
        }
        if previous_offset.is_some_and(|offset| record.monotonic_offset_ms < offset) {
            return Err(VaultFormatError::InvalidRecordStream);
        }
        previous_offset = Some(record.monotonic_offset_ms);
        let payload_bytes =
            u64::try_from(record.payload.len()).map_err(|_| VaultFormatError::RecordTooLarge)?;
        if payload_bytes > limits.record_bytes(record.kind) {
            return Err(VaultFormatError::RecordTooLarge);
        }
        if record.kind == CaptureRecordKind::TerminalOutcome {
            if payload_bytes > TERMINAL_RECORD_RESERVE_BYTES
                || terminal.is_some()
                || index + 1 != capture.records.len()
            {
                return Err(VaultFormatError::InvalidRecordStream);
            }
            terminal = Some(terminal_from_record(record)?);
        } else {
            evidence_total = evidence_total
                .checked_add(payload_bytes)
                .ok_or(VaultFormatError::PlaintextTooLarge)?;
        }
        if record.kind == CaptureRecordKind::OperationBoundary
            && record.endpoint_role == EndpointRole::Operation
            && matches!(
                record.transport_kind,
                TransportKind::OfflineReplay | TransportKind::FreshReplay
            )
        {
            replay_results = replay_results.saturating_add(1);
        }
        if record.kind.starts_provider_exchange() {
            exchanges = exchanges
                .checked_add(1)
                .ok_or(VaultFormatError::InvalidRecordStream)?;
        }
        total = total
            .checked_add(payload_bytes)
            .ok_or(VaultFormatError::PlaintextTooLarge)?;
    }
    if evidence_total > limits.plaintext_bytes
        || total
            > limits
                .plaintext_bytes
                .saturating_add(TERMINAL_RECORD_RESERVE_BYTES)
    {
        return Err(VaultFormatError::PlaintextTooLarge);
    }
    if terminal.is_some_and(|terminal| terminal != capture.terminal_category) {
        return Err(VaultFormatError::InvalidRecordStream);
    }
    if capture.purpose == CapturePurpose::Replay && (replay_results != 1 || terminal.is_none()) {
        return Err(VaultFormatError::MissingRequiredEvidence);
    }
    if capture.purpose == CapturePurpose::Comparison {
        validate_comparison_artifact(capture)?;
    }
    let evidence_records = capture
        .records
        .len()
        .saturating_sub(usize::from(terminal.is_some()));
    if evidence_records > usize::from(limits.record_capacity) {
        return Err(VaultFormatError::InvalidRecordStream);
    }
    if exchanges > limits.exchange_capacity {
        return Err(VaultFormatError::InvalidRecordStream);
    }
    let missing_exchange = !matches!(
        capture.purpose,
        CapturePurpose::Replay | CapturePurpose::Comparison
    ) && exchanges == 0;
    if capture.completeness == CaptureCompleteness::Complete && missing_exchange {
        return Err(VaultFormatError::MissingRequiredEvidence);
    }
    if missing_exchange
        != capture
            .incomplete_reasons
            .contains(&IncompleteReason::MissingProviderExchange)
    {
        return Err(VaultFormatError::InvalidContainer);
    }
    Ok(())
}

fn terminal_from_record(
    record: &CaptureRecordV1,
) -> Result<SafeTerminalCategory, VaultFormatError> {
    let payload =
        decode_fields(record.payload()).map_err(|_| VaultFormatError::InvalidRecordStream)?;
    if payload.kind() != PrivatePayloadKind::TerminalOutcome {
        return Err(VaultFormatError::InvalidRecordStream);
    }
    match payload.field_bytes(field::OUTCOME) {
        Some(b"success") => Ok(SafeTerminalCategory::Success),
        Some(b"failed") => Ok(SafeTerminalCategory::Failed),
        Some(b"cancelled") => Ok(SafeTerminalCategory::Cancelled),
        Some(b"superseded") => Ok(SafeTerminalCategory::Superseded),
        Some(b"timed_out") => Ok(SafeTerminalCategory::TimedOut),
        Some(b"panicked") => Ok(SafeTerminalCategory::Panicked),
        _ => Err(VaultFormatError::InvalidRecordStream),
    }
}

fn validate_capture_summary(
    capture: &PrivateCaptureV1,
    record_count: u16,
    exchange_count: u16,
) -> Result<(), VaultFormatError> {
    if capture.completed_unix_ms < capture.created_unix_ms
        || capture.completeness == CaptureCompleteness::Pending
        || (capture.completeness == CaptureCompleteness::Complete
            && (!capture.incomplete_reasons.is_empty() || capture.dropped_records != 0))
        || (capture.completeness == CaptureCompleteness::Incomplete
            && capture.incomplete_reasons.is_empty())
        || (!capture.records.is_empty()
            && u16::try_from(capture.records.len()).ok() != Some(record_count))
    {
        return Err(VaultFormatError::InvalidContainer);
    }
    for (index, reason) in capture.incomplete_reasons.iter().enumerate() {
        if capture.incomplete_reasons[..index].contains(reason) {
            return Err(VaultFormatError::InvalidContainer);
        }
    }
    if capture.purpose == CapturePurpose::Replay {
        let replay_results = capture
            .records
            .iter()
            .filter(|record| {
                record.kind == CaptureRecordKind::OperationBoundary
                    && record.endpoint_role == EndpointRole::Operation
                    && matches!(
                        record.transport_kind,
                        TransportKind::OfflineReplay | TransportKind::FreshReplay
                    )
            })
            .count();
        let terminals = capture
            .records
            .iter()
            .filter(|record| record.kind == CaptureRecordKind::TerminalOutcome)
            .count();
        if replay_results != 1 || terminals != 1 {
            return Err(VaultFormatError::MissingRequiredEvidence);
        }
    }
    if capture.purpose == CapturePurpose::Comparison {
        validate_comparison_artifact(capture)?;
    }
    let missing_exchange = !matches!(
        capture.purpose,
        CapturePurpose::Replay | CapturePurpose::Comparison
    ) && exchange_count == 0;
    if capture.completeness == CaptureCompleteness::Complete && missing_exchange {
        return Err(VaultFormatError::MissingRequiredEvidence);
    }
    if missing_exchange
        != capture
            .incomplete_reasons
            .contains(&IncompleteReason::MissingProviderExchange)
    {
        return Err(VaultFormatError::InvalidContainer);
    }
    Ok(())
}

fn validate_comparison_artifact(capture: &PrivateCaptureV1) -> Result<(), VaultFormatError> {
    if capture.completeness != CaptureCompleteness::Complete
        || capture.dropped_records != 0
        || !capture.incomplete_reasons.is_empty()
        || capture.terminal_category != SafeTerminalCategory::Success
        || capture.records.len() != 2
    {
        return Err(VaultFormatError::InvalidRecordStream);
    }
    let result = &capture.records[0];
    let terminal = &capture.records[1];
    if result.sequence != 0
        || result.exchange_ref.is_some()
        || result.endpoint_role != EndpointRole::Operation
        || result.client_kind != ProviderClientKind::Unknown
        || result.transport_kind != TransportKind::Unknown
        || result.attempt != 0
        || result.kind != CaptureRecordKind::OperationBoundary
        || terminal.kind != CaptureRecordKind::TerminalOutcome
        || validate_comparison_result_payload(result.payload.expose()).is_err()
    {
        return Err(VaultFormatError::InvalidRecordStream);
    }
    Ok(())
}

fn record_stream_checksum(records: &[CaptureRecordV1]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(CHECKSUM_DOMAIN);
    for record in records {
        update_record_checksum(&mut hasher, record);
    }
    hasher.finalize().into()
}

fn update_record_checksum(hasher: &mut Sha256, record: &CaptureRecordV1) {
    hasher.update(record.sequence.to_be_bytes());
    hasher.update(record.monotonic_offset_ms.to_be_bytes());
    hasher.update([u8::from(record.exchange_ref.is_some())]);
    hasher.update(record.exchange_ref.map_or([0; 8], ExchangeRef::bytes));
    hasher.update([endpoint_role_tag(record.endpoint_role)]);
    hasher.update([provider_client_tag(record.client_kind)]);
    hasher.update([transport_kind_tag(record.transport_kind)]);
    hasher.update([record.attempt]);
    hasher.update([record_kind_tag(record.kind)]);
    hasher.update(
        u64::try_from(record.payload.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    hasher.update(record.payload.expose());
}

const fn record_kind_tag(kind: CaptureRecordKind) -> u8 {
    match kind {
        CaptureRecordKind::OperationBoundary => 1,
        CaptureRecordKind::HttpRequest => 2,
        CaptureRecordKind::HttpResponse => 3,
        CaptureRecordKind::NetworkFailure => 4,
        CaptureRecordKind::SyntheticFixture => 5,
        CaptureRecordKind::AuthSelection => 6,
        CaptureRecordKind::PlayerParse => 7,
        CaptureRecordKind::FormatInventory => 8,
        CaptureRecordKind::SelectionDecision => 9,
        CaptureRecordKind::BrowserExchange => 10,
        CaptureRecordKind::MediaProbe => 11,
        CaptureRecordKind::DecodeStage => 12,
        CaptureRecordKind::TerminalOutcome => 13,
    }
}

fn record_kind_from_tag(tag: u8) -> Result<CaptureRecordKind, VaultFormatError> {
    match tag {
        1 => Ok(CaptureRecordKind::OperationBoundary),
        2 => Ok(CaptureRecordKind::HttpRequest),
        3 => Ok(CaptureRecordKind::HttpResponse),
        4 => Ok(CaptureRecordKind::NetworkFailure),
        5 => Ok(CaptureRecordKind::SyntheticFixture),
        6 => Ok(CaptureRecordKind::AuthSelection),
        7 => Ok(CaptureRecordKind::PlayerParse),
        8 => Ok(CaptureRecordKind::FormatInventory),
        9 => Ok(CaptureRecordKind::SelectionDecision),
        10 => Ok(CaptureRecordKind::BrowserExchange),
        11 => Ok(CaptureRecordKind::MediaProbe),
        12 => Ok(CaptureRecordKind::DecodeStage),
        13 => Ok(CaptureRecordKind::TerminalOutcome),
        _ => Err(VaultFormatError::InvalidRecordStream),
    }
}

const fn endpoint_role_tag(role: EndpointRole) -> u8 {
    match role {
        EndpointRole::Unknown => 0,
        EndpointRole::Operation => 1,
        EndpointRole::PlayerApi => 2,
        EndpointRole::BrowserPlayer => 3,
        EndpointRole::Media => 4,
        EndpointRole::BrowserMedia => 5,
        EndpointRole::Decoder => 6,
    }
}

fn endpoint_role_from_tag(tag: u8) -> Result<EndpointRole, VaultFormatError> {
    match tag {
        0 => Ok(EndpointRole::Unknown),
        1 => Ok(EndpointRole::Operation),
        2 => Ok(EndpointRole::PlayerApi),
        3 => Ok(EndpointRole::BrowserPlayer),
        4 => Ok(EndpointRole::Media),
        5 => Ok(EndpointRole::BrowserMedia),
        6 => Ok(EndpointRole::Decoder),
        _ => Err(VaultFormatError::InvalidRecordStream),
    }
}

const fn provider_client_tag(kind: ProviderClientKind) -> u8 {
    match kind {
        ProviderClientKind::Unknown => 0,
        ProviderClientKind::Web => 1,
        ProviderClientKind::WebRemix => 2,
        ProviderClientKind::Android => 3,
        ProviderClientKind::Ios => 4,
        ProviderClientKind::TvHtml5 => 5,
        ProviderClientKind::Spotify => 6,
    }
}

fn provider_client_from_tag(tag: u8) -> Result<ProviderClientKind, VaultFormatError> {
    match tag {
        0 => Ok(ProviderClientKind::Unknown),
        1 => Ok(ProviderClientKind::Web),
        2 => Ok(ProviderClientKind::WebRemix),
        3 => Ok(ProviderClientKind::Android),
        4 => Ok(ProviderClientKind::Ios),
        5 => Ok(ProviderClientKind::TvHtml5),
        6 => Ok(ProviderClientKind::Spotify),
        _ => Err(VaultFormatError::InvalidRecordStream),
    }
}

const fn transport_kind_tag(kind: TransportKind) -> u8 {
    match kind {
        TransportKind::Unknown => 0,
        TransportKind::NativeHttp => 1,
        TransportKind::BrowserCdp => 2,
        TransportKind::MediaRange => 3,
        TransportKind::OfflineReplay => 4,
        TransportKind::FreshReplay => 5,
    }
}

fn transport_kind_from_tag(tag: u8) -> Result<TransportKind, VaultFormatError> {
    match tag {
        0 => Ok(TransportKind::Unknown),
        1 => Ok(TransportKind::NativeHttp),
        2 => Ok(TransportKind::BrowserCdp),
        3 => Ok(TransportKind::MediaRange),
        4 => Ok(TransportKind::OfflineReplay),
        5 => Ok(TransportKind::FreshReplay),
        _ => Err(VaultFormatError::InvalidRecordStream),
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn constant_time_text_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use super::{
        read_encrypted, safe_review, write_encrypted, write_plaintext_container, VaultFormatError,
    };
    use crate::developer_capture::{
        model::{
            CaptureCompleteness, CaptureLimits, CapturePurpose, CaptureRecordKind, CaptureRecordV1,
            CaptureRef, EndpointRole, ExchangeRef, IncompleteReason, PrivateCaptureV1,
            ProviderClientKind, SafeOperationRef, SafeTerminalCategory, SensitiveBytes,
            TransportKind,
        },
        payload::{encode_fields, field, PrivateField, PrivatePayloadKind},
        security::{secure_new_file, CapturePassphrase},
    };
    use age::Encryptor;
    use std::{fs::File, io::Read as _};

    fn fixture_capture(capture_ref: CaptureRef) -> PrivateCaptureV1 {
        PrivateCaptureV1::new(
            capture_ref,
            1_000,
            1_100,
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([1, 2, 3, 4]),
            vec![CaptureRecordV1::with_context(
                0,
                5,
                Some(ExchangeRef::from_bytes([9; 8])),
                EndpointRole::PlayerApi,
                ProviderClientKind::WebRemix,
                TransportKind::NativeHttp,
                2,
                CaptureRecordKind::HttpRequest,
                SensitiveBytes::new(b"seeded-private-provider-payload".to_vec()),
            )],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Failed,
        )
    }

    fn boundary_limits() -> CaptureLimits {
        CaptureLimits {
            record_capacity: 2,
            exchange_capacity: 1,
            player_request_bytes: 6,
            player_response_bytes: 6,
            transport_error_bytes: 4,
            plaintext_bytes: 12,
            encrypted_artifact_bytes: 192 * 1024,
            total_storage_bytes: 256 * 1024,
            ..CaptureLimits::default()
        }
    }

    #[test]
    fn age_round_trip_is_exact_and_review_is_safe() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.partial");
        let capture_ref = CaptureRef::from_bytes([7; 16]);
        let capture = fixture_capture(capture_ref);
        let passphrase = CapturePassphrase::new("a strong test passphrase".to_owned()).unwrap();
        let file = secure_new_file(&path).unwrap();
        let written =
            write_encrypted(file, &capture, &passphrase, CaptureLimits::default()).unwrap();
        assert_eq!(written.capture_ref, capture_ref);

        let mut ciphertext = Vec::new();
        File::open(&path)
            .unwrap()
            .read_to_end(&mut ciphertext)
            .unwrap();
        assert!(ciphertext.starts_with(b"age-encryption.org/v1"));
        assert!(!ciphertext
            .windows(b"seeded-private-provider-payload".len())
            .any(|window| window == b"seeded-private-provider-payload"));

        let reopened = read_encrypted(
            File::open(&path).unwrap(),
            capture_ref,
            &passphrase,
            CaptureLimits::default(),
        )
        .unwrap();
        assert_eq!(
            reopened.records[0].payload.expose(),
            b"seeded-private-provider-payload"
        );
        assert_eq!(
            reopened.records[0].exchange_ref,
            Some(ExchangeRef::from_bytes([9; 8]))
        );
        assert_eq!(reopened.records[0].endpoint_role, EndpointRole::PlayerApi);
        assert_eq!(
            reopened.records[0].client_kind,
            ProviderClientKind::WebRemix
        );
        assert_eq!(
            reopened.records[0].transport_kind,
            TransportKind::NativeHttp
        );
        assert_eq!(reopened.records[0].attempt, 2);
        let review = safe_review(&reopened);
        assert_eq!(review.capture_ref, capture_ref.safe());
        assert_eq!(review.record_count, 1);
        assert!(review.checksum_valid);
    }

    #[test]
    fn wrong_passphrase_tamper_and_truncation_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.partial");
        let capture_ref = CaptureRef::from_bytes([8; 16]);
        let passphrase = CapturePassphrase::new("correct passphrase".to_owned()).unwrap();
        write_encrypted(
            secure_new_file(&path).unwrap(),
            &fixture_capture(capture_ref),
            &passphrase,
            CaptureLimits::default(),
        )
        .unwrap();

        let wrong = CapturePassphrase::new("wrong passphrase".to_owned()).unwrap();
        assert_eq!(
            read_encrypted(
                File::open(&path).unwrap(),
                capture_ref,
                &wrong,
                CaptureLimits::default(),
            )
            .unwrap_err(),
            VaultFormatError::DecryptionFailed
        );

        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            read_encrypted(
                File::open(&path).unwrap(),
                capture_ref,
                &passphrase,
                CaptureLimits::default(),
            )
            .unwrap_err(),
            VaultFormatError::DecryptionFailed
        );

        bytes.truncate(bytes.len() / 2);
        std::fs::write(&path, bytes).unwrap();
        assert!(read_encrypted(
            File::open(&path).unwrap(),
            capture_ref,
            &passphrase,
            CaptureLimits::default(),
        )
        .is_err());
    }

    #[test]
    fn raw_payload_budget_round_trips_at_its_exact_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.partial");
        let capture_ref = CaptureRef::from_bytes([9; 16]);
        let capture = PrivateCaptureV1::new(
            capture_ref,
            1_000,
            1_100,
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([1, 2, 3, 4]),
            vec![
                CaptureRecordV1::new(
                    0,
                    5,
                    CaptureRecordKind::HttpRequest,
                    SensitiveBytes::new(vec![0x41; 6]),
                ),
                CaptureRecordV1::new(
                    1,
                    6,
                    CaptureRecordKind::HttpResponse,
                    SensitiveBytes::new(vec![0x42; 6]),
                ),
            ],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Success,
        );
        let passphrase = CapturePassphrase::new("boundary test passphrase".to_owned()).unwrap();
        write_encrypted(
            secure_new_file(&path).unwrap(),
            &capture,
            &passphrase,
            boundary_limits(),
        )
        .unwrap();

        let reopened = read_encrypted(
            File::open(path).unwrap(),
            capture_ref,
            &passphrase,
            boundary_limits(),
        )
        .unwrap();
        assert_eq!(reopened.records.len(), 2);
        assert_eq!(reopened.records[0].payload.expose(), &[0x41; 6]);
        assert_eq!(reopened.records[1].payload.expose(), &[0x42; 6]);
    }

    #[test]
    fn evidence_record_and_plaintext_limits_round_trip_with_reserved_terminal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("terminal-reserve.partial");
        let capture_ref = CaptureRef::from_bytes([10; 16]);
        let limits = CaptureLimits {
            record_capacity: 2,
            exchange_capacity: 1,
            player_request_bytes: 32,
            player_response_bytes: 32,
            transport_error_bytes: 64,
            plaintext_bytes: 64,
            encrypted_artifact_bytes: 192 * 1024,
            total_storage_bytes: 256 * 1024,
            ..CaptureLimits::default()
        };
        let terminal = encode_fields(
            PrivatePayloadKind::TerminalOutcome,
            &[PrivateField::text(field::OUTCOME, "success")],
        )
        .unwrap();
        let capture = PrivateCaptureV1::new(
            capture_ref,
            1,
            2,
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([1; 4]),
            vec![
                CaptureRecordV1::new(
                    0,
                    0,
                    CaptureRecordKind::HttpRequest,
                    SensitiveBytes::new(vec![1; 32]),
                ),
                CaptureRecordV1::new(
                    1,
                    1,
                    CaptureRecordKind::HttpResponse,
                    SensitiveBytes::new(vec![2; 32]),
                ),
                CaptureRecordV1::new(2, 2, CaptureRecordKind::TerminalOutcome, terminal),
            ],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Success,
        );
        let passphrase =
            CapturePassphrase::new("terminal reserve boundary passphrase".to_owned()).unwrap();
        write_encrypted(
            secure_new_file(&path).unwrap(),
            &capture,
            &passphrase,
            limits,
        )
        .unwrap();

        let reopened =
            read_encrypted(File::open(path).unwrap(), capture_ref, &passphrase, limits).unwrap();
        assert_eq!(reopened.records.len(), 3);
        assert_eq!(
            reopened.records.last().map(CaptureRecordV1::kind),
            Some(CaptureRecordKind::TerminalOutcome)
        );
        assert_eq!(
            reopened.records[..2]
                .iter()
                .map(|record| record.payload.len())
                .sum::<usize>(),
            usize::try_from(limits.plaintext_bytes).unwrap()
        );
    }

    #[test]
    fn schema_v1_purpose_extension_keeps_old_tags_and_fails_forward_closed() {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum LegacyPurposeDto {
            InteractivePlayback,
            Prefetch,
            Resume,
            Probe,
            Replay,
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum LegacyTriggerDto {
            ManualForeground,
            Prefetch,
            Resume,
            Probe,
            Replay,
        }

        for (encoded, expected) in [
            (
                "\"interactive_playback\"",
                CapturePurpose::InteractivePlayback,
            ),
            ("\"prefetch\"", CapturePurpose::Prefetch),
            ("\"resume\"", CapturePurpose::Resume),
            ("\"probe\"", CapturePurpose::Probe),
            ("\"replay\"", CapturePurpose::Replay),
        ] {
            let decoded: super::PurposeDto = serde_json::from_str(encoded).unwrap();
            assert_eq!(CapturePurpose::from(decoded), expected);
            assert!(serde_json::from_str::<LegacyPurposeDto>(encoded).is_ok());
        }
        let comparison: super::PurposeDto = serde_json::from_str("\"comparison\"").unwrap();
        assert_eq!(CapturePurpose::from(comparison), CapturePurpose::Comparison);
        assert!(serde_json::from_str::<LegacyPurposeDto>("\"comparison\"").is_err());
        assert!(serde_json::from_str::<super::PurposeDto>("\"future-purpose\"").is_err());
        for (encoded, expected) in [
            ("\"manual_foreground\"", super::TriggerDto::ManualForeground),
            ("\"prefetch\"", super::TriggerDto::Prefetch),
            ("\"resume\"", super::TriggerDto::Resume),
            ("\"probe\"", super::TriggerDto::Probe),
            ("\"replay\"", super::TriggerDto::Replay),
        ] {
            let decoded: super::TriggerDto = serde_json::from_str(encoded).unwrap();
            assert_eq!(decoded, expected);
            assert!(serde_json::from_str::<LegacyTriggerDto>(encoded).is_ok());
        }
        let comparison: super::TriggerDto = serde_json::from_str("\"comparison\"").unwrap();
        assert_eq!(comparison, super::TriggerDto::Comparison);
        assert!(serde_json::from_str::<LegacyTriggerDto>("\"comparison\"").is_err());
        assert!(serde_json::from_str::<super::TriggerDto>("\"future-trigger\"").is_err());
    }

    #[test]
    fn complete_capture_requires_a_provider_exchange() {
        let directory = tempfile::tempdir().unwrap();
        let passphrase = CapturePassphrase::new("evidence test passphrase".to_owned()).unwrap();
        for (name, records) in [
            ("empty", Vec::new()),
            (
                "boundary-only",
                vec![CaptureRecordV1::new(
                    0,
                    1,
                    CaptureRecordKind::OperationBoundary,
                    SensitiveBytes::new(vec![1]),
                )],
            ),
        ] {
            let capture_ref = CaptureRef::from_bytes(
                [u8::try_from(records.len()).unwrap().saturating_add(10); 16],
            );
            let capture = PrivateCaptureV1::new(
                capture_ref,
                1,
                2,
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes([1; 4]),
                records,
                CaptureCompleteness::Complete,
                Vec::new(),
                0,
                SafeTerminalCategory::Failed,
            );
            assert_eq!(
                write_encrypted(
                    secure_new_file(&directory.path().join(name)).unwrap(),
                    &capture,
                    &passphrase,
                    CaptureLimits::default(),
                )
                .unwrap_err(),
                VaultFormatError::MissingRequiredEvidence
            );
        }
    }

    #[test]
    fn incomplete_capture_can_preserve_a_pre_http_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.partial");
        let capture_ref = CaptureRef::from_bytes([12; 16]);
        let capture = PrivateCaptureV1::new(
            capture_ref,
            1,
            2,
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([1; 4]),
            vec![CaptureRecordV1::new(
                0,
                1,
                CaptureRecordKind::OperationBoundary,
                SensitiveBytes::new(vec![1]),
            )],
            CaptureCompleteness::Incomplete,
            vec![IncompleteReason::MissingProviderExchange],
            0,
            SafeTerminalCategory::Failed,
        );
        let passphrase = CapturePassphrase::new("incomplete test passphrase".to_owned()).unwrap();
        write_encrypted(
            secure_new_file(&path).unwrap(),
            &capture,
            &passphrase,
            CaptureLimits::default(),
        )
        .unwrap();
        let reopened = read_encrypted(
            File::open(path).unwrap(),
            capture_ref,
            &passphrase,
            CaptureLimits::default(),
        )
        .unwrap();
        assert_eq!(reopened.completeness, CaptureCompleteness::Incomplete);
    }

    #[test]
    fn typed_record_limit_is_enforced_before_encryption() {
        let directory = tempfile::tempdir().unwrap();
        let capture_ref = CaptureRef::from_bytes([13; 16]);
        let capture = PrivateCaptureV1::new(
            capture_ref,
            1,
            2,
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([1; 4]),
            vec![CaptureRecordV1::new(
                0,
                1,
                CaptureRecordKind::HttpRequest,
                SensitiveBytes::new(vec![0; 128 * 1024 + 7]),
            )],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Failed,
        );
        let passphrase = CapturePassphrase::new("record limit passphrase".to_owned()).unwrap();
        assert_eq!(
            write_encrypted(
                secure_new_file(&directory.path().join("capture.partial")).unwrap(),
                &capture,
                &passphrase,
                boundary_limits(),
            )
            .unwrap_err(),
            VaultFormatError::RecordTooLarge
        );
    }

    #[test]
    fn authenticated_container_with_tampered_checksum_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.partial");
        let capture_ref = CaptureRef::from_bytes([14; 16]);
        let passphrase = CapturePassphrase::new("checksum test passphrase".to_owned()).unwrap();
        let encryptor = Encryptor::with_user_passphrase(passphrase.age_secret());
        let mut writer = encryptor
            .wrap_output(secure_new_file(&path).unwrap())
            .unwrap();
        write_plaintext_container(
            &mut writer,
            &fixture_capture(capture_ref),
            CaptureLimits::default(),
            Some([0; 32]),
            None,
        )
        .unwrap();
        writer.finish().unwrap().sync_all().unwrap();

        assert_eq!(
            read_encrypted(
                File::open(path).unwrap(),
                capture_ref,
                &passphrase,
                CaptureLimits::default(),
            )
            .unwrap_err(),
            VaultFormatError::ChecksumMismatch
        );
    }

    #[test]
    fn authenticated_unknown_schema_is_rejected_before_use() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.partial");
        let capture_ref = CaptureRef::from_bytes([15; 16]);
        let passphrase = CapturePassphrase::new("schema test passphrase".to_owned()).unwrap();
        let encryptor = Encryptor::with_user_passphrase(passphrase.age_secret());
        let mut writer = encryptor
            .wrap_output(secure_new_file(&path).unwrap())
            .unwrap();
        write_plaintext_container(
            &mut writer,
            &fixture_capture(capture_ref),
            CaptureLimits::default(),
            None,
            Some(super::CAPTURE_SCHEMA_VERSION.saturating_add(1)),
        )
        .unwrap();
        writer.finish().unwrap().sync_all().unwrap();

        assert_eq!(
            read_encrypted(
                File::open(path).unwrap(),
                capture_ref,
                &passphrase,
                CaptureLimits::default(),
            )
            .unwrap_err(),
            VaultFormatError::UnsupportedSchema
        );
    }
}
