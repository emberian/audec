//! Cryptographic, schema-tagged identity for immutable Audec content.
//!
//! Equal bytes do not imply equal meaning. Encoded media, canonical decoded
//! PCM, recipes, derived products, and runtime observations occupy distinct
//! schema domains. This module names and hashes those domains; it never claims
//! that two decodes, analyses, or renders are semantically equivalent merely
//! because a cache key happens to agree.
//!
//! The SHA-256 core is centralized from the existing private implementations
//! in `artifact_catalog`, `model_registry`, `model_store`, and
//! `render_runtime`. Consumers should migrate here rather than adding another.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io::{self, Read};

const CONTENT_PREAMBLE: &[u8] = b"audec:schema-content:v1\0";
const PRODUCT_KEY_SCHEMA_NAME: &str = "audec.product-key";
const MAX_LABEL_BYTES: usize = 160;

/// Semantic class of canonical bytes. Discriminants are durable preimage
/// values and must never be reordered or reused.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ContentClass {
    CanonicalEncodedMedia = 1,
    CanonicalDecodedPcm = 2,
    Recipe = 3,
    RenderProduct = 4,
    AnalysisArtifact = 5,
    ModelArtifact = 6,
    ReadingAttachment = 7,
    RuntimeObservation = 8,
    ProductKey = 9,
    Extension = 255,
}

impl ContentClass {
    pub const fn storage_label(self) -> &'static str {
        match self {
            Self::CanonicalEncodedMedia => "encoded-media",
            Self::CanonicalDecodedPcm => "decoded-pcm",
            Self::Recipe => "recipe",
            Self::RenderProduct => "render-product",
            Self::AnalysisArtifact => "analysis-artifact",
            Self::ModelArtifact => "model-artifact",
            Self::ReadingAttachment => "reading-attachment",
            Self::RuntimeObservation => "runtime-observation",
            Self::ProductKey => "product-key",
            Self::Extension => "extension",
        }
    }

    pub fn from_storage_label(value: &str) -> Option<Self> {
        Some(match value {
            "encoded-media" => Self::CanonicalEncodedMedia,
            "decoded-pcm" => Self::CanonicalDecodedPcm,
            "recipe" => Self::Recipe,
            "render-product" => Self::RenderProduct,
            "analysis-artifact" => Self::AnalysisArtifact,
            "model-artifact" => Self::ModelArtifact,
            "reading-attachment" => Self::ReadingAttachment,
            "runtime-observation" => Self::RuntimeObservation,
            "product-key" => Self::ProductKey,
            "extension" => Self::Extension,
            _ => return None,
        })
    }

    pub(crate) fn from_discriminant(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::CanonicalEncodedMedia,
            2 => Self::CanonicalDecodedPcm,
            3 => Self::Recipe,
            4 => Self::RenderProduct,
            5 => Self::AnalysisArtifact,
            6 => Self::ModelArtifact,
            7 => Self::ReadingAttachment,
            8 => Self::RuntimeObservation,
            9 => Self::ProductKey,
            255 => Self::Extension,
            _ => return None,
        })
    }
}

/// Versioned canonicalization contract. `version` changes whenever exact
/// canonical bytes change, not merely when implementation code changes.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SchemaTag {
    class: ContentClass,
    name: String,
    version: u32,
}

impl SchemaTag {
    pub fn new(
        class: ContentClass,
        name: impl Into<String>,
        version: u32,
    ) -> Result<Self, IdentityError> {
        let name = name.into();
        validate_label("schema name", &name)?;
        if version == 0 {
            return Err(IdentityError::ZeroSchemaVersion);
        }
        if class == ContentClass::ProductKey && name != PRODUCT_KEY_SCHEMA_NAME {
            return Err(IdentityError::ReservedProductKeySchema(name));
        }
        Ok(Self {
            class,
            name,
            version,
        })
    }

    pub fn encoded_media(name: impl Into<String>, version: u32) -> Result<Self, IdentityError> {
        Self::new(ContentClass::CanonicalEncodedMedia, name, version)
    }

    pub fn decoded_pcm(name: impl Into<String>, version: u32) -> Result<Self, IdentityError> {
        Self::new(ContentClass::CanonicalDecodedPcm, name, version)
    }

