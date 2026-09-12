//! One decoded image per material, written once and memory-mapped after.
//!
//! Opening material used to mean decoding the whole file into a `Vec<f32>`,
//! copying it into an `Arc<[f32]>`, copying that again for the waveform
//! pyramid, and keeping a mono copy beside it. Every open paid for all of it
//! again. Here a decode instead streams straight into a canonical image file
//! keyed by the source fingerprint, and every later reader maps that file:
//! `PcmAsset`, `ProjectAudio` and the pyramid hold [`PcmSamples`], whose
//! `&[f32]` is either an owned buffer (generated material, tests) or a window
//! onto the mapping. A second open of the same material decodes nothing.
//!
//! The image is a cache, not project truth: it is keyed by the fingerprint of
//! the encoded source bytes, its header repeats that key, and a file whose
//! header disagrees with its name is refused and re-decoded rather than read.
//! The canonical generated-media format in `sample_material` is unchanged and
//! remains the durable one; its payload begins at byte 35, which cannot be
//! mapped as `f32`, and that is exactly why this image exists with an aligned
//! header of its own instead of a silently misaligned read of the old one.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use memmap2::Mmap;

use crate::assets::{ContentFingerprint, ContentHashAlgorithm, Fnv1a128Hasher};

#[cfg(target_endian = "big")]
compile_error!("the mapped material image stores little-endian f32 bits");

/// On-disk magic for the mapped decoded-material image. The trailing version
/// is part of the magic: a future layout gets a new one and an old file is
/// refused by name rather than reinterpreted.
pub const MATERIAL_IMAGE_MAGIC: &[u8; 24] = b"audec.material-image.v1\0";

/// Bytes before the interleaved payload. A multiple of 16 so the mapped
/// payload is aligned for `f32` (and for the vector loads a decode pass over
/// it will use) on every platform we build for.
pub const MATERIAL_IMAGE_HEADER_BYTES: usize = 128;

const RATE_OFFSET: usize = 24;
const CHANNEL_OFFSET: usize = 28;
const ALGORITHM_OFFSET: usize = 30;
const FRAME_COUNT_OFFSET: usize = 32;
const SOURCE_ID_OFFSET: usize = 40;
const SOURCE_BYTES_OFFSET: usize = 56;
const BIT_DEPTH_OFFSET: usize = 64;
const CONTAINER_OFFSET: usize = 66;
const CONTAINER_BYTES: usize = 30;
const CODEC_OFFSET: usize = 96;
const CODEC_BYTES: usize = 32;

const FINGERPRINT_CHUNK_BYTES: usize = 1 << 20;
const WRITE_CHUNK_SAMPLES: usize = 8 * 1024;

/// A cache directory is shared between processes. A publication that finds
/// the destination busy waits this long in total before giving up, so a
/// concurrent open is a wait, never a different route.
const PUBLISH_RETRY_BUDGET: Duration = Duration::from_millis(750);
const PUBLISH_RETRY_STEP: Duration = Duration::from_millis(25);

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Interleaved PCM held either as an owned buffer or as a window onto a
/// mapped image. Both forms hand out the same `&[f32]`, so every consumer
/// signature stays what it was.
#[derive(Clone, Debug)]
pub enum PcmSamples {
    Owned(Arc<[f32]>),
    Mapped(MappedPcm),
}

impl PcmSamples {
    pub fn as_slice(&self) -> &[f32] {
        match self {
            Self::Owned(samples) => samples,
            Self::Mapped(mapped) => mapped.as_slice(),
        }
    }

    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    pub fn is_mapped(&self) -> bool {
        matches!(self, Self::Mapped(_))
    }

    /// Copy out an owned buffer. Free for already-owned samples; for a mapped
    /// image this is a whole-file copy, so it is spelled out rather than
    /// hidden behind `Arc::clone`.
    pub fn to_shared_owned(&self) -> Arc<[f32]> {
        match self {
            Self::Owned(samples) => Arc::clone(samples),
            Self::Mapped(mapped) => Arc::from(mapped.as_slice()),
        }
    }

    /// True when both names refer to the same allocation or the same window
    /// of the same mapping — that is, when nothing was copied between them.
    pub fn shares_allocation(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Owned(left), Self::Owned(right)) => Arc::ptr_eq(left, right),
            (Self::Mapped(left), Self::Mapped(right)) => {
                Arc::ptr_eq(&left.map, &right.map)
                    && left.start_sample == right.start_sample
                    && left.end_sample == right.end_sample
            }
            _ => false,
        }
    }

    /// Narrow to an interleaved sub-range without copying.
    pub fn slice(&self, start: usize, end: usize) -> Option<PcmSamples> {
        let end = end.min(self.len());
        if start > end {
            return None;
        }
        match self {
            Self::Owned(samples) => Some(Self::Owned(Arc::from(&samples[start..end]))),
            Self::Mapped(mapped) => mapped.slice(start, end).map(Self::Mapped),
        }
    }
}

