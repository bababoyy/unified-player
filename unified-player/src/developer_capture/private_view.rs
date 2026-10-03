//! Explicit terminal-only viewer for encrypted Tier 2 provider evidence.
//!
//! The caller owns the sensitivity acknowledgement. This module authorizes and
//! rechecks the concrete terminal sink, then keeps payload decoding, credential
//! masking, and rendering inside the private subsystem so raw models never
//! enter ordinary UI state.

use std::io::{IsTerminal as _, Write};

use serde_json::Value;
use zeroize::{Zeroize as _, Zeroizing};

use super::{
    model::{
        CaptureCompleteness, CapturePurpose, CaptureRecordKind, EndpointRole, PrivateCaptureV1,
        ProviderClientKind, SafeTerminalCategory, TransportKind,
    },
    payload::{decode_fields, field, DecodedPrivatePayload, PrivatePayloadKind},
    writer::{DecryptedCaptureArtifact, PrivateManifestFacts},
    PRIVATE_PAYLOAD_SCHEMA_VERSION,
};

const MASKED: &str = "[masked credential or signed transport value]";
const MASKED_URL: &str = "[masked malformed or relative URL]";
const OPAQUE_BODY: &str = "[opaque non-JSON body withheld by the masked viewer]";
const MAX_VIEW_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_MASKED_VIEW_BYTES: usize = 16 * 1024 * 1024;

pub(super) struct PrivateTerminalAuthorization(());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PrivateViewSelection {
    Catalog,
    Record(u16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PrivateViewError {
    InvalidArtifact,
    RecordNotFound,
    OutputUnavailable,
    TerminalRequired,
}

impl std::fmt::Display for PrivateViewError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidArtifact => "private capture inspection could not validate the artifact",
            Self::RecordNotFound => "the requested private capture record was not found",
            Self::OutputUnavailable => "private capture inspection output is unavailable",
            Self::TerminalRequired => {
                "private capture inspection requires an interactive output terminal"
            }
        })
    }
}

impl std::error::Error for PrivateViewError {}

pub(super) fn authorize_private_terminal_output(
) -> Result<PrivateTerminalAuthorization, PrivateViewError> {
    authorize_private_terminal_output_with(std::io::stdout().is_terminal())
}

fn authorize_private_terminal_output_with(
    stdout_is_terminal: bool,
) -> Result<PrivateTerminalAuthorization, PrivateViewError> {
    if stdout_is_terminal {
        Ok(PrivateTerminalAuthorization(()))
    } else {
        Err(PrivateViewError::TerminalRequired)
    }
}

pub(super) fn write_masked_private_view_to_terminal(
    artifact: &DecryptedCaptureArtifact,
    selection: PrivateViewSelection,
    _authorization: PrivateTerminalAuthorization,
) -> Result<(), PrivateViewError> {
    if !std::io::stdout().is_terminal() {
        return Err(PrivateViewError::TerminalRequired);
    }
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    write_masked_private_view(artifact, selection, &mut output)?;
    output
        .flush()
        .map_err(|_| PrivateViewError::OutputUnavailable)
}

fn write_masked_private_view(
    artifact: &DecryptedCaptureArtifact,
    selection: PrivateViewSelection,
    output: &mut impl Write,
) -> Result<(), PrivateViewError> {
    let capture = artifact.capture();
    let mut rendered = BoundedPrivateOutput::new(MAX_MASKED_VIEW_BYTES)?;
    write_capture_header(capture, artifact.manifest(), &mut rendered)?;
    match selection {
        PrivateViewSelection::Catalog => {
            writeln!(
                rendered,
                "view=catalog\nhint=rerun with --record <sequence> to inspect one masked record"
            )
            .map_err(|_| PrivateViewError::OutputUnavailable)?;
            for record in capture.records() {
                let exchange = exchange_ordinal(capture.records(), record.exchange_ref());
                writeln!(
                    rendered,
                    "record={} exchange={} kind={} offset_ms={} endpoint={} client={} transport={} attempt={} payload_bytes={}",
                    record.sequence(),
                    exchange.map_or_else(|| "none".to_owned(), |value| value.to_string()),
                    record_kind(record.kind()),
                    record.monotonic_offset_ms(),
                    endpoint_role(record.endpoint_role()),
                    client_kind(record.client_kind()),
                    transport_kind(record.transport_kind()),
                    record.attempt(),
                    record.payload().len(),
                )
                .map_err(|_| PrivateViewError::OutputUnavailable)?;
            }
        }
        PrivateViewSelection::Record(sequence) => {
            let record = capture
                .records()
                .iter()
                .find(|record| record.sequence() == sequence)
                .ok_or(PrivateViewError::RecordNotFound)?;
            let exchange = exchange_ordinal(capture.records(), record.exchange_ref());
            writeln!(
                rendered,
                "view=record\nrecord={} exchange={} kind={} offset_ms={} endpoint={} client={} transport={} attempt={} payload_bytes={}",
                record.sequence(),
                exchange.map_or_else(|| "none".to_owned(), |value| value.to_string()),
                record_kind(record.kind()),
                record.monotonic_offset_ms(),
                endpoint_role(record.endpoint_role()),
                client_kind(record.client_kind()),
                transport_kind(record.transport_kind()),
                record.attempt(),
                record.payload().len(),
            )
            .map_err(|_| PrivateViewError::OutputUnavailable)?;
            write_record_payload(record.payload(), &mut rendered)?;
        }
    }
    if rendered.overflowed() {
        return Err(PrivateViewError::InvalidArtifact);
    }
    output
        .write_all(rendered.as_slice())
        .map_err(|_| PrivateViewError::OutputUnavailable)
}

struct BoundedPrivateOutput {
    bytes: Zeroizing<Vec<u8>>,
    limit: usize,
    overflowed: bool,
}