    pub fn recipe(name: impl Into<String>, version: u32) -> Result<Self, IdentityError> {
        Self::new(ContentClass::Recipe, name, version)
    }

    pub fn render_product(name: impl Into<String>, version: u32) -> Result<Self, IdentityError> {
        Self::new(ContentClass::RenderProduct, name, version)
    }

    pub fn analysis_artifact(name: impl Into<String>, version: u32) -> Result<Self, IdentityError> {
        Self::new(ContentClass::AnalysisArtifact, name, version)
    }

    pub fn model_artifact(name: impl Into<String>, version: u32) -> Result<Self, IdentityError> {
        Self::new(ContentClass::ModelArtifact, name, version)
    }

    pub fn reading_attachment(
        name: impl Into<String>,
        version: u32,
    ) -> Result<Self, IdentityError> {
        Self::new(ContentClass::ReadingAttachment, name, version)
    }

    pub fn runtime_observation(
        name: impl Into<String>,
        version: u32,
    ) -> Result<Self, IdentityError> {
        Self::new(ContentClass::RuntimeObservation, name, version)
    }

    pub fn extension(name: impl Into<String>, version: u32) -> Result<Self, IdentityError> {
        Self::new(ContentClass::Extension, name, version)
    }

    fn product_key() -> Self {
        Self {
            class: ContentClass::ProductKey,
            name: PRODUCT_KEY_SCHEMA_NAME.into(),
            version: 1,
        }
    }

    pub const fn class(&self) -> ContentClass {
        self.class
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub const fn version(&self) -> u32 {
        self.version
    }

    pub fn encode_canonical(&self, output: &mut Vec<u8>) {
        output.push(self.class as u8);
        push_bytes(output, self.name.as_bytes());
        output.extend_from_slice(&self.version.to_le_bytes());
    }

    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, IdentityError> {
        let mut input = CanonicalInput::new(bytes);
        let class = ContentClass::from_discriminant(input.byte()?)
            .ok_or_else(|| IdentityError::MalformedCanonical("unknown content class".into()))?;
        let name = String::from_utf8(input.bytes()?.to_vec())
            .map_err(|_| IdentityError::MalformedCanonical("schema name is not UTF-8".into()))?;
        let version = input.u32()?;
        input.finish()?;
        Self::new(class, name, version)
    }
}

impl fmt::Display for SchemaTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}@{}",
            self.class.storage_label(),
            self.name,
            self.version
        )
    }
}

#[derive(Clone, Copy, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    pub const ZERO: Self = Self([0; 32]);
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    pub const fn bytes(self) -> [u8; 32] {
        self.0
    }

    pub fn to_hex(self) -> String {
        let mut output = String::with_capacity(64);
        use std::fmt::Write as _;
        for byte in self.0 {
            write!(&mut output, "{byte:02x}").unwrap();
        }
        output
    }

    pub fn from_hex(value: &str) -> Result<Self, IdentityError> {
        if value.len() != 64 {
            return Err(IdentityError::InvalidDigestHex(value.into()));
        }
        let mut bytes = [0; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
        }
        Ok(Self(bytes))
    }

    /// Raw SHA-256 for external formats whose established contract hashes bare
    /// bytes. New Audec identities should use schema-tagged [`Digest`].
    pub fn hash_raw(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self(hasher.finalize())
    }

    /// Raw SHA-256 over a sequence of byte parts without allocating their
    /// concatenation. This exists for established external/domain formats;
    /// new Audec objects should use schema-tagged [`Digest`].
    pub fn hash_raw_parts(parts: &[&[u8]]) -> Self {
        let mut hasher = Sha256::new();
        for part in parts {
            hasher.update(part);
        }
        Self(hasher.finalize())
    }

    /// Stream a raw SHA-256 used by formats which already define a bare-byte
    /// digest. Returns the number of bytes committed to the digest.
    pub fn hash_raw_reader(mut reader: impl Read) -> io::Result<(Self, u64)> {
        let mut hasher = Sha256::new();
        let mut byte_len = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                return Ok((Self(hasher.finalize()), byte_len));
            }
            byte_len = byte_len
                .checked_add(count as u64)
                .ok_or_else(|| io::Error::other("SHA-256 input length overflow"))?;
            hasher.update(&buffer[..count]);
        }
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256Digest({self})")
    }
}
impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Strong identity of canonical bytes in one exact schema domain.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Digest {
    schema: SchemaTag,
    sha256: Sha256Digest,
}