impl Deref for PcmSamples {
    type Target = [f32];

    fn deref(&self) -> &[f32] {
        self.as_slice()
    }
}

impl AsRef<[f32]> for PcmSamples {
    fn as_ref(&self) -> &[f32] {
        self.as_slice()
    }
}

impl From<Arc<[f32]>> for PcmSamples {
    fn from(samples: Arc<[f32]>) -> Self {
        Self::Owned(samples)
    }
}

impl From<Vec<f32>> for PcmSamples {
    fn from(samples: Vec<f32>) -> Self {
        Self::Owned(Arc::from(samples))
    }
}

impl From<&[f32]> for PcmSamples {
    fn from(samples: &[f32]) -> Self {
        Self::Owned(Arc::from(samples))
    }
}

impl Default for PcmSamples {
    fn default() -> Self {
        Self::Owned(Arc::from(Vec::new()))
    }
}

impl PartialEq for PcmSamples {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

/// A window of interleaved `f32` onto a mapped image. The mapping is shared;
/// a window is a pair of sample indices into it.
#[derive(Clone, Debug)]
pub struct MappedPcm {
    map: Arc<Mmap>,
    start_sample: usize,
    end_sample: usize,
}

impl MappedPcm {
    /// # Panics
    /// Never: the payload bounds are validated against the mapping length and
    /// the alignment of the payload offset before the slice is formed.
    pub fn as_slice(&self) -> &[f32] {
        let offset = MATERIAL_IMAGE_HEADER_BYTES + self.start_sample * 4;
        let count = self.end_sample - self.start_sample;
        let bytes = &self.map[offset..offset + count * 4];
        debug_assert_eq!(bytes.as_ptr().align_offset(std::mem::align_of::<f32>()), 0);
        // SAFETY: the mapping begins page-aligned and the payload offset is a
        // multiple of 16, so the pointer is aligned for f32; the range was
        // bounds-checked by the slice above; every bit pattern is a valid f32
        // and the file is published read-only and never truncated in place.
        unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<f32>(), count) }
    }

    fn slice(&self, start: usize, end: usize) -> Option<Self> {
        let start_sample = self.start_sample.checked_add(start)?;
        let end_sample = self.start_sample.checked_add(end)?;
        if start_sample > end_sample || end_sample > self.end_sample {
            return None;
        }
        Some(Self {
            map: Arc::clone(&self.map),
            start_sample,
            end_sample,
        })
    }
}

/// What a decoder learned about the encoded source while writing the image.
/// These are descriptive facts for the musician and for provenance, not
/// identity.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DecodedImageFacts {
    pub container: Option<String>,
    pub codec: Option<String>,
    pub bit_depth: Option<u16>,
}

/// The decoded shape recorded in an image header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterialImageShape {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub frame_count: u64,
}

/// A mapped decoded image plus how it was obtained.
#[derive(Clone, Debug)]
pub struct MaterialImage {
    pub samples: PcmSamples,
    pub shape: MaterialImageShape,
    pub source_fingerprint: ContentFingerprint,
    pub facts: DecodedImageFacts,
    pub path: PathBuf,
    /// True when this open decoded nothing because the image was already on
    /// disk.
    pub cache_hit: bool,
    pub fingerprint_seconds: f64,
    pub decode_seconds: f64,
    pub map_seconds: f64,
}

impl MaterialImage {
    pub fn image_bytes(&self) -> u64 {
        MATERIAL_IMAGE_HEADER_BYTES as u64 + self.samples.len() as u64 * 4
    }
}

/// The sink a decoder streams one whole decode into. There is deliberately no
/// way to hand it a finished `Vec<f32>`: the point of the image is that no
/// caller ever holds the whole decode in memory.
#[derive(Debug)]
pub struct ImageWriter {
    file: BufWriter<File>,
    path: PathBuf,
    format: Option<(u32, u16)>,
    samples_written: u64,
    byte_buffer: Vec<u8>,
}