impl BoundedPrivateOutput {
    fn new(limit: usize) -> Result<Self, PrivateViewError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(limit)
            .map_err(|_| PrivateViewError::OutputUnavailable)?;
        Ok(Self {
            bytes: Zeroizing::new(bytes),
            limit,
            overflowed: false,
        })
    }

    fn overflowed(&self) -> bool {
        self.overflowed
    }

    fn as_slice(&self) -> &[u8] {
        self.bytes.as_slice()
    }
}

impl Write for BoundedPrivateOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.overflowed
            || self
                .bytes
                .len()
                .checked_add(bytes.len())
                .is_none_or(|next| next > self.limit)
        {
            self.overflowed = true;
            return Ok(bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn exchange_ordinal(
    records: &[super::model::CaptureRecordV1],
    target: Option<super::model::ExchangeRef>,
) -> Option<u16> {
    let target = target?;
    let mut seen = Vec::with_capacity(32);
    for record in records {
        let Some(reference) = record.exchange_ref() else {
            continue;
        };
        if !seen.contains(&reference) {
            seen.push(reference);
        }
        if reference == target {
            return u16::try_from(seen.len()).ok();
        }
    }
    None
}

fn write_capture_header(
    capture: &PrivateCaptureV1,
    manifest: &PrivateManifestFacts,
    output: &mut impl Write,
) -> Result<(), PrivateViewError> {
    writeln!(
        output,
        "unified-player private provider evidence\n\
         sensitivity=private\n\
         credentials=masked\n\
         clipboard=disabled\n\
         artifact_payload=bounded_exact_bytes\n\
         viewer_body=normalized_masked_json_or_opaque_withheld\n\
         capture_schema={}\n\
         consent_schema={}\n\
         payload_schema={}\n\
         application_version={}\n\
         source_revision={}\n\
         source_dirty={}\n\
         platform_os={}\n\
         platform_arch={}\n\
         reference={}\n\
         operation_reference={}\n\
         purpose={}\n\
         created_unix_ms={}\n\
         completed_unix_ms={}\n\
         completeness={}\n\
         terminal={}\n\
         records={}\n\
         plaintext_bytes={}\n\
         record_stream_checksum={}\n\
         dropped={}\n\
         credential_class_values_present={}\n\
         limits.arm_deadline_ms={}\n\
         limits.operation_deadline_ms={}\n\
         limits.record_capacity={}\n\
         limits.exchange_capacity={}\n\
         limits.player_request_bytes={}\n\
         limits.player_response_bytes={}\n\
         limits.transport_error_bytes={}\n\
         limits.plaintext_bytes={}\n\
         limits.encrypted_artifact_bytes={}\n\
         limits.retained_artifacts={}\n\
         limits.total_storage_bytes={}\n\
         limits.retention_ms={}\n\
         limits.writer_finalization_ms={}",
        manifest.schema_version,
        manifest.consent_version,
        PRIVATE_PAYLOAD_SCHEMA_VERSION,
        manifest.application_version,
        manifest.source_revision,
        manifest.source_dirty.as_str(),
        manifest.platform_os,
        manifest.platform_arch,
        capture.capture_ref().safe(),
        capture.operation_ref(),
        capture_purpose(capture.purpose()),
        capture.created_unix_ms,
        capture.completed_unix_ms,
        completeness(capture.completeness()),
        terminal(capture.terminal_category),
        manifest.record_count,
        manifest.plaintext_byte_count,
        if manifest.checksum_valid {
            "valid"
        } else {
            "invalid"
        },
        capture.dropped_records,
        yes_no(capture.credential_values_present()),
        manifest.limits.arm_deadline_ms,
        manifest.limits.operation_deadline_ms,
        manifest.limits.record_capacity,
        manifest.limits.exchange_capacity,
        manifest.limits.player_request_bytes,
        manifest.limits.player_response_bytes,
        manifest.limits.transport_error_bytes,
        manifest.limits.plaintext_bytes,
        manifest.limits.encrypted_artifact_bytes,
        manifest.limits.retained_artifacts,
        manifest.limits.total_storage_bytes,
        manifest.limits.retention_ms,
        manifest.limits.writer_finalization_ms,
    )
    .map_err(|_| PrivateViewError::OutputUnavailable)?;
    if !capture.incomplete_reasons.is_empty() {
        write!(output, "incomplete_reasons=").map_err(|_| PrivateViewError::OutputUnavailable)?;
        for (index, reason) in capture.incomplete_reasons.iter().enumerate() {
            if index != 0 {
                write!(output, ",").map_err(|_| PrivateViewError::OutputUnavailable)?;
            }
            write!(output, "{}", incomplete_reason(*reason))
                .map_err(|_| PrivateViewError::OutputUnavailable)?;
        }
        writeln!(output).map_err(|_| PrivateViewError::OutputUnavailable)?;
    }
    Ok(())
}

fn write_record_payload(
    payload: &super::model::SensitiveBytes,
    output: &mut impl Write,
) -> Result<(), PrivateViewError> {
    let Ok(decoded) = decode_fields(payload) else {
        writeln!(
            output,
            "payload=private binary operation data; structured field inspection unavailable"
        )
        .map_err(|_| PrivateViewError::OutputUnavailable)?;
        return Ok(());
    };
    writeln!(output, "payload_kind={}", payload_kind(decoded.kind()))
        .map_err(|_| PrivateViewError::OutputUnavailable)?;
    for (tag, value) in decoded.field_values() {
        write!(output, "{}=", field_name(tag)).map_err(|_| PrivateViewError::OutputUnavailable)?;
        match tag {
            field::HEADERS => write_headers(value, output)?,
            field::BODY => write_body(value, output)?,
            field::FORMAT_FACTS => write_format_facts(value, output)?,
            field::URL => write_masked_url(value, output)?,
            _ if numeric_field(tag) => {
                let number = decoded
                    .field_u64(tag)
                    .ok_or(PrivateViewError::InvalidArtifact)?;
                write!(output, "{number}").map_err(|_| PrivateViewError::OutputUnavailable)?;
            }
            _ if boolean_field(tag) => {
                let flag = decoded
                    .field_bool(tag)
                    .ok_or(PrivateViewError::InvalidArtifact)?;
                write!(output, "{flag}").map_err(|_| PrivateViewError::OutputUnavailable)?;
            }
            _ if text_field(tag) => write_escaped(value, output)?,
            _ => {
                write!(output, "[private binary value; bytes={}]", value.len())
                    .map_err(|_| PrivateViewError::OutputUnavailable)?;
            }
        }
        writeln!(output).map_err(|_| PrivateViewError::OutputUnavailable)?;
    }
    Ok(())
}

fn write_headers(encoded: &[u8], output: &mut impl Write) -> Result<(), PrivateViewError> {
    let mut cursor = 0_usize;
    let count = usize::from(take_u16(encoded, &mut cursor)?);
    writeln!(output, "{count} header(s)").map_err(|_| PrivateViewError::OutputUnavailable)?;
    for _ in 0..count {
        let name_len = usize::from(take_u16(encoded, &mut cursor)?);
        let name = take(encoded, &mut cursor, name_len)?;
        let value_len = usize::try_from(take_u32(encoded, &mut cursor)?)
            .map_err(|_| PrivateViewError::InvalidArtifact)?;
        let value = take(encoded, &mut cursor, value_len)?;
        write!(output, "  ").map_err(|_| PrivateViewError::OutputUnavailable)?;
        write_escaped(name, output)?;
        write!(output, ": ").map_err(|_| PrivateViewError::OutputUnavailable)?;
        if sensitive_name(name) {
            write!(output, "{MASKED}").map_err(|_| PrivateViewError::OutputUnavailable)?;
        } else if url_header_name(name) && !looks_like_http_url(value) {
            write!(output, "{MASKED_URL}").map_err(|_| PrivateViewError::OutputUnavailable)?;
        } else if looks_like_http_url(value) {
            write_masked_url(value, output)?;
        } else {
            write_escaped(value, output)?;
        }
        writeln!(output).map_err(|_| PrivateViewError::OutputUnavailable)?;
    }
    if cursor != encoded.len() {
        return Err(PrivateViewError::InvalidArtifact);
    }
    Ok(())
}

fn write_body(body: &[u8], output: &mut impl Write) -> Result<(), PrivateViewError> {
    if body.len() > MAX_VIEW_BODY_BYTES {
        return Err(PrivateViewError::InvalidArtifact);
    }
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        write!(
            output,
            "opaque_withheld; {OPAQUE_BODY}; bytes={}",
            body.len()
        )
        .map_err(|_| PrivateViewError::OutputUnavailable)?;
        return Ok(());
    };
    let mut sensitive = SensitiveJson(value);
    mask_json_value(&mut sensitive.0, None)?;
    writeln!(output, "normalized_masked_json").map_err(|_| PrivateViewError::OutputUnavailable)?;
    let mut terminal_safe = TerminalSafeWriter(output);
    serde_json::to_writer_pretty(&mut terminal_safe, &sensitive.0)
        .map_err(|_| PrivateViewError::InvalidArtifact)
}

struct SensitiveJson(Value);

impl Drop for SensitiveJson {
    fn drop(&mut self) {
        zeroize_json_value(&mut self.0);
    }
}

fn mask_json_value(value: &mut Value, key: Option<&str>) -> Result<(), PrivateViewError> {
    if key.is_some_and(sensitive_name_str) {
        replace_json_with_mask(value);
        return Ok(());
    }
    if key.is_some_and(cipher_field_name) {
        replace_json_with_mask(value);
        return Ok(());
    }
    if key.is_some_and(url_field_name) {
        if let Value::String(text) = value {
            let mut private = Zeroizing::new(std::mem::take(text));
            let masked = masked_url_bytes(private.as_bytes());
            *text = String::from_utf8(masked.to_vec())
                .map_err(|_| PrivateViewError::InvalidArtifact)?;
            private.zeroize();
            return Ok(());
        }
    }
    if let Value::String(text) = value {
        if looks_like_http_url(text.as_bytes()) {
            let mut private = Zeroizing::new(std::mem::take(text));
            let masked = masked_url_bytes(private.as_bytes());
            *text = String::from_utf8(masked.to_vec())
                .map_err(|_| PrivateViewError::InvalidArtifact)?;
            private.zeroize();
            return Ok(());
        }
    }
    match value {
        Value::Array(values) => {
            for value in values {
                mask_json_value(value, None)?;
            }
        }
        Value::Object(values) => {
            let named_sensitive_value = values.iter().any(|(name, value)| {
                (name.eq_ignore_ascii_case("name")
                    || name.eq_ignore_ascii_case("header")
                    || name.eq_ignore_ascii_case("parameter"))
                    && value.as_str().is_some_and(sensitive_name_str)
            });
            for (name, value) in values {
                if named_sensitive_value && name.eq_ignore_ascii_case("value") {
                    replace_json_with_mask(value);
                } else {
                    mask_json_value(value, Some(name))?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn replace_json_with_mask(value: &mut Value) {
    zeroize_json_value(value);
    *value = Value::String(MASKED.to_owned());
}

fn zeroize_json_value(value: &mut Value) {
    match value {
        Value::String(text) => text.zeroize(),
        Value::Array(values) => {
            for value in &mut *values {
                zeroize_json_value(value);
            }
            values.clear();
        }
        Value::Object(values) => {
            let values = std::mem::take(values);
            for (mut name, mut value) in values {
                name.zeroize();
                zeroize_json_value(&mut value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn write_masked_url(value: &[u8], output: &mut impl Write) -> Result<(), PrivateViewError> {
    let masked = masked_url_bytes(value);
    write_escaped(&masked, output)
}

fn masked_url_bytes(value: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut masked = Zeroizing::new(Vec::with_capacity(value.len()));
    let authority_start = if value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"http://"))
    {
        7
    } else if value
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"https://"))
    {
        8
    } else {
        masked.extend_from_slice(MASKED_URL.as_bytes());
        return masked;
    };
    let authority_end = value[authority_start..]
        .iter()
        .position(|byte| matches!(byte, b'/' | b'?' | b'#'))
        .map_or(value.len(), |offset| authority_start + offset);
    if authority_end == authority_start {
        masked.extend_from_slice(MASKED_URL.as_bytes());
        return masked;
    }

    masked.extend_from_slice(&value[..authority_start]);
    let authority = &value[authority_start..authority_end];
    if let Some(userinfo_end) = authority.iter().rposition(|byte| *byte == b'@') {
        masked.extend_from_slice(b"%5Bmasked-userinfo%5D@");
        masked.extend_from_slice(&authority[userinfo_end + 1..]);
    } else {
        masked.extend_from_slice(authority);
    }

    let question = value[authority_end..]
        .iter()
        .position(|byte| *byte == b'?')
        .map(|offset| authority_end + offset);
    let fragment = value[authority_end..]
        .iter()
        .position(|byte| *byte == b'#')
        .map(|offset| authority_end + offset);
    let path_end = match (question, fragment) {
        (Some(question), Some(fragment)) => question.min(fragment),
        (Some(question), None) => question,
        (None, Some(fragment)) => fragment,
        (None, None) => value.len(),
    };
    masked.extend_from_slice(&value[authority_end..path_end]);
    let Some(question) =
        question.filter(|question| fragment.is_none_or(|fragment| *question < fragment))
    else {
        if fragment.is_some() {
            masked.extend_from_slice(b"#%5Bmasked%5D");
        }
        return masked;
    };

    masked.push(b'?');
    let query_end = fragment.unwrap_or(value.len());
    for (index, pair) in value[question + 1..query_end]
        .split(|byte| *byte == b'&')
        .enumerate()
    {
        if index != 0 {
            masked.push(b'&');
        }
        let split = pair.iter().position(|byte| *byte == b'=');
        if let Some(position) = split {
            masked.extend_from_slice(&pair[..position]);
            masked.push(b'=');
            masked.extend_from_slice(b"%5Bmasked%5D");
        } else if !pair.is_empty() {
            masked.extend_from_slice(b"unnamed=%5Bmasked%5D");
        }
    }
    if fragment.is_some() {
        masked.extend_from_slice(b"#%5Bmasked%5D");
    }
    masked
}

fn write_format_facts(bytes: &[u8], output: &mut impl Write) -> Result<(), PrivateViewError> {
    let mut cursor = 0_usize;
    let mut index = 0_usize;
    writeln!(output).map_err(|_| PrivateViewError::OutputUnavailable)?;
    while cursor < bytes.len() {
        let itag = take_u64(bytes, &mut cursor)?;
        let bitrate = take_u64(bytes, &mut cursor)?;
        let direct = *take(bytes, &mut cursor, 1)?
            .first()
            .ok_or(PrivateViewError::InvalidArtifact)?;
        let cipher = *take(bytes, &mut cursor, 1)?
            .first()
            .ok_or(PrivateViewError::InvalidArtifact)?;
        let mime_len = usize::from(take_u16(bytes, &mut cursor)?);
        let mime = take(bytes, &mut cursor, mime_len)?;
        let content_length_len = usize::from(take_u16(bytes, &mut cursor)?);
        let content_length = take(bytes, &mut cursor, content_length_len)?;
        let duration_len = usize::from(take_u16(bytes, &mut cursor)?);
        let duration = take(bytes, &mut cursor, duration_len)?;
        write!(
            output,
            "  format={index} itag={itag} bitrate={bitrate} direct={} cipher={} mime=",
            direct != 0,
            cipher != 0
        )
        .map_err(|_| PrivateViewError::OutputUnavailable)?;
        write_escaped(mime, output)?;
        write!(output, " content_length=").map_err(|_| PrivateViewError::OutputUnavailable)?;
        write_escaped(content_length, output)?;
        write!(output, " duration_ms=").map_err(|_| PrivateViewError::OutputUnavailable)?;
        write_escaped(duration, output)?;
        writeln!(output).map_err(|_| PrivateViewError::OutputUnavailable)?;
        index = index.saturating_add(1);
    }
    Ok(())
}

fn write_escaped(value: &[u8], output: &mut impl Write) -> Result<(), PrivateViewError> {
    write!(output, "\"").map_err(|_| PrivateViewError::OutputUnavailable)?;
    for byte in value {
        match byte {
            b'"' => write!(output, "\\\""),
            b'\\' => write!(output, "\\\\"),
            b'\n' => write!(output, "\\n"),
            b'\r' => write!(output, "\\r"),
            b'\t' => write!(output, "\\t"),
            0x20..=0x7e => output.write_all(&[*byte]),
            _ => write!(output, "\\x{byte:02x}"),
        }
        .map_err(|_| PrivateViewError::OutputUnavailable)?;
    }
    write!(output, "\"").map_err(|_| PrivateViewError::OutputUnavailable)
}

struct TerminalSafeWriter<'a, W>(&'a mut W);

impl<W: Write> Write for TerminalSafeWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut start = 0_usize;
        for (index, byte) in bytes.iter().copied().enumerate() {
            if byte == b'\n' || (0x20..=0x7e).contains(&byte) {
                continue;
            }
            self.0.write_all(&bytes[start..index])?;
            write!(self.0, "\\x{byte:02x}")?;
            start = index.saturating_add(1);
        }
        self.0.write_all(&bytes[start..])?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

fn take<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    length: usize,
) -> Result<&'a [u8], PrivateViewError> {
    let end = cursor
        .checked_add(length)
        .ok_or(PrivateViewError::InvalidArtifact)?;
    let value = bytes
        .get(*cursor..end)
        .ok_or(PrivateViewError::InvalidArtifact)?;
    *cursor = end;
    Ok(value)
}

fn take_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16, PrivateViewError> {
    let bytes = take(bytes, cursor, 2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn take_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, PrivateViewError> {
    let bytes = take(bytes, cursor, 4)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn take_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, PrivateViewError> {
    let bytes = take(bytes, cursor, 8)?;
    Ok(u64::from_be_bytes(
        bytes
            .try_into()
            .map_err(|_| PrivateViewError::InvalidArtifact)?,
    ))
}

fn sensitive_name(name: &[u8]) -> bool {
    std::str::from_utf8(name).is_ok_and(sensitive_name_str)
}

fn looks_like_http_url(value: &[u8]) -> bool {
    value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"http://"))
        || value
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"https://"))
}

fn url_header_name(name: &[u8]) -> bool {
    [
        b"location".as_slice(),
        b"content-location".as_slice(),
        b"referer".as_slice(),
        b"referrer".as_slice(),
        b"link".as_slice(),
    ]
    .iter()
    .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn sensitive_name_str(name: &str) -> bool {
    let name = Zeroizing::new(name.to_ascii_lowercase());
    matches!(name.as_str(), "s" | "sig" | "lsig" | "n" | "spc" | "key")
        || [
            "auth",
            "cookie",
            "credential",
            "secret",
            "password",
            "passwd",
            "signature",
            "token",
            "visitor",
            "tracking",
            "session",
            "apikey",
            "api_key",
            "api-key",
            "proof",
            "csrf",
            "xsrf",
            "nonce",
        ]
        .iter()
        .any(|part| name.contains(part))
}

fn url_field_name(name: &str) -> bool {
    let name = Zeroizing::new(name.to_ascii_lowercase());
    name.ends_with("url")
        || name.ends_with("uri")
        || matches!(
            name.as_str(),
            "href" | "src" | "location" | "referer" | "referrer" | "link"
        )
}

fn cipher_field_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("cipher") || name.eq_ignore_ascii_case("signatureCipher")
}

const fn numeric_field(tag: u16) -> bool {
    matches!(
        tag,
        field::STATUS
            | field::ELAPSED_MS
            | field::RETURNED_FORMATS
            | field::SUPPORTED_FORMATS
            | field::DIRECT_FORMATS
            | field::CIPHER_FORMATS
            | field::SELECTED_ITAG
            | field::BODY_LENGTH
            | field::RETAINED_BODY_LENGTH
            | field::RANGE_START
            | field::RANGE_END
            | field::RESPONSE_LENGTH
            | field::REQUEST_ORDINAL
            | field::DROPPED_COUNT
            | field::REDIRECT_COUNT
    )
}

const fn boolean_field(tag: u16) -> bool {
    matches!(
        tag,
        field::BODY_COMPLETE
            | field::PROOF_TOKEN_PRESENT
            | field::STREAMING_DATA_PRESENT
            | field::FALLBACK_ELIGIBLE
            | field::FALLBACK_ATTEMPTED
            | field::ERROR_IS_TIMEOUT
            | field::ERROR_IS_CONNECT
            | field::ERROR_IS_REQUEST
            | field::ERROR_IS_BODY
            | field::ERROR_IS_DECODE
            | field::REDIRECTED
            | field::CONTENT_RANGE_PRESENT
            | field::FROM_DISK_CACHE
            | field::FROM_SERVICE_WORKER
    )
}

const fn text_field(tag: u16) -> bool {
    matches!(
        tag,
        field::METHOD
            | field::STAGE
            | field::CATEGORY
            | field::AUTH_KIND
            | field::CLIENT_KIND
            | field::CLIENT_VERSION
            | field::VERSION_SOURCE
            | field::USER_AGENT_PROFILE
            | field::PLAYABILITY_STATUS
            | field::PLAYABILITY_REASON
            | field::QUALITY
            | field::OUTCOME
    )
}

const fn field_name(tag: u16) -> &'static str {
    match tag {
        field::METHOD => "method",
        field::URL => "url",
        field::HEADERS => "headers",
        field::BODY => "body",
        field::STATUS => "status",
        field::ELAPSED_MS => "elapsed_ms",
        field::BODY_COMPLETE => "body_complete",
        field::STAGE => "stage",
        field::CATEGORY => "category",
        field::AUTH_KIND => "auth_kind",
        field::CLIENT_KIND => "client_kind",
        field::CLIENT_VERSION => "client_version",
        field::VERSION_SOURCE => "version_source",
        field::USER_AGENT_PROFILE => "user_agent_profile",
        field::PROOF_TOKEN_PRESENT => "proof_token_present",
        field::PLAYABILITY_STATUS => "playability_status",
        field::PLAYABILITY_REASON => "playability_reason",
        field::STREAMING_DATA_PRESENT => "streaming_data_present",
        field::RETURNED_FORMATS => "returned_formats",
        field::SUPPORTED_FORMATS => "supported_formats",
        field::DIRECT_FORMATS => "direct_formats",
        field::CIPHER_FORMATS => "cipher_formats",
        field::FORMAT_FACTS => "format_facts",
        field::QUALITY => "quality",
        field::SELECTED_ITAG => "selected_itag",
        field::FALLBACK_ELIGIBLE => "fallback_eligible",
        field::FALLBACK_ATTEMPTED => "fallback_attempted",
        field::OUTCOME => "outcome",
        field::ERROR_IS_TIMEOUT => "error_is_timeout",
        field::ERROR_IS_CONNECT => "error_is_connect",
        field::ERROR_IS_REQUEST => "error_is_request",
        field::ERROR_IS_BODY => "error_is_body",
        field::ERROR_IS_DECODE => "error_is_decode",
        field::REDIRECTED => "redirected",
        field::BODY_LENGTH => "body_length",
        field::RETAINED_BODY_LENGTH => "retained_body_length",
        field::RANGE_START => "range_start",
        field::RANGE_END => "range_end",
        field::CONTENT_RANGE_PRESENT => "content_range_present",
        field::RESPONSE_LENGTH => "response_length",
        field::REQUEST_ORDINAL => "request_ordinal",
        field::DROPPED_COUNT => "dropped_count",
        field::FROM_DISK_CACHE => "from_disk_cache",
        field::FROM_SERVICE_WORKER => "from_service_worker",
        field::REDIRECT_COUNT => "redirect_count",
        _ => "unknown_private_field",
    }
}

const fn payload_kind(kind: PrivatePayloadKind) -> &'static str {
    match kind {
        PrivatePayloadKind::Operation => "operation",
        PrivatePayloadKind::AuthSelection => "auth_selection",
        PrivatePayloadKind::HttpRequest => "http_request",
        PrivatePayloadKind::HttpResponse => "http_response",
        PrivatePayloadKind::NetworkFailure => "network_failure",
        PrivatePayloadKind::PlayerParse => "player_parse",
        PrivatePayloadKind::FormatInventory => "format_inventory",
        PrivatePayloadKind::SelectionDecision => "selection_decision",
        PrivatePayloadKind::BrowserExchange => "browser_exchange",
        PrivatePayloadKind::MediaProbe => "media_probe",
        PrivatePayloadKind::DecodeStage => "decode_stage",
        PrivatePayloadKind::TerminalOutcome => "terminal_outcome",
    }
}

const fn capture_purpose(value: CapturePurpose) -> &'static str {
    match value {
        CapturePurpose::InteractivePlayback => "interactive_playback",
        CapturePurpose::Prefetch => "prefetch",
        CapturePurpose::Resume => "resume",
        CapturePurpose::Probe => "probe",
        CapturePurpose::Replay => "replay",
        CapturePurpose::Comparison => "comparison",
    }
}