impl Digest {
    pub const ALGORITHM: &'static str = "sha256";
    pub fn from_verified_parts(schema: SchemaTag, sha256: Sha256Digest) -> Self {
        Self { schema, sha256 }
    }

    pub fn of_bytes(schema: SchemaTag, bytes: &[u8]) -> Self {
        let mut hasher = SchemaHasher::new(schema.clone());
        hasher.update(bytes);
        Self {
            schema,
            sha256: hasher.finish(),
        }
    }

    pub fn of_reader(
        schema: SchemaTag,
        mut reader: impl Read,
    ) -> Result<(Self, u64), IdentityError> {
        let mut hasher = SchemaHasher::new(schema.clone());
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = reader.read(&mut buffer).map_err(IdentityError::Io)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        let byte_len = hasher.byte_len();
        Ok((
            Self {
                schema,
                sha256: hasher.finish(),
            },
            byte_len,
        ))
    }

    pub fn verify(&self, bytes: &[u8]) -> bool {
        Self::of_bytes(self.schema.clone(), bytes) == *self
    }
    pub fn schema(&self) -> &SchemaTag {
        &self.schema
    }
    pub const fn sha256(&self) -> Sha256Digest {
        self.sha256
    }

    pub fn encode_canonical(&self) -> Vec<u8> {
        let mut output = Vec::new();
        encode_digest(&mut output, self);
        output
    }

    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, IdentityError> {
        let mut input = CanonicalInput::new(bytes);
        let digest = decode_digest(&mut input)?;
        input.finish()?;
        Ok(digest)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", Self::ALGORITHM, self.schema, self.sha256)
    }
}

/// Streaming schema hasher. Length is committed as a trailer so multi-GB
/// objects need not be buffered merely to prefix their payload length.
pub struct SchemaHasher {
    hasher: Sha256,
    byte_len: u64,
}

impl SchemaHasher {
    pub fn new(schema: SchemaTag) -> Self {
        let mut encoded = Vec::new();
        schema.encode_canonical(&mut encoded);
        let mut hasher = Sha256::new();
        hasher.update(CONTENT_PREAMBLE);
        hasher.update(&(encoded.len() as u64).to_le_bytes());
        hasher.update(&encoded);
        Self {
            hasher,
            byte_len: 0,
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.byte_len = self
            .byte_len
            .checked_add(bytes.len() as u64)
            .expect("one content object cannot exceed u64::MAX bytes");
        self.hasher.update(bytes);
    }
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }
    pub fn finish(mut self) -> Sha256Digest {
        self.hasher.update(&self.byte_len.to_le_bytes());
        Sha256Digest(self.hasher.finalize())
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DependencySlot {
    pub role: String,
    pub slot: u32,
}

impl DependencySlot {
    pub fn new(role: impl Into<String>, slot: u32) -> Result<Self, IdentityError> {
        let role = role.into();
        validate_label("dependency role", &role)?;
        Ok(Self { role, slot })
    }
}

/// Runtime state cannot be smuggled in as media or a recipe. Its observation
/// digest names exact runtime facts; generation makes replacement explicit.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RuntimeDependency {
    pub provider: String,
    pub generation: u64,
    pub observation: Digest,
}

impl RuntimeDependency {
    pub fn new(
        provider: impl Into<String>,
        generation: u64,
        observation: Digest,
    ) -> Result<Self, IdentityError> {
        let provider = provider.into();
        validate_label("runtime dependency provider", &provider)?;
        require_class(&observation, ContentClass::RuntimeObservation)?;
        Ok(Self {
            provider,
            generation,
            observation,
        })
    }
}

/// Identity of a pure product request. Output bytes are separately addressed;
/// this says what was requested, not what happened to be produced.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProductKey {
    output_schema: SchemaTag,
    recipe: Digest,
    encoded_media: BTreeMap<DependencySlot, Digest>,
    decoded_pcm: BTreeMap<DependencySlot, Digest>,
    products: BTreeMap<DependencySlot, Digest>,
    artifacts: BTreeMap<DependencySlot, Digest>,
    runtime: BTreeMap<DependencySlot, RuntimeDependency>,
    digest: Digest,
}