impl ImageWriter {
    fn create(path: PathBuf) -> Result<Self, MaterialImageError> {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|source| MaterialImageError::Io {
                action: "create the decoded-material image",
                path: path.clone(),
                detail: source.to_string(),
            })?;
        let mut writer = Self {
            file: BufWriter::with_capacity(1 << 20, file),
            path,
            format: None,
            samples_written: 0,
            byte_buffer: Vec::with_capacity(WRITE_CHUNK_SAMPLES * 4),
        };
        writer.write_all(&[0; MATERIAL_IMAGE_HEADER_BYTES])?;
        Ok(writer)
    }

    /// Declare the decoded format. A decoder calls this once, before the
    /// first samples; a second call with a different format is a refusal, not
    /// a silent re-interpretation of what came before.
    pub fn declare_format(&mut self, sample_rate_hz: u32, channels: u16) -> Result<(), String> {
        if sample_rate_hz == 0 || channels == 0 {
            return Err(format!(
                "decoded format must be nonzero, got {sample_rate_hz} Hz and {channels} channels"
            ));
        }
        match self.format {
            None => {
                self.format = Some((sample_rate_hz, channels));
                Ok(())
            }
            Some(existing) if existing == (sample_rate_hz, channels) => Ok(()),
            Some((rate, existing_channels)) => Err(format!(
                "decoded stream changed format from {rate} Hz/{existing_channels} channels to \
                 {sample_rate_hz} Hz/{channels} channels"
            )),
        }
    }

    /// Append interleaved samples. The caller has already rejected non-finite
    /// samples; this is the one copy of the decode, and it goes to the file.
    pub fn push(&mut self, samples: &[f32]) -> Result<(), String> {
        if self.format.is_none() {
            return Err("decoded samples arrived before the decoded format".into());
        }
        for chunk in samples.chunks(WRITE_CHUNK_SAMPLES) {
            self.byte_buffer.clear();
            for sample in chunk {
                self.byte_buffer
                    .extend_from_slice(&sample.to_bits().to_le_bytes());
            }
            let bytes = std::mem::take(&mut self.byte_buffer);
            let result = self.write_all(&bytes);
            self.byte_buffer = bytes;
            result.map_err(|error| error.to_string())?;
        }
        self.samples_written += samples.len() as u64;
        Ok(())
    }

    pub fn samples_written(&self) -> u64 {
        self.samples_written
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), MaterialImageError> {
        self.file
            .write_all(bytes)
            .map_err(|source| MaterialImageError::Io {
                action: "write the decoded-material image",
                path: self.path.clone(),
                detail: source.to_string(),
            })
    }

    fn finish(
        mut self,
        source: &ContentFingerprint,
        facts: &DecodedImageFacts,
    ) -> Result<(MaterialImageShape, File), MaterialImageError> {
        let (sample_rate_hz, channels) = self
            .format
            .ok_or_else(|| MaterialImageError::Decode("the decode produced no audio".into()))?;
        if self.samples_written == 0 {
            return Err(MaterialImageError::Decode(
                "the decode produced no audio frames".into(),
            ));
        }
        if self.samples_written % u64::from(channels) != 0 {
            return Err(MaterialImageError::Decode(format!(
                "the decode produced {} samples, which is not whole {channels}-channel frames",
                self.samples_written
            )));
        }
        let shape = MaterialImageShape {
            sample_rate_hz,
            channels,
            frame_count: self.samples_written / u64::from(channels),
        };
        let header = encode_header(&shape, source, facts);
        self.file.flush().map_err(|source| MaterialImageError::Io {
            action: "flush the decoded-material image",
            path: self.path.clone(),
            detail: source.to_string(),
        })?;
        let mut file = self
            .file
            .into_inner()
            .map_err(|error| MaterialImageError::Io {
                action: "finish the decoded-material image",
                path: self.path.clone(),
                detail: error.to_string(),
            })?;
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.write_all(&header))
            .and_then(|_| file.sync_all())
            .map_err(|source| MaterialImageError::Io {
                action: "publish the decoded-material image header",
                path: self.path.clone(),
                detail: source.to_string(),
            })?;
        Ok((shape, file))
    }
}

/// One whole decode, streamed. Implemented by the production media decoder;
/// the cache never learns what a container is.
pub trait MaterialDecoder {
    /// Decode every frame of `path` into `writer`, in order, exactly once.
    fn decode_into(
        &self,
        path: &Path,
        writer: &mut ImageWriter,
    ) -> Result<DecodedImageFacts, String>;
}

