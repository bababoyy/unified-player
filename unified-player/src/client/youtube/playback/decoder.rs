use std::{
    io::{Read, Seek},
    num::NonZeroUsize,
    time::Duration,
};

use anyhow::Context as _;
use tokio_util::sync::CancellationToken;

use stream_download::storage::{adaptive::AdaptiveStorageProvider, temp::TempStorageProvider};

#[cfg(feature = "private-capture")]
use super::forensics::{
    record_decode_stage, CapturedMediaSource, MediaCaptureHandle, MediaCaptureLifecycle,
    MediaPrivateEvidence,
};
use super::{
    innertube::YouTubeProbeRecorder,
    source::ResolvedAudioSource,
    transport::{MediaHttpClient, RedactedMediaUrl},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum YouTubeProbeDecoderChunkSize {
    OneMib,
    TenMib,
}

impl YouTubeProbeDecoderChunkSize {
    pub fn from_cli(value: &str) -> Self {
        match value {
            "10m" => Self::TenMib,
            _ => Self::OneMib,
        }
    }

    pub const fn bytes(self) -> u64 {
        match self {
            Self::OneMib => 1024 * 1024,
            Self::TenMib => 10 * 1024 * 1024,
        }
    }
}

fn native_storage_provider(
    cache_size_bytes: usize,
) -> AdaptiveStorageProvider<TempStorageProvider, TempStorageProvider> {
    AdaptiveStorageProvider::new(
        TempStorageProvider::new(),
        NonZeroUsize::new(cache_size_bytes.max(1))
            .expect("clamped non-zero native audio cache size"),
    )
}

fn build_native_decoder<R>(
    data: R,
    mime_type: &str,
    content_length: Option<u64>,
) -> Result<rodio::Decoder<R>, rodio::decoder::DecoderError>
where
    R: Read + Seek + Send + Sync + 'static,
{
    let base_mime_type = mime_type
        .split_once(';')
        .map_or(mime_type, |(base, _)| base)
        .trim();
    let mut builder = rodio::Decoder::builder()
        .with_data(data)
        .with_hint("mp4")
        .with_mime_type(base_mime_type);
    if let Some(content_length) = content_length {
        builder = builder.with_byte_len(content_length);
    }
    builder.build()
}

pub async fn open_decoded_source(
    source: &ResolvedAudioSource,
    position: Duration,
    cache_size_bytes: usize,
    cancellation: CancellationToken,
) -> anyhow::Result<Box<dyn rodio::Source<Item = f32> + Send>> {
    open_decoded_source_inner(
        source,
        position,
        cache_size_bytes,
        cancellation,
        None,
        None,
        #[cfg(feature = "private-capture")]
        None,
    )
    .await
}

pub(crate) async fn open_decoded_source_for_probe(
    source: &ResolvedAudioSource,
    position: Duration,
    cache_size_bytes: usize,
    cancellation: CancellationToken,
    decoder_chunk_size: YouTubeProbeDecoderChunkSize,
) -> (
    anyhow::Result<Box<dyn rodio::Source<Item = f32> + Send>>,
    Vec<super::innertube::YouTubeProbeAttempt>,
) {
    let recorder = YouTubeProbeRecorder::default();
    let result = open_decoded_source_inner(
        source,
        position,
        cache_size_bytes,
        cancellation,
        Some(recorder.clone()),
        Some(decoder_chunk_size.bytes()),
        #[cfg(feature = "private-capture")]
        None,
    )
    .await;
    (result, recorder.finish())
}

#[cfg(feature = "private-capture")]
pub(crate) async fn open_decoded_source_with_capture(
    source: &ResolvedAudioSource,
    position: Duration,
    cache_size_bytes: usize,
    cancellation: CancellationToken,
    capture: crate::developer_capture::CaptureSession,
    exchange_ref: crate::developer_capture::ExchangeRef,
) -> anyhow::Result<(
    Box<dyn rodio::Source<Item = f32> + Send>,
    MediaCaptureHandle,
)> {
    let evidence = MediaPrivateEvidence::new(capture, exchange_ref);
    let source = open_decoded_source_inner(
        source,
        position,
        cache_size_bytes,
        cancellation.clone(),
        None,
        None,
        Some(evidence.clone()),
    )
    .await?;
    let lifecycle = MediaCaptureLifecycle::new(evidence, cancellation);
    let handle = MediaCaptureHandle::new(lifecycle.clone());
    Ok((
        Box::new(CapturedMediaSource::new(source, lifecycle)),
        handle,
    ))
}

async fn open_decoded_source_inner(
    source: &ResolvedAudioSource,
    position: Duration,
    cache_size_bytes: usize,
    cancellation: CancellationToken,
    probe_recorder: Option<YouTubeProbeRecorder>,
    decoder_chunk_bytes: Option<u64>,
    #[cfg(feature = "private-capture")] evidence: Option<MediaPrivateEvidence>,
) -> anyhow::Result<Box<dyn rodio::Source<Item = f32> + Send>> {
    use stream_download::{http::HttpStream, source::SourceStream as _, Settings, StreamDownload};

    #[cfg(feature = "private-capture")]
    let transport_started = std::time::Instant::now();
    #[cfg(feature = "private-capture")]
    if let Some(evidence) = &evidence {
        record_decode_stage(evidence, "transport_open", "started", 0, Duration::ZERO);
    }
    let http_stream_result = tokio::select! {
        () = cancellation.cancelled() => Err(anyhow::anyhow!("native YouTube stream was cancelled")),
        stream = HttpStream::new(
            MediaHttpClient::new(
                source.required_headers.clone(),
                #[cfg(feature = "private-capture")]
                evidence.clone(),
            )?
            .with_probe(source.source_client, probe_recorder)
            .with_range_chunk_bytes(decoder_chunk_bytes),
            RedactedMediaUrl(source.url.clone()),
        ) => {
            stream.map_err(|_| anyhow::anyhow!("open native YouTube media transport"))
        }
    };
    let http_stream = match http_stream_result {
        Ok(stream) => {
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &evidence {
                record_decode_stage(
                    evidence,
                    "transport_open",
                    "completed",
                    0,
                    transport_started.elapsed(),
                );
            }
            stream
        }
        Err(error) => {
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &evidence {
                record_decode_stage(
                    evidence,
                    "transport_open",
                    if cancellation.is_cancelled() {
                        "cancelled"
                    } else {
                        "failed"
                    },
                    0,
                    transport_started.elapsed(),
                );
            }
            return Err(error);
        }
    };
    tracing::debug!(
        transport_content_length = ?http_stream.content_length(),
        descriptor_content_length = ?source.content_length,
        "Opened native YouTube ranged media transport"
    );
    let progress_cancellation = cancellation.clone();
    let settings = Settings::default().prefetch_bytes(64 * 1024).on_progress(
        move |_, _, stream_cancellation| {
            if progress_cancellation.is_cancelled() {
                stream_cancellation.cancel();
            }
        },
    );
    let storage = native_storage_provider(cache_size_bytes);
    #[cfg(feature = "private-capture")]
    let stream_started = std::time::Instant::now();
    #[cfg(feature = "private-capture")]
    if let Some(evidence) = &evidence {
        record_decode_stage(evidence, "stream_initialize", "started", 0, Duration::ZERO);
    }
    let stream_result = tokio::select! {
        () = cancellation.cancelled() => Err(anyhow::anyhow!("native YouTube stream was cancelled")),
        stream = StreamDownload::from_stream(http_stream, storage, settings) => {
            stream.map_err(|_| anyhow::anyhow!("initialize native YouTube stream"))
        }
    };
    let stream = match stream_result {
        Ok(stream) => {
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &evidence {
                record_decode_stage(
                    evidence,
                    "stream_initialize",
                    "completed",
                    0,
                    stream_started.elapsed(),
                );
            }
            stream
        }
        Err(error) => {
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &evidence {
                record_decode_stage(
                    evidence,
                    "stream_initialize",
                    if cancellation.is_cancelled() {
                        "cancelled"
                    } else {
                        "failed"
                    },
                    0,
                    stream_started.elapsed(),
                );
            }
            return Err(error);
        }
    };
    let decoder_cancellation = cancellation.clone();
    let decoder_mime_type = source.mime_type.clone();
    let decoder_content_length = source.content_length;
    #[cfg(feature = "private-capture")]
    let decoder_evidence = evidence.clone();
    let decoder_result = tokio::task::spawn_blocking(move || {
        use rodio::Source as _;

        #[cfg(feature = "private-capture")]
        let decoder_started = std::time::Instant::now();
        #[cfg(feature = "private-capture")]
        if let Some(evidence) = &decoder_evidence {
            record_decode_stage(evidence, "decoder_initialize", "started", 0, Duration::ZERO);
        }
        if decoder_cancellation.is_cancelled() {
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &decoder_evidence {
                record_decode_stage(
                    evidence,
                    "decoder_initialize",
                    "cancelled",
                    0,
                    decoder_started.elapsed(),
                );
            }
            anyhow::bail!("native YouTube decoder initialization was cancelled");
        }
        let decoder = build_native_decoder(stream, &decoder_mime_type, decoder_content_length)
            .context("decode native YouTube audio stream");
        let mut decoder = match decoder {
            Ok(decoder) => {
                #[cfg(feature = "private-capture")]
                if let Some(evidence) = &decoder_evidence {
                    record_decode_stage(
                        evidence,
                        "decoder_initialize",
                        "completed",
                        0,
                        decoder_started.elapsed(),
                    );
                }
                decoder
            }
            Err(error) => {
                #[cfg(feature = "private-capture")]
                if let Some(evidence) = &decoder_evidence {
                    record_decode_stage(
                        evidence,
                        "decoder_initialize",
                        "failed",
                        0,
                        decoder_started.elapsed(),
                    );
                }
                return Err(error);
            }
        };
        if !position.is_zero() {
            #[cfg(feature = "private-capture")]
            let seek_started = std::time::Instant::now();
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &decoder_evidence {
                record_decode_stage(evidence, "decoder_seek", "started", 0, Duration::ZERO);
            }
            if let Err(_error) = decoder.try_seek(position) {
                #[cfg(feature = "private-capture")]
                if let Some(evidence) = &decoder_evidence {
                    record_decode_stage(
                        evidence,
                        "decoder_seek",
                        "failed",
                        0,
                        seek_started.elapsed(),
                    );
                }
                return Err(anyhow::anyhow!("seek native YouTube audio stream"));
            }
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &decoder_evidence {
                record_decode_stage(
                    evidence,
                    "decoder_seek",
                    "completed",
                    0,
                    seek_started.elapsed(),
                );
            }
        }
        if decoder_cancellation.is_cancelled() {
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &decoder_evidence {
                record_decode_stage(
                    evidence,
                    "decoder_initialize",
                    "cancelled",
                    0,
                    decoder_started.elapsed(),
                );
            }
            anyhow::bail!("native YouTube decoder initialization was cancelled");
        }
        Ok::<Box<dyn rodio::Source<Item = f32> + Send>, anyhow::Error>(Box::new(decoder))
    })
    .await
    .context("join native YouTube decoder initialization")?;
    decoder_result
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read as _, Seek as _, SeekFrom, Write as _};

    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use stream_download::storage::StorageProvider as _;

    use super::{build_native_decoder, native_storage_provider};

    // One silent fragmented MP4/AAC packet generated solely as a local decoder fixture.
    const SILENT_FRAGMENTED_M4A: &str = "AAAAIGZ0eXBNNEEgAAACAE00QSBpc282aXNvbWlzbzIAAALMbW9vdgAAAGxtdmhkAAAAAAAAAAAAAAAAAAAD6AAAAAAAAQAAAQAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgAAAc90cmFrAAAAXHRraGQAAAADAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAAAAAAAAAAEBAAAAAAEAAAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAAAAkZWR0cwAAABxlbHN0AAAAAAAAAAEAAAAAAAAEAAABAAAAAAFHbWRpYQAAACBtZGhkAAAAAAAAAAAAAAAAAACsRAAAAABVxAAAAAAALWhkbHIAAAAAAAAAAHNvdW4AAAAAAAAAAAAAAABTb3VuZEhhbmRsZXIAAAAA8m1pbmYAAAAQc21oZAAAAAAAAAAAAAAAJGRpbmYAAAAcZHJlZgAAAAAAAAABAAAADHVybCAAAAABAAAAtnN0YmwAAABqc3RzZAAAAAAAAAABAAAAWm1wNGEAAAAAAAAAAQAAAAAAAAAAAAIAEAAAAACsRAAAAAAANmVzZHMAAAAAA4CAgCUAAQAEgICAF0AVAAAAAAD6AAAA+gAFgICABRIQVuUABoCAgAECAAAAEHN0dHMAAAAAAAAAAAAAABBzdHNjAAAAAAAAAAAAAAAUc3RzegAAAAAAAAAAAAAAAAAAABBzdGNvAAAAAAAAAAAAAAAobXZleAAAACB0cmV4AAAAAAAAAAEAAAABAAAAAAAAAAAAAAAAAAAAYXVkdGEAAABZbWV0YQAAAAAAAAAhaGRscgAAAAAAAAAAbWRpcmFwcGwAAAAAAAAAAAAAAAAsaWxzdAAAACSpdG9vAAAAHGRhdGEAAAABAAAAAExhdmY2MS41LjEwMQAAAIxtb29mAAAAEG1maGQAAAAAAAAAAQAAAHR0cmFmAAAAJHRmaGQAAAA5AAAAAQAAAAAAAALsAAAEAAAAABcCAAAAAAAAFHRmZHQBAAAAAAAAAAAAAAAAAAA0dHJ1bgAAAwEAAAAEAAAAlAAABAAAAAAXAAAEAAAAAAYAAAQAAAAABgAAAJ0AAAAGAAAAMW1kYXTeAgBMYXZjNjEuMTEuMTAwAEIgCMEYOCEQBGCMHCEQBGCMHCEQBGCMHAAAAENtZnJhAAAAK3RmcmEBAAAAAAAAAQAAAAAAAAABAAAAAAAAAAAAAAAAAAAC7AEBAQAAABBtZnJvAAAAAAAAAEM=";

    #[test]
    fn initializes_mp4_decoder_with_descriptor_metadata() {
        let media = STANDARD
            .decode(SILENT_FRAGMENTED_M4A)
            .expect("valid embedded MP4 fixture");
        let media_len = media.len() as u64;

        let decoder = build_native_decoder(
            Cursor::new(media),
            "audio/mp4; codecs=\"mp4a.40.2\"",
            Some(media_len),
        );

        assert!(decoder.is_ok(), "valid MP4/AAC fixture should initialize");
    }

    #[test]
    fn known_finite_native_storage_allows_seek_before_first_read() {
        let (mut reader, mut writer) = native_storage_provider(128)
            .into_reader_writer(Some(64))
            .unwrap();
        writer.write_all(&[7_u8; 64]).unwrap();
        writer.flush().unwrap();

        reader.seek(SeekFrom::Start(32)).unwrap();
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut byte = [0_u8; 1];
        reader.read_exact(&mut byte).unwrap();

        assert_eq!(byte, [7]);
    }
}