impl ProductKey {
    pub fn builder(
        output_schema: SchemaTag,
        recipe: Digest,
    ) -> Result<ProductKeyBuilder, IdentityError> {
        require_class(&recipe, ContentClass::Recipe)?;
        if matches!(
            output_schema.class,
            ContentClass::CanonicalEncodedMedia
                | ContentClass::CanonicalDecodedPcm
                | ContentClass::Recipe
                | ContentClass::RuntimeObservation
                | ContentClass::ProductKey
        ) {
            return Err(IdentityError::InvalidProductOutput(output_schema.class));
        }
        Ok(ProductKeyBuilder {
            output_schema,
            recipe,
            encoded_media: BTreeMap::new(),
            decoded_pcm: BTreeMap::new(),
            products: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            runtime: BTreeMap::new(),
        })
    }
    pub fn output_schema(&self) -> &SchemaTag {
        &self.output_schema
    }
    pub fn recipe(&self) -> &Digest {
        &self.recipe
    }
    pub fn encoded_media(&self) -> &BTreeMap<DependencySlot, Digest> {
        &self.encoded_media
    }
    pub fn decoded_pcm(&self) -> &BTreeMap<DependencySlot, Digest> {
        &self.decoded_pcm
    }
    pub fn products(&self) -> &BTreeMap<DependencySlot, Digest> {
        &self.products
    }
    pub fn artifacts(&self) -> &BTreeMap<DependencySlot, Digest> {
        &self.artifacts
    }
    pub fn runtime(&self) -> &BTreeMap<DependencySlot, RuntimeDependency> {
        &self.runtime
    }
    pub fn digest(&self) -> &Digest {
        &self.digest
    }

    /// Canonical dependency manifest. The product-key digest commits to these
    /// exact bytes, so this form is suitable for durable receipts and restart
    /// hydration rather than a Debug-derived cache key.
    pub fn encode_canonical(&self) -> Vec<u8> {
        encode_product_key_fields(
            &self.output_schema,
            &self.recipe,
            &self.encoded_media,
            &self.decoded_pcm,
            &self.products,
            &self.artifacts,
            &self.runtime,
        )
    }

    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, IdentityError> {
        let mut input = CanonicalInput::new(bytes);
        let output_schema = decode_schema_inline(&mut input)?;
        let recipe = decode_digest(&mut input)?;
        let encoded_media = decode_digest_map(&mut input)?;
        let decoded_pcm = decode_digest_map(&mut input)?;
        let products = decode_digest_map(&mut input)?;
        let artifacts = decode_digest_map(&mut input)?;
        let runtime_count = input.usize_len()?;
        let mut runtime = BTreeMap::new();
        for _ in 0..runtime_count {
            let slot = decode_slot(&mut input)?;
            let provider = input.string()?;
            let generation = input.u64()?;
            let observation = decode_digest(&mut input)?;
            let dependency = RuntimeDependency::new(provider, generation, observation)?;
            if runtime.insert(slot.clone(), dependency).is_some() {
                return Err(IdentityError::DuplicateDependency(slot));
            }
        }
        input.finish()?;

        let mut builder = ProductKey::builder(output_schema, recipe)?;
        for (slot, digest) in encoded_media {
            builder = builder.encoded_media(slot, digest)?;
        }
        for (slot, digest) in decoded_pcm {
            builder = builder.decoded_pcm(slot, digest)?;
        }
        for (slot, digest) in products {
            builder = builder.product(slot, digest)?;
        }
        for (slot, digest) in artifacts {
            builder = builder.artifact(slot, digest)?;
        }
        for (slot, dependency) in runtime {
            builder = builder.runtime(slot, dependency)?;
        }
        Ok(builder.build())
    }
}

pub struct ProductKeyBuilder {
    output_schema: SchemaTag,
    recipe: Digest,
    encoded_media: BTreeMap<DependencySlot, Digest>,
    decoded_pcm: BTreeMap<DependencySlot, Digest>,
    products: BTreeMap<DependencySlot, Digest>,
    artifacts: BTreeMap<DependencySlot, Digest>,
    runtime: BTreeMap<DependencySlot, RuntimeDependency>,
}