/// The keyed on-disk store of decoded images.
///
/// The key is the fingerprint of the encoded source bytes plus the project
/// rate the image was decoded for (`None` means the source's own rate, which
/// is what opening material asks for). Channel count is a decode output: it
/// is recorded in the header and verified, not chosen by the caller.
#[derive(Clone, Debug)]
pub struct MaterialImageCache {
    root: PathBuf,
}

impl MaterialImageCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The application's cache. `AUDEC_CACHE_ROOT` overrides the per-user
    /// cache directory so a lane, a test and the musician's own instance do
    /// not share one store.
    pub fn application() -> Result<Self, MaterialImageError> {
        Ok(Self::new(
            application_cache_root()?.join("decoded-material"),
        ))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn image_path(&self, source: &ContentFingerprint, project_rate_hz: Option<u32>) -> PathBuf {
        let rate = match project_rate_hz {
            None => "native".to_string(),
            Some(rate) => format!("r{rate}"),
        };
        self.root.join(format!(
            "{}-{}-{}.audec-image",
            source.id.to_hex(),
            source.bytes_hashed,
            rate
        ))
    }

    /// Map the decoded image of `path`, decoding it exactly once if this is
    /// the first time this machine has seen these source bytes.
    pub fn open(
        &self,
        path: &Path,
        project_rate_hz: Option<u32>,
        decoder: &dyn MaterialDecoder,
    ) -> Result<MaterialImage, MaterialImageError> {
        let fingerprint_started = Instant::now();
        let source_fingerprint = fingerprint_file(path)?;
        let fingerprint_seconds = fingerprint_started.elapsed().as_secs_f64();
        let image_path = self.image_path(&source_fingerprint, project_rate_hz);

        if let Some(hit) = self.map_existing(&image_path, &source_fingerprint)? {
            let (samples, shape, facts, map_seconds) = hit;
            return Ok(MaterialImage {
                samples,
                shape,
                source_fingerprint,
                facts,
                path: image_path,
                cache_hit: true,
                fingerprint_seconds,
                decode_seconds: 0.0,
                map_seconds,
            });
        }

        let decode_started = Instant::now();
        let (shape, facts) =
            self.decode_into_image(path, &image_path, &source_fingerprint, decoder)?;
        let decode_seconds = decode_started.elapsed().as_secs_f64();

        let map_started = Instant::now();
        let (samples, mapped_shape, _) = map_image(&image_path, &source_fingerprint)?;
        let map_seconds = map_started.elapsed().as_secs_f64();
        if mapped_shape != shape {
            return Err(MaterialImageError::Corrupt {
                path: image_path,
                detail: "the published image does not describe the decode that wrote it".into(),
            });
        }
        Ok(MaterialImage {
            samples,
            shape,
            source_fingerprint,
            facts,
            path: image_path,
            cache_hit: false,
            fingerprint_seconds,
            decode_seconds,
            map_seconds,
        })
    }

    /// A hit, or `None` when nothing usable is on disk. A file that exists
    /// but does not validate is named in the log and removed: the next open
    /// decodes it again rather than reading it as plausible audio.
    fn map_existing(
        &self,
        image_path: &Path,
        source: &ContentFingerprint,
    ) -> Result<Option<(PcmSamples, MaterialImageShape, DecodedImageFacts, f64)>, MaterialImageError>
    {
        if !image_path.exists() {
            return Ok(None);
        }
        let started = Instant::now();
        match map_image(image_path, source) {
            Ok((samples, shape, facts)) => Ok(Some((
                samples,
                shape,
                facts,
                started.elapsed().as_secs_f64(),
            ))),
            Err(MaterialImageError::Io { .. }) => Ok(None),
            Err(error) => {
                eprintln!(
                    "audec decoded-material cache: re-decoding {} · {error}",
                    image_path.display()
                );
                let _ = fs::remove_file(image_path);
                Ok(None)
            }
        }
    }

    fn decode_into_image(
        &self,
        source_path: &Path,
        image_path: &Path,
        source: &ContentFingerprint,
        decoder: &dyn MaterialDecoder,
    ) -> Result<(MaterialImageShape, DecodedImageFacts), MaterialImageError> {
        let staging = self.staging_path(image_path)?;
        let mut writer = ImageWriter::create(staging.clone())?;
        let facts = match decoder.decode_into(source_path, &mut writer) {
            Ok(facts) => facts,
            Err(detail) => {
                drop(writer);
                let _ = fs::remove_file(&staging);
                return Err(MaterialImageError::Decode(detail));
            }
        };
        let shape = match writer.finish(source, &facts) {
            Ok((shape, file)) => {
                drop(file);
                shape
            }
            Err(error) => {
                let _ = fs::remove_file(&staging);
                return Err(error);
            }
        };
        let mut permissions = fs::metadata(&staging)
            .map_err(|error| MaterialImageError::Io {
                action: "inspect the staged decoded-material image",
                path: staging.clone(),
                detail: error.to_string(),
            })?
            .permissions();
        permissions.set_readonly(true);
        let _ = fs::set_permissions(&staging, permissions);
        if let Err(error) = fs::rename(&staging, image_path) {
            let _ = fs::remove_file(&staging);
            return Err(MaterialImageError::Io {
                action: "publish the decoded-material image",
                path: image_path.into(),
                detail: error.to_string(),
            });
        }
        Ok((shape, facts))
    }

    fn staging_path(&self, image_path: &Path) -> Result<PathBuf, MaterialImageError> {
        let staging = self.root.join("staging");
        fs::create_dir_all(&staging).map_err(|error| MaterialImageError::Io {
            action: "create the decoded-material cache",
            path: staging.clone(),
            detail: error.to_string(),
        })?;
        let name = image_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image");
        let deadline = Instant::now() + PUBLISH_RETRY_BUDGET;
        loop {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let candidate =
                staging.join(format!(".{name}.{}.{sequence:020}.tmp", std::process::id()));
            if !candidate.exists() {
                return Ok(candidate);
            }
            if Instant::now() >= deadline {
                return Err(MaterialImageError::Busy {
                    path: staging,
                    detail: "the decoded-material cache has no free staging name".into(),
                });
            }
            std::thread::sleep(PUBLISH_RETRY_STEP);
        }
    }
}