const fn completeness(value: CaptureCompleteness) -> &'static str {
    match value {
        CaptureCompleteness::Pending => "pending",
        CaptureCompleteness::Complete => "complete",
        CaptureCompleteness::Incomplete => "incomplete",
    }
}

const fn terminal(value: SafeTerminalCategory) -> &'static str {
    match value {
        SafeTerminalCategory::Success => "success",
        SafeTerminalCategory::Failed => "failed",
        SafeTerminalCategory::Cancelled => "cancelled",
        SafeTerminalCategory::Superseded => "superseded",
        SafeTerminalCategory::TimedOut => "timed_out",
        SafeTerminalCategory::Panicked => "panicked",
    }
}

const fn incomplete_reason(value: super::model::IncompleteReason) -> &'static str {
    use super::model::IncompleteReason as R;
    match value {
        R::RecordCapacity => "record_capacity",
        R::ExchangeCapacity => "exchange_capacity",
        R::RecordSize => "record_size",
        R::QueueCapacity => "queue_capacity",
        R::PlaintextBudget => "plaintext_budget",
        R::OperationDeadline => "operation_deadline",
        R::FinalizationDeadline => "finalization_deadline",
        R::MissingProviderExchange => "missing_provider_exchange",
        R::WriterUnavailable => "writer_unavailable",
        R::Abandoned => "abandoned",
        R::Shutdown => "shutdown",
        R::Persistence => "persistence",
        R::Truncated => "truncated",
    }
}