impl ProductKeyBuilder {
    pub fn encoded_media(
        mut self,
        slot: DependencySlot,
        digest: Digest,
    ) -> Result<Self, IdentityError> {
        require_class(&digest, ContentClass::CanonicalEncodedMedia)?;
        insert_unique(&mut self.encoded_media, slot, digest)?;
        Ok(self)
    }
    pub fn decoded_pcm(
        mut self,
        slot: DependencySlot,
        digest: Digest,
    ) -> Result<Self, IdentityError> {
        require_class(&digest, ContentClass::CanonicalDecodedPcm)?;
        insert_unique(&mut self.decoded_pcm, slot, digest)?;
        Ok(self)
    }
    pub fn product(mut self, slot: DependencySlot, digest: Digest) -> Result<Self, IdentityError> {
        require_class(&digest, ContentClass::RenderProduct)?;
        insert_unique(&mut self.products, slot, digest)?;
        Ok(self)
    }
    pub fn artifact(mut self, slot: DependencySlot, digest: Digest) -> Result<Self, IdentityError> {
        if !matches!(
            digest.schema.class,
            ContentClass::AnalysisArtifact
                | ContentClass::ModelArtifact
                | ContentClass::ReadingAttachment
        ) {
            return Err(IdentityError::InvalidArtifactClass(digest.schema.class));
        }
        insert_unique(&mut self.artifacts, slot, digest)?;
        Ok(self)
    }
    pub fn runtime(
        mut self,
        slot: DependencySlot,
        dependency: RuntimeDependency,
    ) -> Result<Self, IdentityError> {
        insert_unique(&mut self.runtime, slot, dependency)?;
        Ok(self)
    }
    pub fn build(self) -> ProductKey {
        let bytes = encode_product_key_fields(
            &self.output_schema,
            &self.recipe,
            &self.encoded_media,
            &self.decoded_pcm,
            &self.products,
            &self.artifacts,
            &self.runtime,
        );
        let digest = Digest::of_bytes(SchemaTag::product_key(), &bytes);
        ProductKey {
            output_schema: self.output_schema,
            recipe: self.recipe,
            encoded_media: self.encoded_media,
            decoded_pcm: self.decoded_pcm,
            products: self.products,
            artifacts: self.artifacts,
            runtime: self.runtime,
            digest,
        }
    }
}

#[derive(Debug)]
pub enum IdentityError {
    InvalidLabel {
        field: &'static str,
        value: String,
    },
    ZeroSchemaVersion,
    ReservedProductKeySchema(String),
    InvalidDigestHex(String),
    WrongContentClass {
        expected: ContentClass,
        actual: ContentClass,
    },
    InvalidArtifactClass(ContentClass),
    InvalidProductOutput(ContentClass),
    DuplicateDependency(DependencySlot),
    MalformedCanonical(String),
    Io(io::Error),
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLabel { field, value } => write!(f, "invalid {field}: {value:?}"),
            Self::ZeroSchemaVersion => f.write_str("schema version must be nonzero"),
            Self::ReservedProductKeySchema(name) => write!(
                f,
                "product-key schemas must use reserved name {PRODUCT_KEY_SCHEMA_NAME:?}, got {name:?}"
            ),
            Self::InvalidDigestHex(value) => write!(f, "invalid SHA-256 hex digest: {value:?}"),
            Self::WrongContentClass { expected, actual } => {
                write!(f, "content class {actual:?}, expected {expected:?}")
            }
            Self::InvalidArtifactClass(class) => write!(f, "{class:?} is not an artifact class"),
            Self::InvalidProductOutput(class) => write!(f, "{class:?} cannot be a product output"),
            Self::DuplicateDependency(slot) => write!(f, "duplicate product dependency {slot:?}"),
            Self::MalformedCanonical(detail) => write!(f, "malformed canonical identity: {detail}"),
            Self::Io(error) => error.fmt(f),
        }
    }
}
impl Error for IdentityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        if let Self::Io(error) = self {
            Some(error)
        } else {
            None
        }
    }
}