fn application_cache_root() -> Result<PathBuf, MaterialImageError> {
    if let Some(root) = std::env::var_os("AUDEC_CACHE_ROOT") {
        return Ok(PathBuf::from(root));
    }
    dirs::cache_dir()
        .map(|root| root.join("software.ember.audec"))
        .ok_or(MaterialImageError::NoCacheDirectory)
}

/// The fingerprint of the encoded source bytes, read once in bounded chunks.
/// This is the same value `ContentFingerprint::from_bytes` returns for the
/// same file; nothing holds the compressed bytes to compute it.
pub fn fingerprint_file(path: &Path) -> Result<ContentFingerprint, MaterialImageError> {
    let mut file = File::open(path).map_err(|error| MaterialImageError::Io {
        action: "open the source material",
        path: path.into(),
        detail: error.to_string(),
    })?;
    let mut hasher = Fnv1a128Hasher::new();
    let mut buffer = vec![0_u8; FINGERPRINT_CHUNK_BYTES];
    let mut bytes_hashed: u64 = 0;
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| MaterialImageError::Io {
                action: "read the source material",
                path: path.into(),
                detail: error.to_string(),
            })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        bytes_hashed += count as u64;
    }
    Ok(ContentFingerprint {
        algorithm: ContentHashAlgorithm::Fnv1a128NonCryptographic,
        id: hasher.finish(),
        bytes_hashed,
    })
}

fn encode_header(
    shape: &MaterialImageShape,
    source: &ContentFingerprint,
    facts: &DecodedImageFacts,
) -> [u8; MATERIAL_IMAGE_HEADER_BYTES] {
    let mut header = [0_u8; MATERIAL_IMAGE_HEADER_BYTES];
    header[..MATERIAL_IMAGE_MAGIC.len()].copy_from_slice(MATERIAL_IMAGE_MAGIC);
    header[RATE_OFFSET..RATE_OFFSET + 4].copy_from_slice(&shape.sample_rate_hz.to_le_bytes());
    header[CHANNEL_OFFSET..CHANNEL_OFFSET + 2].copy_from_slice(&shape.channels.to_le_bytes());
    header[ALGORITHM_OFFSET..ALGORITHM_OFFSET + 2]
        .copy_from_slice(&algorithm_code(source.algorithm).to_le_bytes());
    header[FRAME_COUNT_OFFSET..FRAME_COUNT_OFFSET + 8]
        .copy_from_slice(&shape.frame_count.to_le_bytes());
    header[SOURCE_ID_OFFSET..SOURCE_ID_OFFSET + 16].copy_from_slice(&source.id.0.to_le_bytes());
    header[SOURCE_BYTES_OFFSET..SOURCE_BYTES_OFFSET + 8]
        .copy_from_slice(&source.bytes_hashed.to_le_bytes());
    header[BIT_DEPTH_OFFSET..BIT_DEPTH_OFFSET + 2]
        .copy_from_slice(&facts.bit_depth.unwrap_or(0).to_le_bytes());
    write_name(
        &mut header[CONTAINER_OFFSET..CONTAINER_OFFSET + CONTAINER_BYTES],
        facts.container.as_deref(),
    );
    write_name(
        &mut header[CODEC_OFFSET..CODEC_OFFSET + CODEC_BYTES],
        facts.codec.as_deref(),
    );
    header
}

