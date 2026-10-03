use std::time::Duration;

use reqwest::{header::HeaderMap, Request, StatusCode, Url};
use zeroize::Zeroizing;

use super::model::SensitiveBytes;

const PAYLOAD_MAGIC: &[u8; 8] = b"SPPEV1\0\0";
pub(crate) const PRIVATE_PAYLOAD_SCHEMA_VERSION: u16 = 1;
const MAX_FIELDS: usize = 128;
pub(super) const MAX_HEADER_COUNT: usize = 256;

pub(crate) mod field {
    pub(crate) const METHOD: u16 = 1;
    pub(crate) const URL: u16 = 2;
    pub(crate) const HEADERS: u16 = 3;
    pub(crate) const BODY: u16 = 4;
    pub(crate) const STATUS: u16 = 5;
    pub(crate) const ELAPSED_MS: u16 = 6;
    pub(crate) const BODY_COMPLETE: u16 = 7;
    pub(crate) const STAGE: u16 = 8;
    pub(crate) const CATEGORY: u16 = 9;
    pub(crate) const AUTH_KIND: u16 = 10;
    pub(crate) const CLIENT_KIND: u16 = 11;
    pub(crate) const CLIENT_VERSION: u16 = 12;
    pub(crate) const VERSION_SOURCE: u16 = 13;
    pub(crate) const USER_AGENT_PROFILE: u16 = 14;
    pub(crate) const PROOF_TOKEN_PRESENT: u16 = 15;
    pub(crate) const PLAYABILITY_STATUS: u16 = 16;
    pub(crate) const PLAYABILITY_REASON: u16 = 17;
    pub(crate) const STREAMING_DATA_PRESENT: u16 = 18;
    pub(crate) const RETURNED_FORMATS: u16 = 19;
    pub(crate) const SUPPORTED_FORMATS: u16 = 20;
    pub(crate) const DIRECT_FORMATS: u16 = 21;
    pub(crate) const CIPHER_FORMATS: u16 = 22;
    pub(crate) const FORMAT_FACTS: u16 = 23;
    pub(crate) const QUALITY: u16 = 24;
    pub(crate) const SELECTED_ITAG: u16 = 25;
    pub(crate) const FALLBACK_ELIGIBLE: u16 = 26;
    pub(crate) const FALLBACK_ATTEMPTED: u16 = 27;
    pub(crate) const OUTCOME: u16 = 28;
    pub(crate) const ERROR_IS_TIMEOUT: u16 = 29;
    pub(crate) const ERROR_IS_CONNECT: u16 = 30;
    pub(crate) const ERROR_IS_REQUEST: u16 = 31;
    pub(crate) const ERROR_IS_BODY: u16 = 32;
    pub(crate) const ERROR_IS_DECODE: u16 = 33;
    pub(crate) const REDIRECTED: u16 = 34;
    pub(crate) const BODY_LENGTH: u16 = 35;
    pub(crate) const RETAINED_BODY_LENGTH: u16 = 36;
    pub(crate) const RANGE_START: u16 = 37;
    pub(crate) const RANGE_END: u16 = 38;
    pub(crate) const CONTENT_RANGE_PRESENT: u16 = 39;
    pub(crate) const RESPONSE_LENGTH: u16 = 40;
    pub(crate) const REQUEST_ORDINAL: u16 = 41;
    pub(crate) const DROPPED_COUNT: u16 = 42;
    pub(crate) const FROM_DISK_CACHE: u16 = 43;
    pub(crate) const FROM_SERVICE_WORKER: u16 = 44;
    pub(crate) const REDIRECT_COUNT: u16 = 45;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PrivatePayloadKind {
    Operation = 1,
    AuthSelection = 2,
    HttpRequest = 3,
    HttpResponse = 4,
    NetworkFailure = 5,
    PlayerParse = 6,
    FormatInventory = 7,
    SelectionDecision = 8,
    BrowserExchange = 9,
    MediaProbe = 10,
    DecodeStage = 11,
    TerminalOutcome = 12,
}

impl PrivatePayloadKind {
    fn from_tag(tag: u8) -> Result<Self, PrivatePayloadError> {
        match tag {
            1 => Ok(Self::Operation),
            2 => Ok(Self::AuthSelection),
            3 => Ok(Self::HttpRequest),
            4 => Ok(Self::HttpResponse),
            5 => Ok(Self::NetworkFailure),
            6 => Ok(Self::PlayerParse),
            7 => Ok(Self::FormatInventory),
            8 => Ok(Self::SelectionDecision),
            9 => Ok(Self::BrowserExchange),
            10 => Ok(Self::MediaProbe),
            11 => Ok(Self::DecodeStage),
            12 => Ok(Self::TerminalOutcome),
            _ => Err(PrivatePayloadError::UnknownKind),
        }
    }
}

pub(crate) enum PrivateValue<'a> {
    Bytes(&'a [u8]),
    Text(&'a str),
    U64(u64),
    Bool(bool),
}

pub(crate) struct PrivateField<'a> {
    tag: u16,
    value: PrivateValue<'a>,
}

impl<'a> PrivateField<'a> {
    pub(crate) const fn bytes(tag: u16, value: &'a [u8]) -> Self {
        Self {
            tag,
            value: PrivateValue::Bytes(value),
        }
    }