const fn endpoint_role(value: EndpointRole) -> &'static str {
    match value {
        EndpointRole::Unknown => "unknown",
        EndpointRole::Operation => "operation",
        EndpointRole::PlayerApi => "player_api",
        EndpointRole::BrowserPlayer => "browser_player",
        EndpointRole::Media => "media",
        EndpointRole::BrowserMedia => "browser_media",
        EndpointRole::Decoder => "decoder",
    }
}

const fn client_kind(value: ProviderClientKind) -> &'static str {
    match value {
        ProviderClientKind::Unknown => "unknown",
        ProviderClientKind::Web => "web",
        ProviderClientKind::WebRemix => "web_remix",
        ProviderClientKind::Android => "android",
        ProviderClientKind::Ios => "ios",
        ProviderClientKind::TvHtml5 => "tv_html5",
        ProviderClientKind::Spotify => "spotify",
    }
}

const fn transport_kind(value: TransportKind) -> &'static str {
    match value {
        TransportKind::Unknown => "unknown",
        TransportKind::NativeHttp => "native_http",
        TransportKind::BrowserCdp => "browser_cdp",
        TransportKind::MediaRange => "media_range",
        TransportKind::OfflineReplay => "offline_replay",
        TransportKind::FreshReplay => "fresh_replay",
    }
}