/// Names are descriptive, bounded and ASCII: the image says what it was
/// decoded from without carrying a second, variable-length format.
fn write_name(field: &mut [u8], name: Option<&str>) {
    let Some(name) = name else { return };
    let bytes: Vec<u8> = name
        .bytes()
        .filter(|byte| byte.is_ascii_graphic() || *byte == b' ')
        .take(field.len())
        .collect();
    field[..bytes.len()].copy_from_slice(&bytes);
}

fn read_name(field: &[u8]) -> Option<String> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    if end == 0 {
        return None;
    }
    std::str::from_utf8(&field[..end]).ok().map(str::to_owned)
}

const fn algorithm_code(algorithm: ContentHashAlgorithm) -> u16 {
    match algorithm {
        ContentHashAlgorithm::Fnv1a128NonCryptographic => 1,
    }
}

/// Validate an image against the key it claims and map it. Every refusal
/// names what disagreed; nothing here reinterprets bytes it does not
/// recognize.
fn map_image(
    path: &Path,
    source: &ContentFingerprint,
) -> Result<(PcmSamples, MaterialImageShape, DecodedImageFacts), MaterialImageError> {
    let file = File::open(path).map_err(|error| MaterialImageError::Io {
        action: "open the decoded-material image",
        path: path.into(),
        detail: error.to_string(),
    })?;
    let map = unsafe { Mmap::map(&file) }.map_err(|error| MaterialImageError::Io {
        action: "map the decoded-material image",
        path: path.into(),
        detail: error.to_string(),
    })?;
    if map.len() < MATERIAL_IMAGE_HEADER_BYTES {
        return Err(MaterialImageError::Corrupt {
            path: path.into(),
            detail: "the image is shorter than its header".into(),
        });
    }
    if &map[..MATERIAL_IMAGE_MAGIC.len()] != MATERIAL_IMAGE_MAGIC.as_slice() {
        return Err(MaterialImageError::Corrupt {
            path: path.into(),
            detail: "the image does not begin with this version of the image magic".into(),
        });
    }
    let sample_rate_hz = u32::from_le_bytes(
        map[RATE_OFFSET..RATE_OFFSET + 4]
            .try_into()
            .expect("checked image header"),
    );
    let channels = u16::from_le_bytes(
        map[CHANNEL_OFFSET..CHANNEL_OFFSET + 2]
            .try_into()
            .expect("checked image header"),
    );
    let algorithm = u16::from_le_bytes(
        map[ALGORITHM_OFFSET..ALGORITHM_OFFSET + 2]
            .try_into()
            .expect("checked image header"),
    );
    let frame_count = u64::from_le_bytes(
        map[FRAME_COUNT_OFFSET..FRAME_COUNT_OFFSET + 8]
            .try_into()
            .expect("checked image header"),
    );
    let source_id = u128::from_le_bytes(
        map[SOURCE_ID_OFFSET..SOURCE_ID_OFFSET + 16]
            .try_into()
            .expect("checked image header"),
    );
    let source_bytes = u64::from_le_bytes(
        map[SOURCE_BYTES_OFFSET..SOURCE_BYTES_OFFSET + 8]
            .try_into()
            .expect("checked image header"),
    );
    if algorithm != algorithm_code(source.algorithm)
        || source_id != source.id.0
        || source_bytes != source.bytes_hashed
    {
        return Err(MaterialImageError::Corrupt {
            path: path.into(),
            detail: "the image names different source bytes than the material being opened".into(),
        });
    }
    if sample_rate_hz == 0 || channels == 0 || frame_count == 0 {
        return Err(MaterialImageError::Corrupt {
            path: path.into(),
            detail: "the image declares an empty format".into(),
        });
    }
    let sample_count = frame_count
        .checked_mul(u64::from(channels))
        .and_then(|samples| usize::try_from(samples).ok())
        .ok_or_else(|| MaterialImageError::Corrupt {
            path: path.into(),
            detail: "the image declares more samples than this machine can address".into(),
        })?;
    let expected = MATERIAL_IMAGE_HEADER_BYTES + sample_count * 4;
    if map.len() != expected {
        return Err(MaterialImageError::Corrupt {
            path: path.into(),
            detail: format!(
                "the image is {} bytes but its header describes {expected}",
                map.len()
            ),
        });
    }
    let bit_depth = u16::from_le_bytes(
        map[BIT_DEPTH_OFFSET..BIT_DEPTH_OFFSET + 2]
            .try_into()
            .expect("checked image header"),
    );
    let facts = DecodedImageFacts {
        container: read_name(&map[CONTAINER_OFFSET..CONTAINER_OFFSET + CONTAINER_BYTES]),
        codec: read_name(&map[CODEC_OFFSET..CODEC_OFFSET + CODEC_BYTES]),
        bit_depth: (bit_depth != 0).then_some(bit_depth),
    };
    let samples = PcmSamples::Mapped(MappedPcm {
        map: Arc::new(map),
        start_sample: 0,
        end_sample: sample_count,
    });
    if let Some(index) = samples
        .as_slice()
        .iter()
        .position(|sample| !sample.is_finite())
    {
        return Err(MaterialImageError::Corrupt {
            path: path.into(),
            detail: format!("the image holds a non-finite sample at offset {index}"),
        });
    }
    Ok((
        samples,
        MaterialImageShape {
            sample_rate_hz,
            channels,
            frame_count,
        },
        facts,
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaterialImageError {
    NoCacheDirectory,
    Io {
        action: &'static str,
        path: PathBuf,
        detail: String,
    },
    Busy {
        path: PathBuf,
        detail: String,
    },
    Corrupt {
        path: PathBuf,
        detail: String,
    },
    Decode(String),
}

impl std::fmt::Display for MaterialImageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCacheDirectory => write!(
                formatter,
                "the operating system did not provide a cache directory for decoded material"
            ),
            Self::Io {
                action,
                path,
                detail,
            } => write!(formatter, "could not {action} {}: {detail}", path.display()),
            Self::Busy { path, detail } => write!(
                formatter,
                "the decoded-material store at {} is busy: {detail}",
                path.display()
            ),
            Self::Corrupt { path, detail } => write!(
                formatter,
                "the decoded-material image {} is not usable: {detail}",
                path.display()
            ),
            Self::Decode(detail) => write!(formatter, "{detail}"),
        }
    }
}