    pub(crate) const fn text(tag: u16, value: &'a str) -> Self {
        Self {
            tag,
            value: PrivateValue::Text(value),
        }
    }

    pub(crate) const fn u64(tag: u16, value: u64) -> Self {
        Self {
            tag,
            value: PrivateValue::U64(value),
        }
    }

    pub(crate) const fn boolean(tag: u16, value: bool) -> Self {
        Self {
            tag,
            value: PrivateValue::Bool(value),
        }
    }
}

pub(crate) fn encode_fields(
    kind: PrivatePayloadKind,
    fields: &[PrivateField<'_>],
) -> Result<SensitiveBytes, PrivatePayloadError> {
    if fields.len() > MAX_FIELDS {
        return Err(PrivatePayloadError::TooManyFields);
    }
    let mut output = Zeroizing::new(Vec::new());
    output.extend_from_slice(PAYLOAD_MAGIC);
    output.extend_from_slice(&PRIVATE_PAYLOAD_SCHEMA_VERSION.to_be_bytes());
    output.push(kind as u8);
    output.extend_from_slice(
        &u16::try_from(fields.len())
            .map_err(|_| PrivatePayloadError::TooManyFields)?
            .to_be_bytes(),
    );
    for field in fields {
        output.extend_from_slice(&field.tag.to_be_bytes());
        match field.value {
            PrivateValue::Bytes(value) => append_value(&mut output, 1, value)?,
            PrivateValue::Text(value) => append_value(&mut output, 2, value.as_bytes())?,
            PrivateValue::U64(value) => append_value(&mut output, 3, &value.to_be_bytes())?,
            PrivateValue::Bool(value) => append_value(&mut output, 4, &[u8::from(value)])?,
        }
    }
    Ok(SensitiveBytes::new(std::mem::take(&mut output)))
}

pub(crate) fn encode_http_request(
    request: &Request,
) -> Result<SensitiveBytes, PrivatePayloadError> {
    encode_http_request_bounded(request, usize::MAX).map(|(payload, _)| payload)
}

pub(crate) fn encode_http_request_bounded(
    request: &Request,
    body_limit: usize,
) -> Result<(SensitiveBytes, bool), PrivatePayloadError> {
    let body = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .ok_or(PrivatePayloadError::StreamingBody)?;
    let retained = &body[..body.len().min(body_limit)];
    let complete = retained.len() == body.len();
    let headers = encode_headers(request.headers())?;
    let payload = encode_fields(
        PrivatePayloadKind::HttpRequest,
        &[
            PrivateField::text(field::METHOD, request.method().as_str()),
            PrivateField::text(field::URL, request.url().as_str()),
            PrivateField::bytes(field::HEADERS, &headers),
            PrivateField::bytes(field::BODY, retained),
            PrivateField::u64(
                field::BODY_LENGTH,
                u64::try_from(body.len()).unwrap_or(u64::MAX),
            ),
            PrivateField::u64(
                field::RETAINED_BODY_LENGTH,
                u64::try_from(retained.len()).unwrap_or(u64::MAX),
            ),
            PrivateField::boolean(field::BODY_COMPLETE, complete),
        ],
    )?;
    Ok((payload, complete))
}

pub(crate) fn encode_http_response(
    status: StatusCode,
    final_url: &Url,
    headers: &HeaderMap,
    body: &[u8],
    elapsed: Duration,
    body_complete: bool,
) -> Result<SensitiveBytes, PrivatePayloadError> {
    encode_http_response_bounded(
        status,
        final_url,
        headers,
        body,
        elapsed,
        body_complete,
        false,
        usize::MAX,
    )
    .map(|(payload, _)| payload)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_http_response_bounded(
    status: StatusCode,
    final_url: &Url,
    headers: &HeaderMap,
    body: &[u8],
    elapsed: Duration,
    body_complete: bool,
    redirected: bool,
    body_limit: usize,
) -> Result<(SensitiveBytes, bool), PrivatePayloadError> {
    let retained = &body[..body.len().min(body_limit)];
    let capture_complete = body_complete && retained.len() == body.len();
    let headers = encode_headers(headers)?;
    let payload = encode_fields(
        PrivatePayloadKind::HttpResponse,
        &[
            PrivateField::u64(field::STATUS, u64::from(status.as_u16())),
            PrivateField::text(field::URL, final_url.as_str()),
            PrivateField::bytes(field::HEADERS, &headers),
            PrivateField::bytes(field::BODY, retained),
            PrivateField::u64(
                field::BODY_LENGTH,
                u64::try_from(body.len()).unwrap_or(u64::MAX),
            ),
            PrivateField::u64(
                field::RETAINED_BODY_LENGTH,
                u64::try_from(retained.len()).unwrap_or(u64::MAX),
            ),
            PrivateField::u64(
                field::ELAPSED_MS,
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            ),
            PrivateField::boolean(field::BODY_COMPLETE, capture_complete),
            PrivateField::boolean(field::REDIRECTED, redirected),
        ],
    )?;
    Ok((payload, capture_complete))
}

fn append_value(
    output: &mut Vec<u8>,
    value_kind: u8,
    value: &[u8],
) -> Result<(), PrivatePayloadError> {
    output.push(value_kind);
    output.extend_from_slice(
        &u32::try_from(value.len())
            .map_err(|_| PrivatePayloadError::FieldTooLarge)?
            .to_be_bytes(),
    );
    output.extend_from_slice(value);
    Ok(())
}

pub(crate) fn encode_headers(
    headers: &HeaderMap,
) -> Result<Zeroizing<Vec<u8>>, PrivatePayloadError> {
    if headers.len() > MAX_HEADER_COUNT {
        return Err(PrivatePayloadError::TooManyHeaders);
    }
    let mut output = Zeroizing::new(Vec::new());
    output.extend_from_slice(
        &u16::try_from(headers.len())
            .map_err(|_| PrivatePayloadError::TooManyHeaders)?
            .to_be_bytes(),
    );
    for (name, value) in headers {
        let name = name.as_str().as_bytes();
        let value = value.as_bytes();
        output.extend_from_slice(
            &u16::try_from(name.len())
                .map_err(|_| PrivatePayloadError::FieldTooLarge)?
                .to_be_bytes(),
        );
        output.extend_from_slice(name);
        output.extend_from_slice(
            &u32::try_from(value.len())
                .map_err(|_| PrivatePayloadError::FieldTooLarge)?
                .to_be_bytes(),
        );
        output.extend_from_slice(value);
    }
    Ok(output)
}

pub(crate) struct DecodedPrivatePayload {
    kind: PrivatePayloadKind,
    fields: Vec<DecodedPrivateField>,
}

impl DecodedPrivatePayload {
    pub(crate) const fn kind(&self) -> PrivatePayloadKind {
        self.kind
    }

    pub(crate) fn field_bytes(&self, tag: u16) -> Option<&[u8]> {
        self.fields
            .iter()
            .find(|field| field.tag == tag)
            .map(|field| field.value.expose())
    }

    pub(crate) fn field_u64(&self, tag: u16) -> Option<u64> {
        let field = self.fields.iter().find(|field| field.tag == tag)?;
        (field.value_kind == 3 && field.value.len() == 8).then(|| {
            let mut bytes = [0_u8; 8];
            bytes.copy_from_slice(field.value.expose());
            u64::from_be_bytes(bytes)
        })
    }

    pub(crate) fn field_bool(&self, tag: u16) -> Option<bool> {
        let field = self.fields.iter().find(|field| field.tag == tag)?;
        match (field.value_kind, field.value.expose()) {
            (4, [0]) => Some(false),
            (4, [1]) => Some(true),
            _ => None,
        }
    }

    /// Iterates raw private field values for in-boundary leak canary seeding.
    /// Callers must not format, serialize, or move these values into safe state.
    pub(super) fn field_values(&self) -> impl Iterator<Item = (u16, &[u8])> {
        self.fields
            .iter()
            .map(|field| (field.tag, field.value.expose()))
    }
}

struct DecodedPrivateField {
    tag: u16,
    value_kind: u8,
    value: SensitiveBytes,
}

pub(crate) fn decode_fields(
    payload: &SensitiveBytes,
) -> Result<DecodedPrivatePayload, PrivatePayloadError> {
    let bytes = payload.expose();
    if bytes.len() < 13 || &bytes[..8] != PAYLOAD_MAGIC {
        return Err(PrivatePayloadError::InvalidPayload);
    }
    let schema = u16::from_be_bytes([bytes[8], bytes[9]]);
    if schema != PRIVATE_PAYLOAD_SCHEMA_VERSION {
        return Err(PrivatePayloadError::UnsupportedSchema);
    }
    let kind = PrivatePayloadKind::from_tag(bytes[10])?;
    let count = usize::from(u16::from_be_bytes([bytes[11], bytes[12]]));
    if count > MAX_FIELDS {
        return Err(PrivatePayloadError::TooManyFields);
    }
    let mut cursor = 13_usize;
    let mut fields = Vec::with_capacity(count);
    for _ in 0..count {
        let header = bytes
            .get(cursor..cursor.saturating_add(7))
            .ok_or(PrivatePayloadError::InvalidPayload)?;
        let tag = u16::from_be_bytes([header[0], header[1]]);
        let value_kind = header[2];
        if !(1..=4).contains(&value_kind) {
            return Err(PrivatePayloadError::InvalidPayload);
        }
        let length = usize::try_from(u32::from_be_bytes([
            header[3], header[4], header[5], header[6],
        ]))
        .map_err(|_| PrivatePayloadError::FieldTooLarge)?;
        cursor = cursor.saturating_add(7);
        let value = bytes
            .get(cursor..cursor.saturating_add(length))
            .ok_or(PrivatePayloadError::InvalidPayload)?;
        cursor = cursor.saturating_add(length);
        if fields
            .iter()
            .any(|field: &DecodedPrivateField| field.tag == tag)
        {
            return Err(PrivatePayloadError::DuplicateField);
        }
        fields.push(DecodedPrivateField {
            tag,
            value_kind,
            value: SensitiveBytes::new(value.to_vec()),
        });
    }
    if cursor != bytes.len() {
        return Err(PrivatePayloadError::InvalidPayload);
    }
    Ok(DecodedPrivatePayload { kind, fields })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrivatePayloadError {
    DuplicateField,
    FieldTooLarge,
    InvalidPayload,
    StreamingBody,
    TooManyFields,
    TooManyHeaders,
    UnknownKind,
    UnsupportedSchema,
}

impl std::fmt::Display for PrivatePayloadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("private provider evidence encoding failed")
    }
}

impl std::error::Error for PrivatePayloadError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_payload_round_trips_without_general_serialization() {
        let payload = encode_fields(
            PrivatePayloadKind::PlayerParse,
            &[
                PrivateField::text(field::PLAYABILITY_STATUS, "fixture-private-status"),
                PrivateField::u64(field::RETURNED_FORMATS, 7),
                PrivateField::boolean(field::STREAMING_DATA_PRESENT, true),
            ],
        )
        .unwrap();
        let decoded = decode_fields(&payload).unwrap();
        assert_eq!(decoded.kind(), PrivatePayloadKind::PlayerParse);
        assert_eq!(
            decoded.field_bytes(field::PLAYABILITY_STATUS),
            Some("fixture-private-status".as_bytes())
        );
        assert_eq!(decoded.field_u64(field::RETURNED_FORMATS), Some(7));
        assert_eq!(
            decoded.field_bool(field::STREAMING_DATA_PRESENT),
            Some(true)
        );
    }

    #[test]
    fn exact_request_body_url_and_headers_are_preserved() {
        let request = reqwest::Client::new()
            .post("https://example.invalid/private?fixture=value")
            .header("x-private-fixture", "header-value")
            .body("body-value")
            .build()
            .unwrap();
        let payload = encode_http_request(&request).unwrap();
        let decoded = decode_fields(&payload).unwrap();
        assert_eq!(decoded.kind(), PrivatePayloadKind::HttpRequest);
        assert_eq!(
            decoded.field_bytes(field::BODY),
            Some(b"body-value".as_slice())
        );
        assert_eq!(
            decoded.field_bytes(field::URL),
            Some("https://example.invalid/private?fixture=value".as_bytes())
        );
        assert!(decoded
            .field_bytes(field::HEADERS)
            .unwrap()
            .windows(b"header-value".len())
            .any(|window| window == b"header-value"));
    }

    #[test]
    fn oversized_http_bodies_are_bounded_without_changing_length_evidence() {
        let request = reqwest::Client::new()
            .post("https://example.invalid/player")
            .body(b"0123456789".to_vec())
            .build()
            .unwrap();
        let (request_payload, request_complete) = encode_http_request_bounded(&request, 4).unwrap();
        assert!(!request_complete);
        let request = decode_fields(&request_payload).unwrap();
        assert_eq!(request.field_bytes(field::BODY), Some(&b"0123"[..]));
        assert_eq!(request.field_u64(field::BODY_LENGTH), Some(10));
        assert_eq!(request.field_u64(field::RETAINED_BODY_LENGTH), Some(4));
        assert_eq!(request.field_bool(field::BODY_COMPLETE), Some(false));

        let (response_payload, response_complete) = encode_http_response_bounded(
            StatusCode::OK,
            &Url::parse("https://example.invalid/player").unwrap(),
            &HeaderMap::new(),
            b"abcdefghij",
            Duration::from_millis(3),
            true,
            true,
            5,
        )
        .unwrap();
        assert!(!response_complete);
        let response = decode_fields(&response_payload).unwrap();
        assert_eq!(response.field_bytes(field::BODY), Some(&b"abcde"[..]));
        assert_eq!(response.field_u64(field::BODY_LENGTH), Some(10));
        assert_eq!(response.field_u64(field::RETAINED_BODY_LENGTH), Some(5));
        assert_eq!(response.field_bool(field::BODY_COMPLETE), Some(false));
        assert_eq!(response.field_bool(field::REDIRECTED), Some(true));
    }

    #[test]
    fn unknown_schema_duplicate_fields_and_trailing_bytes_are_rejected() {
        let payload = encode_fields(
            PrivatePayloadKind::Operation,
            &[PrivateField::u64(field::STAGE, 1)],
        )
        .unwrap();
        let mut unknown = payload.expose().to_vec();
        unknown[9] = 2;
        assert_eq!(
            decode_fields(&SensitiveBytes::new(unknown)).err().unwrap(),
            PrivatePayloadError::UnsupportedSchema
        );

        let mut trailing = payload.expose().to_vec();
        trailing.push(0);
        assert_eq!(
            decode_fields(&SensitiveBytes::new(trailing)).err().unwrap(),
            PrivatePayloadError::InvalidPayload
        );

        let duplicate = encode_fields(
            PrivatePayloadKind::Operation,
            &[
                PrivateField::u64(field::STAGE, 1),
                PrivateField::u64(field::STAGE, 2),
            ],
        )
        .unwrap();
        assert_eq!(
            decode_fields(&duplicate).err().unwrap(),
            PrivatePayloadError::DuplicateField
        );
    }
}