fn require_class(digest: &Digest, expected: ContentClass) -> Result<(), IdentityError> {
    if digest.schema.class != expected {
        return Err(IdentityError::WrongContentClass {
            expected,
            actual: digest.schema.class,
        });
    }
    Ok(())
}
fn insert_unique<T>(
    values: &mut BTreeMap<DependencySlot, T>,
    slot: DependencySlot,
    value: T,
) -> Result<(), IdentityError> {
    if values.insert(slot.clone(), value).is_some() {
        return Err(IdentityError::DuplicateDependency(slot));
    }
    Ok(())
}
fn encode_digest(output: &mut Vec<u8>, digest: &Digest) {
    let mut schema = Vec::new();
    digest.schema.encode_canonical(&mut schema);
    push_bytes(output, &schema);
    output.extend_from_slice(&digest.sha256.bytes());
}

fn decode_schema(input: &mut CanonicalInput<'_>) -> Result<SchemaTag, IdentityError> {
    SchemaTag::decode_canonical(input.bytes()?)
}

fn decode_schema_inline(input: &mut CanonicalInput<'_>) -> Result<SchemaTag, IdentityError> {
    let class = ContentClass::from_discriminant(input.byte()?)
        .ok_or_else(|| IdentityError::MalformedCanonical("unknown content class".into()))?;
    let name = input.string()?;
    let version = input.u32()?;
    SchemaTag::new(class, name, version)
}

fn decode_digest(input: &mut CanonicalInput<'_>) -> Result<Digest, IdentityError> {
    let schema = decode_schema(input)?;
    let sha256 = Sha256Digest::from_bytes(input.array_32()?);
    Ok(Digest::from_verified_parts(schema, sha256))
}
fn encode_slot(output: &mut Vec<u8>, slot: &DependencySlot) {
    push_bytes(output, slot.role.as_bytes());
    output.extend_from_slice(&slot.slot.to_le_bytes());
}
fn encode_digest_map(output: &mut Vec<u8>, values: &BTreeMap<DependencySlot, Digest>) {
    output.extend_from_slice(&(values.len() as u64).to_le_bytes());
    for (slot, digest) in values {
        encode_slot(output, slot);
        encode_digest(output, digest);
    }
}

fn decode_slot(input: &mut CanonicalInput<'_>) -> Result<DependencySlot, IdentityError> {
    let role = input.string()?;
    let slot = input.u32()?;
    DependencySlot::new(role, slot)
}

fn decode_digest_map(
    input: &mut CanonicalInput<'_>,
) -> Result<BTreeMap<DependencySlot, Digest>, IdentityError> {
    let count = input.usize_len()?;
    let mut values = BTreeMap::new();
    for _ in 0..count {
        let slot = decode_slot(input)?;
        let digest = decode_digest(input)?;
        if values.insert(slot.clone(), digest).is_some() {
            return Err(IdentityError::DuplicateDependency(slot));
        }
    }
    Ok(values)
}

fn encode_product_key_fields(
    output_schema: &SchemaTag,
    recipe: &Digest,
    encoded_media: &BTreeMap<DependencySlot, Digest>,
    decoded_pcm: &BTreeMap<DependencySlot, Digest>,
    products: &BTreeMap<DependencySlot, Digest>,
    artifacts: &BTreeMap<DependencySlot, Digest>,
    runtime: &BTreeMap<DependencySlot, RuntimeDependency>,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    output_schema.encode_canonical(&mut bytes);
    encode_digest(&mut bytes, recipe);
    encode_digest_map(&mut bytes, encoded_media);
    encode_digest_map(&mut bytes, decoded_pcm);
    encode_digest_map(&mut bytes, products);
    encode_digest_map(&mut bytes, artifacts);
    bytes.extend_from_slice(&(runtime.len() as u64).to_le_bytes());
    for (slot, dependency) in runtime {
        encode_slot(&mut bytes, slot);
        push_bytes(&mut bytes, dependency.provider.as_bytes());
        bytes.extend_from_slice(&dependency.generation.to_le_bytes());
        encode_digest(&mut bytes, &dependency.observation);
    }
    bytes
}
fn push_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    output.extend_from_slice(bytes);
}

fn validate_label(field: &'static str, value: &str) -> Result<(), IdentityError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_LABEL_BYTES
        && value.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(b, b'.' | b'-' | b'_' | b'/' | b':')
        })
        && !value.starts_with(['.', '/', ':', '-'])
        && !value.ends_with(['.', '/', ':', '-'])
        && !value.contains("..")
        && !value.contains("//");
    if !valid {
        return Err(IdentityError::InvalidLabel {
            field,
            value: value.into(),
        });
    }
    Ok(())
}
fn hex_nibble(value: u8) -> Result<u8, IdentityError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(IdentityError::InvalidDigestHex(
            String::from_utf8_lossy(&[value]).into_owned(),
        )),
    }
}