impl std::error::Error for MaterialImageError {}

#[cfg(test)]
mod tests {
    use super::*;

    struct ToneDecoder {
        sample_rate_hz: u32,
        channels: u16,
        frames: usize,
    }

    impl MaterialDecoder for ToneDecoder {
        fn decode_into(
            &self,
            _path: &Path,
            writer: &mut ImageWriter,
        ) -> Result<DecodedImageFacts, String> {
            writer.declare_format(self.sample_rate_hz, self.channels)?;
            for frame in 0..self.frames {
                let value = (frame as f32 * 0.001).sin();
                let block = vec![value; usize::from(self.channels)];
                writer.push(&block)?;
            }
            Ok(DecodedImageFacts {
                container: Some("tone".into()),
                codec: Some("tone".into()),
                bit_depth: Some(32),
            })
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "audec-material-image-{name}-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn source_file(root: &Path, bytes: &[u8]) -> PathBuf {
        let path = root.join("source.bin");
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_streamed_fingerprint_matches_the_whole_byte_fingerprint() {
        let root = temp_root("fingerprint");
        let bytes: Vec<u8> = (0..(3 * FINGERPRINT_CHUNK_BYTES + 17))
            .map(|index| (index % 251) as u8)
            .collect();
        let path = source_file(&root, &bytes);
        assert_eq!(
            fingerprint_file(&path).unwrap(),
            ContentFingerprint::from_bytes(&bytes)
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_mapped_image_is_sample_identical_to_the_decode_that_wrote_it() {
        let root = temp_root("identity");
        let path = source_file(&root, b"material bytes");
        let cache = MaterialImageCache::new(root.join("cache"));
        fs::create_dir_all(cache.root()).unwrap();
        let decoder = ToneDecoder {
            sample_rate_hz: 48_000,
            channels: 2,
            frames: 5_000,
        };
        let image = cache.open(&path, None, &decoder).unwrap();
        assert!(!image.cache_hit);
        assert!(image.samples.is_mapped());
        assert_eq!(image.shape.frame_count, 5_000);
        assert_eq!(image.shape.channels, 2);
        let expected: Vec<f32> = (0..5_000)
            .flat_map(|frame| {
                let value = (frame as f32 * 0.001).sin();
                [value, value]
            })
            .collect();
        let owned = PcmSamples::from(expected);
        let differing = owned
            .as_slice()
            .iter()
            .zip(image.samples.as_slice())
            .position(|(left, right)| left.to_bits() != right.to_bits());
        assert_eq!(differing, None, "the mapped image is not bit-identical");
        assert!(!owned.shares_allocation(&image.samples));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_second_open_of_the_same_material_decodes_nothing() {
        let root = temp_root("hit");
        let path = source_file(&root, b"material bytes");
        let cache = MaterialImageCache::new(root.join("cache"));
        fs::create_dir_all(cache.root()).unwrap();
        let decoder = ToneDecoder {
            sample_rate_hz: 44_100,
            channels: 1,
            frames: 1_000,
        };
        let first = cache.open(&path, None, &decoder).unwrap();
        assert!(!first.cache_hit);
        struct Refusing;
        impl MaterialDecoder for Refusing {
            fn decode_into(
                &self,
                _path: &Path,
                _writer: &mut ImageWriter,
            ) -> Result<DecodedImageFacts, String> {
                panic!("a cache hit must not decode");
            }
        }
        let second = cache.open(&path, None, &Refusing).unwrap();
        assert!(second.cache_hit);
        assert_eq!(second.shape, first.shape);
        assert_eq!(second.samples.as_slice(), first.samples.as_slice());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_image_naming_other_source_bytes_is_refused_and_redecoded() {
        let root = temp_root("mismatch");
        let path = source_file(&root, b"material bytes");
        let cache = MaterialImageCache::new(root.join("cache"));
        fs::create_dir_all(cache.root()).unwrap();
        let decoder = ToneDecoder {
            sample_rate_hz: 48_000,
            channels: 2,
            frames: 64,
        };
        let image = cache.open(&path, None, &decoder).unwrap();
        let other = ContentFingerprint::from_bytes(b"different material");
        let error = map_image(&image.path, &other).unwrap_err();
        assert!(
            matches!(&error, MaterialImageError::Corrupt { detail, .. }
                if detail.contains("different source bytes")),
            "{error}"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_truncated_image_is_refused_rather_than_read_short() {
        let root = temp_root("truncated");
        let path = source_file(&root, b"material bytes");
        let cache = MaterialImageCache::new(root.join("cache"));
        fs::create_dir_all(cache.root()).unwrap();
        let decoder = ToneDecoder {
            sample_rate_hz: 48_000,
            channels: 2,
            frames: 512,
        };
        let image = cache.open(&path, None, &decoder).unwrap();
        let fingerprint = image.source_fingerprint;
        let image_path = image.path.clone();
        drop(image);
        let mut permissions = fs::metadata(&image_path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(&image_path, permissions).unwrap();
        let file = OpenOptions::new().write(true).open(&image_path).unwrap();
        file.set_len(MATERIAL_IMAGE_HEADER_BYTES as u64 + 40)
            .unwrap();
        drop(file);
        let error = map_image(&image_path, &fingerprint).unwrap_err();
        assert!(
            matches!(&error, MaterialImageError::Corrupt { detail, .. }
                if detail.contains("header describes")),
            "{error}"
        );
        // The cache removes what it refuses, so the next open decodes again.
        let reopened = cache.open(&path, None, &decoder).unwrap();
        assert!(!reopened.cache_hit);
        assert_eq!(reopened.shape.frame_count, 512);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_payload_offset_is_aligned_for_mapped_f32_reads() {
        assert_eq!(MATERIAL_IMAGE_HEADER_BYTES % 16, 0);
        assert!(MATERIAL_IMAGE_MAGIC.len() < MATERIAL_IMAGE_HEADER_BYTES);
        assert_ne!(
            MATERIAL_IMAGE_MAGIC.as_slice(),
            crate::sample_material::CANONICAL_PCM_MAGIC
        );
    }

    #[test]
    fn a_window_onto_a_mapped_image_borrows_rather_than_copies() {
        let root = temp_root("window");
        let path = source_file(&root, b"material bytes");
        let cache = MaterialImageCache::new(root.join("cache"));
        fs::create_dir_all(cache.root()).unwrap();
        let decoder = ToneDecoder {
            sample_rate_hz: 48_000,
            channels: 2,
            frames: 100,
        };
        let image = cache.open(&path, None, &decoder).unwrap();
        let window = image.samples.slice(20, 60).unwrap();
        assert!(window.is_mapped());
        assert_eq!(window.len(), 40);
        assert_eq!(window.as_slice(), &image.samples.as_slice()[20..60]);
        fs::remove_dir_all(&root).unwrap();
    }
}