const fn record_kind(value: CaptureRecordKind) -> &'static str {
    match value {
        CaptureRecordKind::OperationBoundary => "operation_boundary",
        CaptureRecordKind::AuthSelection => "auth_selection",
        CaptureRecordKind::HttpRequest => "http_request",
        CaptureRecordKind::HttpResponse => "http_response",
        CaptureRecordKind::NetworkFailure => "network_failure",
        CaptureRecordKind::PlayerParse => "player_parse",
        CaptureRecordKind::FormatInventory => "format_inventory",
        CaptureRecordKind::SelectionDecision => "selection_decision",
        CaptureRecordKind::BrowserExchange => "browser_exchange",
        CaptureRecordKind::MediaProbe => "media_probe",
        CaptureRecordKind::DecodeStage => "decode_stage",
        CaptureRecordKind::TerminalOutcome => "terminal_outcome",
        CaptureRecordKind::SyntheticFixture => "synthetic_fixture",
    }
}

const fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use reqwest::{
        header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_LOCATION, COOKIE, LOCATION},
        StatusCode, Url,
    };

    use super::*;
    use crate::developer_capture::{
        encode_private_http_request, encode_private_http_response, CaptureLimits,
        CapturePassphrase, CaptureRecordV1, CaptureRef, CaptureStore, ExchangeRef,
        SafeOperationRef,
    };

    fn fixture_capture() -> PrivateCaptureV1 {
        let created_unix_ms = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(u64::MAX.saturating_sub(1));
        let mut request_headers = HeaderMap::new();
        request_headers.insert(COOKIE, HeaderValue::from_static("private-cookie-canary"));
        request_headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer private-bearer-canary"),
        );
        request_headers.insert(
            "x-youtube-client-version",
            HeaderValue::from_static("1.20260730"),
        );
        request_headers.insert(
            "x-goog-api-key",
            HeaderValue::from_static("private-api-key-canary"),
        );
        let request = reqwest::Client::new()
            .post("https://music.youtube.com/youtubei/v1/player?key=private-key-canary&prettyPrint=false")
            .headers(request_headers)
            .body(
                br#"{"videoId":"visible-private-video-id","poToken":"private-proof-canary","context":{"client":{"clientVersion":"1.20260730"}}}"#
                    .to_vec(),
            )
            .build()
            .unwrap();
        let request_payload = encode_private_http_request(&request).unwrap();

        let response_body = br#"{"playabilityStatus":{"status":"ERROR","reason":"developer-visible-provider-reason"},"streamingData":{"adaptiveFormats":[{"itag":251,"url":"https://media.invalid/path?id=visible-id&sig=private-signature-canary","cipher":"s=private-cipher-canary&sp=sig&url=https%3A%2F%2Fmedia.invalid%2Fprivate"}]},"credentialEnvelope":{"name":"authorization","value":"private-generic-credential-canary"},"terminalText":"\u009b\u202e\u001b\r"}"#;
        let mut response_headers = HeaderMap::new();
        response_headers.insert(
            LOCATION,
            HeaderValue::from_static(
                "https://private-user:private-pass@media.invalid/redirect?bare&id=visible-header-id&sig=private-header-signature#private-fragment",
            ),
        );
        response_headers.insert(
            CONTENT_LOCATION,
            HeaderValue::from_static("/private-relative-location"),
        );
        let response_payload = encode_private_http_response(
            StatusCode::OK,
            &Url::parse("https://music.youtube.com/youtubei/v1/player?key=private-key-canary")
                .unwrap(),
            &response_headers,
            response_body,
            std::time::Duration::from_millis(17),
            true,
        )
        .unwrap();

        let mut capture = PrivateCaptureV1::new(
            CaptureRef::from_bytes([0x44; 16]),
            created_unix_ms,
            created_unix_ms.saturating_add(1),
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([0x44; 4]),
            vec![
                CaptureRecordV1::with_context(
                    0,
                    2,
                    Some(ExchangeRef::from_bytes([9; 8])),
                    EndpointRole::PlayerApi,
                    ProviderClientKind::TvHtml5,
                    TransportKind::NativeHttp,
                    1,
                    CaptureRecordKind::HttpRequest,
                    request_payload,
                ),
                CaptureRecordV1::with_context(
                    1,
                    19,
                    Some(ExchangeRef::from_bytes([9; 8])),
                    EndpointRole::PlayerApi,
                    ProviderClientKind::TvHtml5,
                    TransportKind::NativeHttp,
                    1,
                    CaptureRecordKind::HttpResponse,
                    response_payload,
                ),
            ],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Failed,
        );
        capture.mark_credential_values_present();
        capture
    }

    fn artifact_from_capture(capture: PrivateCaptureV1) -> DecryptedCaptureArtifact {
        let record_count = u16::try_from(capture.records().len()).unwrap();
        let plaintext_byte_count = capture
            .records()
            .iter()
            .map(|record| u64::try_from(record.payload().len()).unwrap())
            .sum();
        DecryptedCaptureArtifact::from_test_parts(
            capture,
            PrivateManifestFacts {
                schema_version: 1,
                consent_version: 1,
                application_version: "0.24.0".to_owned(),
                source_revision: "0123456789abcdef".to_owned(),
                source_dirty: super::super::writer::PrivateBuildDirty::False,
                platform_os: "windows".to_owned(),
                platform_arch: "x86_64".to_owned(),
                record_count,
                plaintext_byte_count,
                checksum_valid: true,
                limits: super::super::writer::PrivateManifestLimits {
                    arm_deadline_ms: 60_000,
                    operation_deadline_ms: 60_000,
                    record_capacity: 256,
                    exchange_capacity: 32,
                    player_request_bytes: 2 * 1024 * 1024,
                    player_response_bytes: 2 * 1024 * 1024,
                    transport_error_bytes: 64 * 1024,
                    plaintext_bytes: 16 * 1024 * 1024,
                    encrypted_artifact_bytes: 20 * 1024 * 1024,
                    retained_artifacts: 5,
                    total_storage_bytes: 100 * 1024 * 1024,
                    retention_ms: 24 * 60 * 60 * 1000,
                    writer_finalization_ms: 5_000,
                },
            },
        )
    }

    #[test]
    fn catalog_is_bounded_metadata_without_private_payload_values() {
        let capture = artifact_from_capture(fixture_capture());
        let mut output = Vec::new();
        write_masked_private_view(&capture, PrivateViewSelection::Catalog, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("record=0 exchange=1 kind=http_request"));
        assert!(output.contains("record=1 exchange=1 kind=http_response"));
        for forbidden in [
            "private-cookie-canary",
            "private-proof-canary",
            "visible-private-video-id",
            "developer-visible-provider-reason",
        ] {
            assert!(!output.contains(forbidden));
        }
    }

    #[test]
    fn encrypted_vault_roundtrip_proves_manifest_masking_and_tamper_rejection() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("private-vault");
        let limits = CaptureLimits::default();
        let (store, _) = CaptureStore::open(&root, limits).unwrap();
        let capture = fixture_capture();
        let safe_ref = capture.capture_ref().safe();
        let passphrase =
            CapturePassphrase::new("private viewer integration passphrase".to_owned()).unwrap();
        store.write(&capture, &passphrase).unwrap();

        let artifact = store
            .read_private_artifact_by_safe_ref(safe_ref, &passphrase)
            .unwrap();
        assert_eq!(artifact.manifest().record_count, 2);
        assert!(artifact.manifest().checksum_valid);
        assert_eq!(
            artifact.manifest().limits.player_response_bytes,
            limits.player_response_bytes
        );
        let mut catalog = Vec::new();
        write_masked_private_view(&artifact, PrivateViewSelection::Catalog, &mut catalog).unwrap();
        let catalog = String::from_utf8(catalog).unwrap();
        assert!(catalog.contains("application_version="));
        assert!(catalog.contains("source_revision="));
        assert!(catalog.contains("operation_reference="));
        assert!(catalog.contains("record_stream_checksum=valid"));
        assert!(catalog.contains("record=0 exchange=1 kind=http_request"));

        let mut response = Vec::new();
        write_masked_private_view(&artifact, PrivateViewSelection::Record(1), &mut response)
            .unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.contains("body=normalized_masked_json"));
        assert!(response.contains("developer-visible-provider-reason"));
        assert!(!response.contains("private-signature-canary"));
        drop(artifact);

        let wrong =
            CapturePassphrase::new("different private viewer passphrase".to_owned()).unwrap();
        assert!(store
            .read_private_artifact_by_safe_ref(safe_ref, &wrong)
            .is_err());

        let artifact_path = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().and_then(std::ffi::OsStr::to_str) == Some("age"))
            .unwrap();
        let mut encrypted = Zeroizing::new(std::fs::read(&artifact_path).unwrap());
        assert!(encrypted.len() > 64);
        let tamper_index = encrypted.len() / 2;
        encrypted[tamper_index] ^= 0x01;
        std::fs::write(&artifact_path, encrypted.as_slice()).unwrap();
        assert!(store
            .read_private_artifact_by_safe_ref(safe_ref, &passphrase)
            .is_err());
    }

    #[test]
    fn record_view_preserves_provider_evidence_but_masks_credentials_and_signed_values() {
        let capture = artifact_from_capture(fixture_capture());
        let mut request = Vec::new();
        write_masked_private_view(&capture, PrivateViewSelection::Record(0), &mut request).unwrap();
        let request = String::from_utf8(request).unwrap();
        assert!(request.contains("visible-private-video-id"));
        assert!(request.contains("1.20260730"));
        assert!(request.contains(MASKED));
        for forbidden in [
            "private-cookie-canary",
            "private-bearer-canary",
            "private-proof-canary",
            "private-key-canary",
            "private-api-key-canary",
        ] {
            assert!(!request.contains(forbidden));
        }

        let mut response = Vec::new();
        write_masked_private_view(&capture, PrivateViewSelection::Record(1), &mut response)
            .unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.contains("developer-visible-provider-reason"));
        assert!(response.contains("https://media.invalid/path?"));
        assert!(response.contains("media.invalid/redirect?"));
        assert!(response.contains("\\xc2\\x9b"));
        assert!(response.contains("\\xe2\\x80\\xae"));
        assert!(!response.as_bytes().contains(&0x1b));
        assert!(!response.as_bytes().contains(&b'\r'));
        assert!(!contains_subslice(response.as_bytes(), &[0xe2, 0x80, 0xae],));
        for forbidden in [
            "visible-id",
            "visible-header-id",
            "private-signature-canary",
            "private-header-signature",
            "private-user",
            "private-pass",
            "private-fragment",
            "private-relative-location",
            "private-cipher-canary",
            "private-generic-credential-canary",
        ] {
            assert!(!response.contains(forbidden));
        }
    }

    #[test]
    fn url_masking_covers_userinfo_fragments_bare_queries_and_relative_values() {
        let masked = masked_url_bytes(
            b"https://user:password@example.invalid/path?bare&id=private#fragment",
        );
        assert_eq!(
            masked.as_slice(),
            b"https://%5Bmasked-userinfo%5D@example.invalid/path?unnamed=%5Bmasked%5D&id=%5Bmasked%5D#%5Bmasked%5D"
        );
        let bare_secret = masked_url_bytes(b"https://example.invalid/path?private-bearer-token");
        assert!(!contains_subslice(&bare_secret, b"private-bearer-token",));
        assert_eq!(
            masked_url_bytes(b"/relative/private").as_slice(),
            MASKED_URL.as_bytes()
        );
    }

    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|candidate| candidate == needle)
    }

    #[test]
    fn missing_record_and_opaque_body_fail_or_hide_safely() {
        let capture = artifact_from_capture(fixture_capture());
        assert_eq!(
            write_masked_private_view(&capture, PrivateViewSelection::Record(99), &mut Vec::new()),
            Err(PrivateViewError::RecordNotFound)
        );

        let payload = super::super::payload::encode_fields(
            PrivatePayloadKind::HttpResponse,
            &[super::super::payload::PrivateField::bytes(
                field::BODY,
                b"private opaque response",
            )],
        )
        .unwrap();
        let capture = artifact_from_capture(PrivateCaptureV1::new(
            super::super::model::CaptureRef::from_bytes([1; 16]),
            1,
            2,
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([1; 4]),
            vec![CaptureRecordV1::new(
                0,
                0,
                CaptureRecordKind::HttpResponse,
                payload,
            )],
            CaptureCompleteness::Incomplete,
            vec![super::super::model::IncompleteReason::Truncated],
            0,
            SafeTerminalCategory::Failed,
        ));
        let mut output = Vec::new();
        write_masked_private_view(&capture, PrivateViewSelection::Record(0), &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("body=opaque_withheld"));
        assert!(output.contains(OPAQUE_BODY));
        assert!(!output.contains("private opaque response"));
    }

    #[test]
    fn private_output_buffer_fails_closed_before_exceeding_its_allocation() {
        let mut output = BoundedPrivateOutput::new(8).unwrap();
        output.write_all(b"12345678").unwrap();
        assert_eq!(output.as_slice(), b"12345678");
        output.write_all(b"9").unwrap();
        assert!(output.overflowed());
        assert_eq!(output.as_slice(), b"12345678");
    }

    #[test]
    fn private_subsystem_rejects_non_terminal_output_authorization() {
        assert!(authorize_private_terminal_output_with(true).is_ok());
        assert!(matches!(
            authorize_private_terminal_output_with(false),
            Err(PrivateViewError::TerminalRequired)
        ));
    }
}