struct CanonicalInput<'a> {
    bytes: &'a [u8],
    cursor: usize,
}
impl<'a> CanonicalInput<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }
    fn byte(&mut self) -> Result<u8, IdentityError> {
        let value = *self
            .bytes
            .get(self.cursor)
            .ok_or_else(|| IdentityError::MalformedCanonical("unexpected end".into()))?;
        self.cursor += 1;
        Ok(value)
    }
    fn u32(&mut self) -> Result<u32, IdentityError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, IdentityError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn usize_len(&mut self) -> Result<usize, IdentityError> {
        usize::try_from(self.u64()?)
            .map_err(|_| IdentityError::MalformedCanonical("length overflow".into()))
    }
    fn bytes(&mut self) -> Result<&'a [u8], IdentityError> {
        let n = self.usize_len()?;
        self.take(n)
    }
    fn string(&mut self) -> Result<String, IdentityError> {
        String::from_utf8(self.bytes()?.to_vec())
            .map_err(|_| IdentityError::MalformedCanonical("string is not UTF-8".into()))
    }
    fn array_32(&mut self) -> Result<[u8; 32], IdentityError> {
        Ok(self.take(32)?.try_into().expect("exact digest bytes"))
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], IdentityError> {
        let end = self
            .cursor
            .checked_add(n)
            .ok_or_else(|| IdentityError::MalformedCanonical("offset overflow".into()))?;
        let value = self
            .bytes
            .get(self.cursor..end)
            .ok_or_else(|| IdentityError::MalformedCanonical("unexpected end".into()))?;
        self.cursor = end;
        Ok(value)
    }
    fn finish(self) -> Result<(), IdentityError> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err(IdentityError::MalformedCanonical("trailing bytes".into()))
        }
    }
}

struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffered: usize,
    bytes: u64,
}
impl Sha256 {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    fn new() -> Self {
        Self {
            state: Self::INITIAL,
            buffer: [0; 64],
            buffered: 0,
            bytes: 0,
        }
    }
    fn update(&mut self, mut bytes: &[u8]) {
        self.bytes = self.bytes.wrapping_add(bytes.len() as u64);
        if self.buffered > 0 {
            let n = (64 - self.buffered).min(bytes.len());
            self.buffer[self.buffered..self.buffered + n].copy_from_slice(&bytes[..n]);
            self.buffered += n;
            bytes = &bytes[n..];
            if self.buffered == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            } else {
                return;
            }
        }
        while bytes.len() >= 64 {
            let block: &[u8; 64] = bytes[..64].try_into().unwrap();
            self.compress(block);
            bytes = &bytes[64..];
        }
        self.buffer[..bytes.len()].copy_from_slice(bytes);
        self.buffered = bytes.len();
    }
    fn finalize(mut self) -> [u8; 32] {
        let bits = self.bytes.wrapping_mul(8);
        self.buffer[self.buffered] = 0x80;
        self.buffered += 1;
        if self.buffered > 56 {
            self.buffer[self.buffered..].fill(0);
            let block = self.buffer;
            self.compress(&block);
            self.buffer = [0; 64];
        } else {
            self.buffer[self.buffered..56].fill(0);
        }
        self.buffer[56..].copy_from_slice(&bits.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);
        let mut out = [0; 32];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, c) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(c.try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(Self::K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (target, add) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *target = target.wrapping_add(add);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_boundary_matches_standard_vectors() {
        assert_eq!(
            Sha256Digest::hash_raw(b"").to_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            Sha256Digest::hash_raw(b"abc").to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            Sha256Digest::from_hex(
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            )
            .unwrap(),
            Sha256Digest::hash_raw(b"abc")
        );
    }

    #[test]
    fn schema_and_chunk_boundaries_are_identity() {
        let encoded = SchemaTag::encoded_media("flac/file-bytes", 1).unwrap();
        let pcm = SchemaTag::decoded_pcm("f32le/interleaved", 1).unwrap();
        assert_ne!(
            Digest::of_bytes(encoded.clone(), b"same"),
            Digest::of_bytes(pcm, b"same")
        );
        let whole = Digest::of_bytes(encoded.clone(), b"abcdef");
        let mut chunked = SchemaHasher::new(encoded.clone());
        chunked.update(b"ab");
        chunked.update(b"");
        chunked.update(b"cdef");
        assert_eq!(whole.sha256(), chunked.finish());
        let mut canonical = Vec::new();
        encoded.encode_canonical(&mut canonical);
        assert_eq!(SchemaTag::decode_canonical(&canonical).unwrap(), encoded);
    }

    #[test]
    fn product_keys_are_order_independent_but_runtime_exact() {
        let recipe = Digest::of_bytes(SchemaTag::recipe("render/engine", 3).unwrap(), b"recipe");
        let encoded = Digest::of_bytes(
            SchemaTag::encoded_media("flac/file-bytes", 1).unwrap(),
            b"flac",
        );
        let pcm = Digest::of_bytes(
            SchemaTag::decoded_pcm("f32le/interleaved", 1).unwrap(),
            b"pcm",
        );
        let observation = Digest::of_bytes(
            SchemaTag::runtime_observation("plugin/process-state", 1).unwrap(),
            b"state",
        );
        let output = SchemaTag::render_product("master/f32le", 1).unwrap();
        let source = DependencySlot::new("source", 0).unwrap();
        let materialized = DependencySlot::new("materialized", 0).unwrap();
        let plugin = DependencySlot::new("plugin", 7).unwrap();
        let first = ProductKey::builder(output.clone(), recipe.clone())
            .unwrap()
            .encoded_media(source.clone(), encoded.clone())
            .unwrap()
            .decoded_pcm(materialized.clone(), pcm.clone())
            .unwrap()
            .runtime(
                plugin.clone(),
                RuntimeDependency::new("clap-worker", 8, observation.clone()).unwrap(),
            )
            .unwrap()
            .build();
        let second = ProductKey::builder(output.clone(), recipe.clone())
            .unwrap()
            .runtime(
                plugin.clone(),
                RuntimeDependency::new("clap-worker", 8, observation.clone()).unwrap(),
            )
            .unwrap()
            .decoded_pcm(materialized, pcm)
            .unwrap()
            .encoded_media(source, encoded)
            .unwrap()
            .build();
        assert_eq!(first.digest(), second.digest());
        let changed = ProductKey::builder(output, recipe)
            .unwrap()
            .runtime(
                plugin,
                RuntimeDependency::new("clap-worker", 9, observation).unwrap(),
            )
            .unwrap()
            .build();
        assert_ne!(first.digest(), changed.digest());
    }

    #[test]
    fn dependencies_cannot_cross_semantic_classes() {
        let recipe = Digest::of_bytes(SchemaTag::recipe("analysis", 1).unwrap(), b"r");
        let encoded =
            Digest::of_bytes(SchemaTag::encoded_media("wav/file-bytes", 1).unwrap(), b"x");
        let builder =
            ProductKey::builder(SchemaTag::analysis_artifact("onsets", 1).unwrap(), recipe)
                .unwrap();
        assert!(matches!(
            builder.decoded_pcm(DependencySlot::new("audio", 0).unwrap(), encoded),
            Err(IdentityError::WrongContentClass { .. })
        ));
    }

    #[test]
    fn product_dependency_manifest_round_trips_canonically() {
        let output = SchemaTag::render_product("test/render", 1).unwrap();
        let recipe_schema = SchemaTag::recipe("test/recipe", 1).unwrap();
        let media_schema = SchemaTag::encoded_media("test/flac", 1).unwrap();
        let key = ProductKey::builder(output, Digest::of_bytes(recipe_schema, b"v1"))
            .unwrap()
            .encoded_media(
                DependencySlot::new("source", 0).unwrap(),
                Digest::of_bytes(media_schema, b"encoded"),
            )
            .unwrap()
            .build();
        let bytes = key.encode_canonical();
        let reopened = ProductKey::decode_canonical(&bytes).unwrap();
        assert_eq!(reopened, key);
        assert_eq!(reopened.encode_canonical(), bytes);
        let mut malformed = bytes;
        malformed.push(0);
        assert!(ProductKey::decode_canonical(&malformed).is_err());
    }
}
