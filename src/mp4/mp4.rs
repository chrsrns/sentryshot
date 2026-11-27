#[cfg(test)]
mod test;

use async_trait::async_trait;
use std::io::Write;
use thiserror::Error;
use tokio::io::{AsyncWrite, AsyncWriteExt};

// Mpeg box type.
pub type BoxType = [u8; 4];

// ImmutableBox is the common trait of boxes.
pub trait ImmutableBox {
    // Type returns the BoxType.
    fn box_type(&self) -> BoxType;

    // Size returns the marshaled size in bytes.
    // The size must be known before marshaling
    // since the box header contains the size.
    fn size(&self) -> usize;
}

pub trait ImmutableBoxSync: ImmutableBox {
    // Marshal box to writer.
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error>;
}

#[async_trait]
pub trait ImmutableBoxAsync: ImmutableBox + Send + Sync {
    // Marshal box to writer.
    async fn marshal(&self, w: &mut (dyn AsyncWrite + Unpin + Send + Sync))
    -> Result<(), Mp4Error>;
}

#[cfg(test)]
#[async_trait]
trait ImmutableBoxBoth: ImmutableBoxSync + ImmutableBoxAsync {
    // Upcasting coercion is unstable: https://github.com/rust-lang/rust/issues/65991
    fn marshal_sync(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        ImmutableBoxSync::marshal(self, w)
    }
    async fn marshal_async(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        ImmutableBoxAsync::marshal(self, w).await
    }
}

macro_rules! impl_from {
    ($x:ident) => {
        impl From<$x> for Box<dyn ImmutableBoxSync> {
            fn from(value: $x) -> Self {
                Box::new(value)
            }
        }
        impl From<$x> for Box<dyn ImmutableBoxAsync> {
            fn from(value: $x) -> Self {
                Box::new(value)
            }
        }

        #[cfg(test)]
        impl ImmutableBoxBoth for $x {}

        #[cfg(test)]
        impl From<$x> for Box<dyn ImmutableBoxBoth> {
            fn from(value: $x) -> Self {
                Box::new(value)
            }
        }
    };
}

#[derive(Debug, Error)]
pub enum Mp4Error {
    #[error("write: {0}")]
    Write(#[from] std::io::Error),

    #[error("from int: {0} {1}")]
    FromInt(String, std::num::TryFromIntError),
}

// Tree of boxes that can be marshaled together.
pub struct Boxes {
    pub mp4_box: Box<dyn ImmutableBoxSync>,
    pub children: Vec<Boxes>,
}

impl Boxes {
    #[must_use]
    pub fn new<T: Into<Box<dyn ImmutableBoxSync>>>(mp4_box: T) -> Self {
        Self {
            mp4_box: mp4_box.into(),
            children: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_child(mut self, child: Boxes) -> Self {
        self.children.push(child);
        self
    }

    #[must_use]
    pub fn with_children2(mut self, child1: Boxes, child2: Boxes) -> Self {
        self.children.extend([child1, child2]);
        self
    }
    #[must_use]
    pub fn with_children3(mut self, child1: Boxes, child2: Boxes, child3: Boxes) -> Self {
        self.children.extend([child1, child2, child3]);
        self
    }

    #[must_use]
    pub fn with_children4(
        mut self,
        child1: Boxes,
        child2: Boxes,
        child3: Boxes,
        child4: Boxes,
    ) -> Self {
        self.children.extend([child1, child2, child3, child4]);
        self
    }

    #[must_use]
    pub fn with_children5(
        mut self,
        child1: Boxes,
        child2: Boxes,
        child3: Boxes,
        child4: Boxes,
        child5: Boxes,
    ) -> Self {
        self.children
            .extend([child1, child2, child3, child4, child5]);
        self
    }

    #[must_use]
    pub fn with_children6(
        mut self,
        child1: Boxes,
        child2: Boxes,
        child3: Boxes,
        child4: Boxes,
        child5: Boxes,
        child6: Boxes,
    ) -> Self {
        self.children
            .extend([child1, child2, child3, child4, child5, child6]);
        self
    }

    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn with_children7(
        mut self,
        child1: Boxes,
        child2: Boxes,
        child3: Boxes,
        child4: Boxes,
        child5: Boxes,
        child6: Boxes,
        child7: Boxes,
    ) -> Self {
        self.children
            .extend([child1, child2, child3, child4, child5, child6, child7]);
        self
    }

    // Size returns the total size of the box including children.
    #[must_use]
    pub fn size(&self) -> usize {
        let mut total = self.mp4_box.size() + 8;

        for child in &self.children {
            let size = child.size();
            total += size;
        }

        total
    }

    // Marshal box including children.
    pub fn marshal<W: Write>(&self, w: &mut W) -> Result<(), Mp4Error> {
        let size = self.size();

        write_box_header(w, size, self.mp4_box.box_type())?;

        // The size of a empty box is 8 bytes.
        if size != 8 {
            self.mp4_box.marshal(w)?;
        }

        for child in &self.children {
            child.marshal(w)?;
        }
        Ok(())
    }
}

// Tree of boxes that can be marshaled together.
pub struct BoxesAsync {
    pub mp4_box: Box<dyn ImmutableBoxAsync>,
    pub children: Vec<BoxesAsync>,
}

impl BoxesAsync {
    #[must_use]
    pub fn new<T: Into<Box<dyn ImmutableBoxAsync>>>(mp4_box: T) -> Self {
        Self {
            mp4_box: mp4_box.into(),
            children: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_child(mut self, child: BoxesAsync) -> Self {
        self.children.push(child);
        self
    }

    #[must_use]
    pub fn with_children2(mut self, child1: BoxesAsync, child2: BoxesAsync) -> Self {
        self.children.extend([child1, child2]);
        self
    }
    #[must_use]
    pub fn with_children3(
        mut self,
        child1: BoxesAsync,
        child2: BoxesAsync,
        child3: BoxesAsync,
    ) -> Self {
        self.children.extend([child1, child2, child3]);
        self
    }

    #[must_use]
    pub fn with_children4(
        mut self,
        child1: BoxesAsync,
        child2: BoxesAsync,
        child3: BoxesAsync,
        child4: BoxesAsync,
    ) -> Self {
        self.children.extend([child1, child2, child3, child4]);
        self
    }

    #[must_use]
    pub fn with_children5(
        mut self,
        child1: BoxesAsync,
        child2: BoxesAsync,
        child3: BoxesAsync,
        child4: BoxesAsync,
        child5: BoxesAsync,
    ) -> Self {
        self.children
            .extend([child1, child2, child3, child4, child5]);
        self
    }

    #[must_use]
    pub fn with_children6(
        mut self,
        child1: BoxesAsync,
        child2: BoxesAsync,
        child3: BoxesAsync,
        child4: BoxesAsync,
        child5: BoxesAsync,
        child6: BoxesAsync,
    ) -> Self {
        self.children
            .extend([child1, child2, child3, child4, child5, child6]);
        self
    }

    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn with_children7(
        mut self,
        child1: BoxesAsync,
        child2: BoxesAsync,
        child3: BoxesAsync,
        child4: BoxesAsync,
        child5: BoxesAsync,
        child6: BoxesAsync,
        child7: BoxesAsync,
    ) -> Self {
        self.children
            .extend([child1, child2, child3, child4, child5, child6, child7]);
        self
    }

    // Size returns the total size of the box including children.
    #[must_use]
    pub fn size(&self) -> usize {
        let mut total = self.mp4_box.size() + 8;

        for child in &self.children {
            let size = child.size();
            total += size;
        }

        total
    }

    // Marshal box including children.
    pub async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        // DFS.
        let mut items = vec![self];
        while let Some(item) = items.pop() {
            write_box_header2(w, item.size(), item.mp4_box.box_type()).await?;

            let box_is_empty = item.size() == 8;
            if !box_is_empty {
                item.mp4_box.marshal(w).await?;
            }

            items.extend(item.children.iter().rev());
        }
        Ok(())
    }
}

pub fn write_box_header<W: Write>(w: &mut W, size: usize, typ: BoxType) -> Result<(), Mp4Error> {
    w.write_all(
        &u32::try_from(size)
            .map_err(|e| Mp4Error::FromInt("write box info".to_owned(), e))?
            .to_be_bytes(),
    )?;
    w.write_all(&typ)?;
    Ok(())
}

pub async fn write_box_header2(
    w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    size: usize,
    typ: BoxType,
) -> Result<(), Mp4Error> {
    w.write_all(
        &u32::try_from(size)
            .map_err(|e| Mp4Error::FromInt("write box info".to_owned(), e))?
            .to_be_bytes(),
    )
    .await?;
    w.write_all(&typ).await?;
    Ok(())
}

pub async fn write_single_box2(
    w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    b: &dyn ImmutableBoxAsync,
) -> Result<usize, Mp4Error> {
    let size = 8 + b.size();

    write_box_header2(w, size, b.box_type()).await?;

    // The size of a empty box is 8 bytes.
    if size != 8 {
        b.marshal(w).await?;
    }
    Ok(size)
}

/*
// Marshal ImmutableBoxes to writer.
fn (boxes ImmutableBoxes) marshal(&self, w: &mut dyn std::io::Write) -> Result<(), MarshalError> {
    for _, b := range boxes {
        if _, err := WriteSingleBox(w, b); err != nil {
            return err
        }
    }
    return nil
}

// Size combined size of boxes.
fn (boxes ImmutableBoxes) size(&self) -> usize {
    var n int
    for _, b := range boxes {
        n += 8
        n += b.Size()
    }
    return n
}

*/
/************************* FullBox **************************/

#[derive(Clone, Copy, Default)]
pub struct FullBox {
    pub version: u8,
    pub flags: [u8; 3],
}

impl FullBox {
    fn get_flags(self) -> u32 {
        parse_fullbox_flags(self.flags)
    }

    fn check_flag(self, flag: u32) -> bool {
        self.get_flags() & flag != 0
    }

    pub fn marshal_field(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        w.write_all(&[self.version])?;
        w.write_all(&self.flags)?;
        Ok(())
    }

    pub async fn marshal_field2(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        w.write_all(&[self.version]).await?;
        w.write_all(&self.flags).await?;
        Ok(())
    }
}

fn parse_fullbox_flags(flags: [u8; 3]) -> u32 {
    (u32::from(flags[0]) << 16) ^ (u32::from(flags[1]) << 8) ^ (u32::from(flags[2]))
}

fn check_fullbox_flag(flags: [u8; 3], flag: u32) -> bool {
    let flags = parse_fullbox_flags(flags);
    flags & flag != 0
}

#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
pub fn u32_to_flags(v: u32) -> [u8; 3] {
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

/*************************** btrt ****************************/

pub const TYPE_BTRT: BoxType = *b"btrt";

pub struct Btrt {
    pub buffer_size_db: u32,
    pub max_bitrate: u32,
    pub avg_bitrate: u32,
}
impl_from!(Btrt);

impl ImmutableBox for Btrt {
    fn box_type(&self) -> BoxType {
        TYPE_BTRT
    }

    fn size(&self) -> usize {
        12
    }
}

impl ImmutableBoxSync for Btrt {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        w.write_all(&self.buffer_size_db.to_be_bytes())?;
        w.write_all(&self.max_bitrate.to_be_bytes())?;
        w.write_all(&self.avg_bitrate.to_be_bytes())?;
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Btrt {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        w.write_all(&self.buffer_size_db.to_be_bytes()).await?;
        w.write_all(&self.max_bitrate.to_be_bytes()).await?;
        w.write_all(&self.avg_bitrate.to_be_bytes()).await?;
        Ok(())
    }
}

/*************************** ctts ****************************/

pub const TYPE_CTTS: BoxType = *b"ctts";

pub struct Ctts {
    pub flags: [u8; 3],
    pub entries: CttsEntries,
}
impl_from!(Ctts);

pub enum CttsEntries {
    V0(Vec<CttsEntryV0>),
    V1(Vec<CttsEntryV1>),
}

#[derive(Clone, Copy)]
pub struct CttsEntryV0 {
    pub sample_count: u32,
    pub sample_offset: u32,
}

#[derive(Clone, Copy)]
pub struct CttsEntryV1 {
    pub sample_count: u32,
    pub sample_offset: i32,
}

impl ImmutableBox for Ctts {
    fn box_type(&self) -> BoxType {
        TYPE_CTTS
    }

    fn size(&self) -> usize {
        let num_entries = match &self.entries {
            CttsEntries::V0(v) => v.len(),
            CttsEntries::V1(v) => v.len(),
        };
        8 + num_entries * 8
    }
}

impl ImmutableBoxSync for Ctts {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        match &self.entries {
            CttsEntries::V0(entries) => {
                w.write_all(&[0])?;
                w.write_all(&self.flags)?;

                w.write_all(
                    &(u32::try_from(entries.len())
                        .map_err(|e| Mp4Error::FromInt("ctts".to_owned(), e))?)
                    .to_be_bytes(),
                )?;
                for entry in entries {
                    w.write_all(&entry.sample_count.to_be_bytes())?;
                    w.write_all(&entry.sample_offset.to_be_bytes())?;
                }
            }
            CttsEntries::V1(entries) => {
                w.write_all(&[1])?;
                w.write_all(&self.flags)?;

                w.write_all(
                    &(u32::try_from(entries.len())
                        .map_err(|e| Mp4Error::FromInt("ctts".to_owned(), e))?)
                    .to_be_bytes(),
                )?;
                for entry in entries {
                    w.write_all(&entry.sample_count.to_be_bytes())?;
                    w.write_all(&entry.sample_offset.to_be_bytes())?;
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Ctts {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        match &self.entries {
            CttsEntries::V0(entries) => {
                w.write_all(&[0]).await?;
                w.write_all(&self.flags).await?;

                w.write_all(
                    &(u32::try_from(entries.len())
                        .map_err(|e| Mp4Error::FromInt("ctts".to_owned(), e))?)
                    .to_be_bytes(),
                )
                .await?;
                for entry in entries {
                    w.write_all(&entry.sample_count.to_be_bytes()).await?;
                    w.write_all(&entry.sample_offset.to_be_bytes()).await?;
                }
            }
            CttsEntries::V1(entries) => {
                w.write_all(&[1]).await?;
                w.write_all(&self.flags).await?;

                w.write_all(
                    &(u32::try_from(entries.len())
                        .map_err(|e| Mp4Error::FromInt("ctts".to_owned(), e))?)
                    .to_be_bytes(),
                )
                .await?;
                for entry in entries {
                    w.write_all(&entry.sample_count.to_be_bytes()).await?;
                    w.write_all(&entry.sample_offset.to_be_bytes()).await?;
                }
            }
        }

        Ok(())
    }
}

/*************************** dinf ****************************/

pub const TYPE_DINF: BoxType = *b"dinf";

pub struct Dinf;
impl_from!(Dinf);

impl ImmutableBox for Dinf {
    fn box_type(&self) -> BoxType {
        TYPE_DINF
    }

    fn size(&self) -> usize {
        0
    }
}

impl ImmutableBoxSync for Dinf {
    fn marshal(&self, _: &mut dyn Write) -> Result<(), Mp4Error> {
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Dinf {
    async fn marshal(
        &self,
        _: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        Ok(())
    }
}

/*************************** dref ****************************/

pub const TYPE_DREF: BoxType = *b"dref";

pub struct Dref {
    pub full_box: FullBox,
    pub entry_count: u32,
}
impl_from!(Dref);

impl ImmutableBox for Dref {
    fn box_type(&self) -> BoxType {
        TYPE_DREF
    }

    fn size(&self) -> usize {
        8
    }
}

impl ImmutableBoxSync for Dref {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        self.full_box.marshal_field(w)?;
        w.write_all(&self.entry_count.to_be_bytes())?;
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Dref {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        self.full_box.marshal_field2(w).await?;
        w.write_all(&self.entry_count.to_be_bytes()).await?;
        Ok(())
    }
}

/*************************** url ****************************/

pub const TYPE_URL: BoxType = *b"url ";

pub struct Url {
    pub full_box: FullBox,
    pub location: String,
}
impl_from!(Url);

pub const URL_NOPT: u32 = 0x0000_0001;

impl ImmutableBox for Url {
    fn box_type(&self) -> BoxType {
        TYPE_URL
    }

    fn size(&self) -> usize {
        if self.full_box.check_flag(URL_NOPT) {
            4
        } else {
            self.location.len() + 5
        }
    }
}

impl ImmutableBoxSync for Url {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        self.full_box.marshal_field(w)?;
        if !self.full_box.check_flag(URL_NOPT) {
            w.write_all((self.location.clone() + "\0").as_bytes())?;
        }
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Url {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        self.full_box.marshal_field2(w).await?;
        if !self.full_box.check_flag(URL_NOPT) {
            w.write_all((self.location.clone() + "\0").as_bytes())
                .await?;
        }
        Ok(())
    }
}

/*************************** ftyp ****************************/

pub const TYPE_FTYP: BoxType = *b"ftyp";

pub struct Ftyp {
    pub major_brand: [u8; 4],
    pub minor_version: u32,
    pub compatible_brands: Vec<CompatibleBrandElem>,
}
impl_from!(Ftyp);

#[repr(transparent)]
pub struct CompatibleBrandElem(pub [u8; 4]);

impl ImmutableBox for Ftyp {
    fn box_type(&self) -> BoxType {
        TYPE_FTYP
    }

    fn size(&self) -> usize {
        8 + self.compatible_brands.len() * 4
    }
}

impl ImmutableBoxSync for Ftyp {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        w.write_all(&self.major_brand)?;
        w.write_all(&self.minor_version.to_be_bytes())?;
        for brands in &self.compatible_brands {
            w.write_all(&brands.0)?;
        }
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Ftyp {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        w.write_all(&self.major_brand).await?;
        w.write_all(&self.minor_version.to_be_bytes()).await?;
        for brands in &self.compatible_brands {
            w.write_all(&brands.0).await?;
        }
        Ok(())
    }
}

/*************************** hdlr ****************************/

pub const TYPE_HDLR: BoxType = *b"hdlr";

#[derive(Default)]
pub struct Hdlr {
    pub full_box: FullBox,
    // Predefined corresponds to component_type of QuickTime.
    // pre_defined of ISO-14496 has albufays zero,
    // hobufever component_type has "mhlr" or "dhlr".
    pub pre_defined: u32,
    pub handler_type: [u8; 4],
    pub reserved: [u32; 3],
    pub name: String,
}
impl_from!(Hdlr);

impl ImmutableBox for Hdlr {
    fn box_type(&self) -> BoxType {
        TYPE_HDLR
    }

    fn size(&self) -> usize {
        25 + self.name.len()
    }
}

impl ImmutableBoxSync for Hdlr {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        self.full_box.marshal_field(w)?;
        w.write_all(&self.pre_defined.to_be_bytes())?;
        w.write_all(&self.handler_type)?;
        for reserved in &self.reserved {
            w.write_all(&reserved.to_be_bytes())?;
        }
        w.write_all((self.name.clone() + "\0").as_bytes())?;
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Hdlr {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        self.full_box.marshal_field2(w).await?;
        w.write_all(&self.pre_defined.to_be_bytes()).await?;
        w.write_all(&self.handler_type).await?;
        for reserved in &self.reserved {
            w.write_all(&reserved.to_be_bytes()).await?;
        }
        w.write_all((self.name.clone() + "\0").as_bytes()).await?;
        Ok(())
    }
}

pub const TYPE_MDAT: BoxType = *b"mdat";

/*************************** mdia ****************************/

pub const TYPE_MDIA: BoxType = *b"mdia";

pub struct Mdia;
impl_from!(Mdia);

impl ImmutableBox for Mdia {
    fn box_type(&self) -> BoxType {
        TYPE_MDIA
    }

    fn size(&self) -> usize {
        0
    }
}

impl ImmutableBoxSync for Mdia {
    fn marshal(&self, _: &mut dyn Write) -> Result<(), Mp4Error> {
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Mdia {
    async fn marshal(
        &self,
        _: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        Ok(())
    }
}

/*************************** mdhd ****************************/

pub const TYPE_MDHD: BoxType = *b"mdhd";

#[derive(Default)]
pub struct Mdhd {
    pub flags: [u8; 3],
    pub version: MdhdVersion,
    pub timescale: u32,

    pub pad: bool,         // 1 bit.
    pub language: [u8; 3], // 5 bits. ISO-639-2/T language code
    pub pre_defined: u16,
}
impl_from!(Mdhd);

pub enum MdhdVersion {
    V0(MdhdV0),
    V1(MdhdV1),
}

impl Default for MdhdVersion {
    fn default() -> Self {
        Self::V0(MdhdV0::default())
    }
}

#[derive(Default)]
pub struct MdhdV0 {
    pub creation_time: u32,
    pub modification_time: u32,
    pub duration: u32,
}

pub struct MdhdV1 {
    pub creation_time: u64,
    pub modification_time: u64,
    pub duration: u64,
}

impl ImmutableBox for Mdhd {
    fn box_type(&self) -> BoxType {
        TYPE_MDHD
    }

    fn size(&self) -> usize {
        match self.version {
            MdhdVersion::V0(_) => 24,
            MdhdVersion::V1(_) => 36,
        }
    }
}

impl ImmutableBoxSync for Mdhd {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        match &self.version {
            MdhdVersion::V0(v) => {
                w.write_all(&[0])?;
                w.write_all(&self.flags)?;
                w.write_all(&v.creation_time.to_be_bytes())?;
                w.write_all(&v.modification_time.to_be_bytes())?;
                w.write_all(&self.timescale.to_be_bytes())?;
                w.write_all(&v.duration.to_be_bytes())?;
            }
            MdhdVersion::V1(v) => {
                w.write_all(&[1])?;
                w.write_all(&self.flags)?;
                w.write_all(&v.creation_time.to_be_bytes())?;
                w.write_all(&v.modification_time.to_be_bytes())?;
                w.write_all(&self.timescale.to_be_bytes())?;
                w.write_all(&v.duration.to_be_bytes())?;
            }
        }

        if self.pad {
            w.write_all(&[((0b0000_0001 << 7)
                | ((self.language[0] & 0b0001_1111) << 2)
                | ((self.language[1] & 0b0001_1111) >> 3))])?;
        } else {
            w.write_all(&[
                (((self.language[0] & 0b0001_1111) << 2) | ((self.language[1] & 0b0001_1111) >> 3))
            ])?;
        }

        w.write_all(&[((self.language[1] << 5) | self.language[2] & 0b0001_1111)])?;
        w.write_all(&self.pre_defined.to_be_bytes())?;
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Mdhd {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        match &self.version {
            MdhdVersion::V0(v) => {
                w.write_all(&[0]).await?;
                w.write_all(&self.flags).await?;
                w.write_all(&v.creation_time.to_be_bytes()).await?;
                w.write_all(&v.modification_time.to_be_bytes()).await?;
                w.write_all(&self.timescale.to_be_bytes()).await?;
                w.write_all(&v.duration.to_be_bytes()).await?;
            }
            MdhdVersion::V1(v) => {
                w.write_all(&[1]).await?;
                w.write_all(&self.flags).await?;
                w.write_all(&v.creation_time.to_be_bytes()).await?;
                w.write_all(&v.modification_time.to_be_bytes()).await?;
                w.write_all(&self.timescale.to_be_bytes()).await?;
                w.write_all(&v.duration.to_be_bytes()).await?;
            }
        }

        if self.pad {
            w.write_all(&[((0b0000_0001 << 7)
                | ((self.language[0] & 0b0001_1111) << 2)
                | ((self.language[1] & 0b0001_1111) >> 3))])
                .await?;
        } else {
            w.write_all(&[
                (((self.language[0] & 0b0001_1111) << 2) | ((self.language[1] & 0b0001_1111) >> 3))
            ])
            .await?;
        }

        w.write_all(&[((self.language[1] << 5) | self.language[2] & 0b0001_1111)])
            .await?;
        w.write_all(&self.pre_defined.to_be_bytes()).await?;
        Ok(())
    }
}

/*************************** mfhd ****************************/

pub const TYPE_MFHD: BoxType = *b"mfhd";

pub struct Mfhd {
    pub full_box: FullBox,
    pub sequence_number: u32,
}
impl_from!(Mfhd);

impl ImmutableBox for Mfhd {
    fn box_type(&self) -> BoxType {
        TYPE_MFHD
    }

    fn size(&self) -> usize {
        8
    }
}

impl ImmutableBoxSync for Mfhd {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        self.full_box.marshal_field(w)?;
        w.write_all(&self.sequence_number.to_be_bytes())?;
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Mfhd {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        self.full_box.marshal_field2(w).await?;
        w.write_all(&self.sequence_number.to_be_bytes()).await?;
        Ok(())
    }
}

/*************************** minf ****************************/

pub const TYPE_MINF: BoxType = *b"minf";

pub struct Minf;
impl_from!(Minf);

impl ImmutableBox for Minf {
    fn box_type(&self) -> BoxType {
        TYPE_MINF
    }

    fn size(&self) -> usize {
        0
    }
}

impl ImmutableBoxSync for Minf {
    fn marshal(&self, _: &mut dyn Write) -> Result<(), Mp4Error> {
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Minf {
    async fn marshal(
        &self,
        _: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        Ok(())
    }
}

/*************************** moof ****************************/

pub const TYPE_MOOF: BoxType = *b"moof";

pub struct Moof;
impl_from!(Moof);

impl ImmutableBox for Moof {
    fn box_type(&self) -> BoxType {
        TYPE_MOOF
    }

    fn size(&self) -> usize {
        0
    }
}

impl ImmutableBoxSync for Moof {
    fn marshal(&self, _: &mut dyn Write) -> Result<(), Mp4Error> {
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Moof {
    async fn marshal(
        &self,
        _: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        Ok(())
    }
}

/*************************** moov ****************************/

pub const TYPE_MOOV: BoxType = *b"moov";

pub struct Moov;
impl_from!(Moov);

impl ImmutableBox for Moov {
    fn box_type(&self) -> BoxType {
        TYPE_MOOV
    }

    fn size(&self) -> usize {
        0
    }
}

impl ImmutableBoxSync for Moov {
    fn marshal(&self, _: &mut dyn Write) -> Result<(), Mp4Error> {
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Moov {
    async fn marshal(
        &self,
        _: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        Ok(())
    }
}

/*************************** mvex ****************************/

pub const TYPE_MVEX: BoxType = *b"mvex";

pub struct Mvex;
impl_from!(Mvex);

impl ImmutableBox for Mvex {
    fn box_type(&self) -> BoxType {
        TYPE_MVEX
    }

    fn size(&self) -> usize {
        0
    }
}

impl ImmutableBoxSync for Mvex {
    fn marshal(&self, _: &mut dyn Write) -> Result<(), Mp4Error> {
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Mvex {
    async fn marshal(
        &self,
        _: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        Ok(())
    }
}

/*************************** mvhd ****************************/

pub const TYPE_MVHD: BoxType = *b"mvhd";

#[derive(Default)]
pub struct Mvhd {
    pub flags: [u8; 3],
    pub version: MvhdVersion,
    pub timescale: u32,
    pub rate: i32,   // fixed-point 16.16 - template=0x00010000
    pub volume: i16, // template=0x0100
    pub reserved: i16,
    pub reserved2: [i32; 2],
    pub matrix: [i32; 9], // template={ 0x00010000,0,0,0,0x00010000,0,0,0,0x40000000 }
    pub pre_defined: [i32; 6],
    pub next_track_id: u32,
}
impl_from!(Mvhd);

pub enum MvhdVersion {
    V0(MvhdV0),
    V1(MvhdV1),
}

impl Default for MvhdVersion {
    fn default() -> Self {
        Self::V0(MvhdV0::default())
    }
}

#[derive(Default)]
pub struct MvhdV0 {
    pub creation_time: u32,
    pub modification_time: u32,
    pub duration: u32,
}

pub struct MvhdV1 {
    pub creation_time: u64,
    pub modification_time: u64,
    pub duration: u64,
}

impl ImmutableBox for Mvhd {
    fn box_type(&self) -> BoxType {
        TYPE_MVHD
    }

    fn size(&self) -> usize {
        match self.version {
            MvhdVersion::V0(_) => 100,
            MvhdVersion::V1(_) => 112,
        }
    }
}

impl ImmutableBoxSync for Mvhd {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        match &self.version {
            MvhdVersion::V0(v) => {
                w.write_all(&[0])?;
                w.write_all(&self.flags)?;
                w.write_all(&v.creation_time.to_be_bytes())?;
                w.write_all(&v.modification_time.to_be_bytes())?;
                w.write_all(&self.timescale.to_be_bytes())?;
                w.write_all(&v.duration.to_be_bytes())?;
            }
            MvhdVersion::V1(v) => {
                w.write_all(&[1])?;
                w.write_all(&self.flags)?;
                w.write_all(&v.creation_time.to_be_bytes())?;
                w.write_all(&v.modification_time.to_be_bytes())?;
                w.write_all(&self.timescale.to_be_bytes())?;
                w.write_all(&v.duration.to_be_bytes())?;
            }
        }

        w.write_all(&self.rate.to_be_bytes())?;
        w.write_all(&self.volume.to_be_bytes())?;
        w.write_all(&self.reserved.to_be_bytes())?;

        for reserved in &self.reserved2 {
            w.write_all(&reserved.to_be_bytes())?;
        }
        for matrix in &self.matrix {
            w.write_all(&matrix.to_be_bytes())?;
        }
        for pre_defined in &self.pre_defined {
            w.write_all(&pre_defined.to_be_bytes())?;
        }

        w.write_all(&self.next_track_id.to_be_bytes())?;

        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Mvhd {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        match &self.version {
            MvhdVersion::V0(v) => {
                w.write_all(&[0]).await?;
                w.write_all(&self.flags).await?;
                w.write_all(&v.creation_time.to_be_bytes()).await?;
                w.write_all(&v.modification_time.to_be_bytes()).await?;
                w.write_all(&self.timescale.to_be_bytes()).await?;
                w.write_all(&v.duration.to_be_bytes()).await?;
            }
            MvhdVersion::V1(v) => {
                w.write_all(&[1]).await?;
                w.write_all(&self.flags).await?;
                w.write_all(&v.creation_time.to_be_bytes()).await?;
                w.write_all(&v.modification_time.to_be_bytes()).await?;
                w.write_all(&self.timescale.to_be_bytes()).await?;
                w.write_all(&v.duration.to_be_bytes()).await?;
            }
        }

        w.write_all(&self.rate.to_be_bytes()).await?;
        w.write_all(&self.volume.to_be_bytes()).await?;
        w.write_all(&self.reserved.to_be_bytes()).await?;

        for reserved in &self.reserved2 {
            w.write_all(&reserved.to_be_bytes()).await?;
        }
        for matrix in &self.matrix {
            w.write_all(&matrix.to_be_bytes()).await?;
        }
        for pre_defined in &self.pre_defined {
            w.write_all(&pre_defined.to_be_bytes()).await?;
        }

        w.write_all(&self.next_track_id.to_be_bytes()).await?;

        Ok(())
    }
}

/*********************** SampleEntry *************************/

#[derive(Default)]
pub struct SampleEntry {
    pub reserved: [u8; 6],
    pub data_reference_index: u16,
}

impl SampleEntry {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        for reserved in &self.reserved {
            w.write_all(&reserved.to_be_bytes())?;
        }
        w.write_all(&self.data_reference_index.to_be_bytes())?;
        Ok(())
    }

    async fn marshal2(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        for reserved in &self.reserved {
            w.write_all(&reserved.to_be_bytes()).await?;
        }
        w.write_all(&self.data_reference_index.to_be_bytes())
            .await?;
        Ok(())
    }
}

/*********************** avc1 *************************/

pub const TYPE_AVC1: BoxType = *b"avc1";

#[derive(Default)]
pub struct Avc1 {
    pub sample_entry: SampleEntry,
    pub pre_defined: u16,
    pub reserved: u16,
    pub pre_defined2: [u32; 3],
    pub width: u16,
    pub height: u16,
    pub horiz_resolution: u32,
    pub vert_resolution: u32,
    pub reserved2: u32,
    pub frame_count: u16,
    pub compressor_name: [u8; 32],
    pub depth: u16,
    pub pre_defined3: i16,
}
impl_from!(Avc1);

impl ImmutableBox for Avc1 {
    fn box_type(&self) -> BoxType {
        TYPE_AVC1
    }

    fn size(&self) -> usize {
        78
    }
}

impl ImmutableBoxSync for Avc1 {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        self.sample_entry.marshal(w)?;
        w.write_all(&self.pre_defined.to_be_bytes())?;
        w.write_all(&self.reserved.to_be_bytes())?;
        for pre_defined in &self.pre_defined2 {
            w.write_all(&pre_defined.to_be_bytes())?;
        }
        w.write_all(&self.width.to_be_bytes())?;
        w.write_all(&self.height.to_be_bytes())?;
        w.write_all(&self.horiz_resolution.to_be_bytes())?;
        w.write_all(&self.vert_resolution.to_be_bytes())?;
        w.write_all(&self.reserved2.to_be_bytes())?;
        w.write_all(&self.frame_count.to_be_bytes())?;
        w.write_all(&self.compressor_name)?;
        w.write_all(&self.depth.to_be_bytes())?;
        w.write_all(&self.pre_defined3.to_be_bytes())?;
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Avc1 {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        self.sample_entry.marshal2(w).await?;
        w.write_all(&self.pre_defined.to_be_bytes()).await?;
        w.write_all(&self.reserved.to_be_bytes()).await?;
        for pre_defined in &self.pre_defined2 {
            w.write_all(&pre_defined.to_be_bytes()).await?;
        }
        w.write_all(&self.width.to_be_bytes()).await?;
        w.write_all(&self.height.to_be_bytes()).await?;
        w.write_all(&self.horiz_resolution.to_be_bytes()).await?;
        w.write_all(&self.vert_resolution.to_be_bytes()).await?;
        w.write_all(&self.reserved2.to_be_bytes()).await?;
        w.write_all(&self.frame_count.to_be_bytes()).await?;
        w.write_all(&self.compressor_name).await?;
        w.write_all(&self.depth.to_be_bytes()).await?;
        w.write_all(&self.pre_defined3.to_be_bytes()).await?;
        Ok(())
    }
}

pub const TYPE_AVCC: BoxType = *b"avcC";

/*************************** stbl ****************************/

pub const TYPE_STBL: BoxType = *b"stbl";

pub struct Stbl;
impl_from!(Stbl);

impl ImmutableBox for Stbl {
    fn box_type(&self) -> BoxType {
        TYPE_STBL
    }

    fn size(&self) -> usize {
        0
    }
}

impl ImmutableBoxSync for Stbl {
    fn marshal(&self, _: &mut dyn Write) -> Result<(), Mp4Error> {
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Stbl {
    async fn marshal(
        &self,
        _: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        Ok(())
    }
}

/*************************** stco ****************************/

pub const TYPE_STCO: BoxType = *b"stco";

#[derive(Default)]
pub struct Stco {
    pub full_box: FullBox,
    pub chunk_offsets: Vec<u32>,
}
impl_from!(Stco);

impl ImmutableBox for Stco {
    fn box_type(&self) -> BoxType {
        TYPE_STCO
    }

    fn size(&self) -> usize {
        8 + (self.chunk_offsets.len()) * 4
    }
}

impl ImmutableBoxSync for Stco {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        self.full_box.marshal_field(w)?;
        w.write_all(
            &u32::try_from(self.chunk_offsets.len())
                .map_err(|e| Mp4Error::FromInt("stco".to_owned(), e))?
                .to_be_bytes(),
        )?;
        for offset in &self.chunk_offsets {
            w.write_all(&offset.to_be_bytes())?;
        }
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Stco {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        self.full_box.marshal_field2(w).await?;
        w.write_all(
            &u32::try_from(self.chunk_offsets.len())
                .map_err(|e| Mp4Error::FromInt("stco".to_owned(), e))?
                .to_be_bytes(),
        )
        .await?;
        for offset in &self.chunk_offsets {
            w.write_all(&offset.to_be_bytes()).await?;
        }
        Ok(())
    }
}

/*************************** stsc ****************************/

pub const TYPE_STSC: BoxType = *b"stsc";

#[derive(Clone, Copy)]
pub struct StscEntry {
    pub first_chunk: u32,
    pub samples_per_chunk: u32,
    pub sample_description_index: u32,
}

impl StscEntry {
    fn marshal_field(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        w.write_all(&self.first_chunk.to_be_bytes())?;
        w.write_all(&self.samples_per_chunk.to_be_bytes())?;
        w.write_all(&self.sample_description_index.to_be_bytes())?;
        Ok(())
    }
    async fn marshal_field2(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        w.write_all(&self.first_chunk.to_be_bytes()).await?;
        w.write_all(&self.samples_per_chunk.to_be_bytes()).await?;
        w.write_all(&self.sample_description_index.to_be_bytes())
            .await?;
        Ok(())
    }
}

#[derive(Default)]
pub struct Stsc {
    pub full_box: FullBox,
    pub entries: Vec<StscEntry>,
}
impl_from!(Stsc);

impl ImmutableBox for Stsc {
    fn box_type(&self) -> BoxType {
        TYPE_STSC
    }

    fn size(&self) -> usize {
        8 + self.entries.len() * 12
    }
}

impl ImmutableBoxSync for Stsc {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        self.full_box.marshal_field(w)?;
        w.write_all(
            &u32::try_from(self.entries.len())
                .map_err(|e| Mp4Error::FromInt("stsc".to_owned(), e))?
                .to_be_bytes(),
        )?; // Entry count.
        for entry in &self.entries {
            entry.marshal_field(w)?;
        }
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Stsc {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        self.full_box.marshal_field2(w).await?;
        w.write_all(
            &u32::try_from(self.entries.len())
                .map_err(|e| Mp4Error::FromInt("stsc".to_owned(), e))?
                .to_be_bytes(),
        )
        .await?; // Entry count.
        for entry in &self.entries {
            entry.marshal_field2(w).await?;
        }
        Ok(())
    }
}

/*************************** stsd ****************************/

pub const TYPE_STSD: BoxType = *b"stsd";

pub struct Stsd {
    pub full_box: FullBox,
    pub entry_count: u32,
}
impl_from!(Stsd);

impl ImmutableBox for Stsd {
    fn box_type(&self) -> BoxType {
        TYPE_STSD
    }

    fn size(&self) -> usize {
        8
    }
}

impl ImmutableBoxSync for Stsd {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        self.full_box.marshal_field(w)?;
        w.write_all(&self.entry_count.to_be_bytes())?;
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Stsd {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        self.full_box.marshal_field2(w).await?;
        w.write_all(&self.entry_count.to_be_bytes()).await?;
        Ok(())
    }
}

/*************************** stss ****************************/

pub const TYPE_STSS: BoxType = *b"stss";

pub struct Stss {
    pub full_box: FullBox,
    pub sample_numbers: Vec<u32>,
}
impl_from!(Stss);

impl ImmutableBox for Stss {
    fn box_type(&self) -> BoxType {
        TYPE_STSS
    }

    fn size(&self) -> usize {
        8 + self.sample_numbers.len() * 4
    }
}

impl ImmutableBoxSync for Stss {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        self.full_box.marshal_field(w)?;
        w.write_all(
            &u32::try_from(self.sample_numbers.len())
                .map_err(|e| Mp4Error::FromInt("stss".to_owned(), e))?
                .to_be_bytes(),
        )?; // Entry count.
        for number in &self.sample_numbers {
            w.write_all(&number.to_be_bytes())?;
        }
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Stss {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        self.full_box.marshal_field2(w).await?;
        w.write_all(
            &u32::try_from(self.sample_numbers.len())
                .map_err(|e| Mp4Error::FromInt("stss".to_owned(), e))?
                .to_be_bytes(),
        )
        .await?; // Entry count.
        for number in &self.sample_numbers {
            w.write_all(&number.to_be_bytes()).await?;
        }
        Ok(())
    }
}

/*************************** stsz ****************************/

pub const TYPE_STSZ: BoxType = *b"stsz";

#[derive(Default)]
pub struct Stsz {
    pub full_box: FullBox,
    pub sample_size: u32,
    pub sample_count: u32,
    pub entry_sizes: Vec<u32>,
}
impl_from!(Stsz);

impl ImmutableBox for Stsz {
    fn box_type(&self) -> BoxType {
        TYPE_STSZ
    }

    fn size(&self) -> usize {
        12 + self.entry_sizes.len() * 4
    }
}

impl ImmutableBoxSync for Stsz {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        self.full_box.marshal_field(w)?;
        w.write_all(&self.sample_size.to_be_bytes())?;
        w.write_all(&self.sample_count.to_be_bytes())?;
        for entry in &self.entry_sizes {
            w.write_all(&entry.to_be_bytes())?;
        }
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Stsz {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        self.full_box.marshal_field2(w).await?;
        w.write_all(&self.sample_size.to_be_bytes()).await?;
        w.write_all(&self.sample_count.to_be_bytes()).await?;
        for entry in &self.entry_sizes {
            w.write_all(&entry.to_be_bytes()).await?;
        }
        Ok(())
    }
}

/*************************** stts ****************************/

pub const TYPE_STTS: BoxType = *b"stts";

#[derive(Default)]
pub struct Stts {
    pub full_box: FullBox,
    pub entries: Vec<SttsEntry>,
}
impl_from!(Stts);

#[derive(Clone)]
pub struct SttsEntry {
    pub sample_count: u32,
    pub sample_delta: u32,
}

impl SttsEntry {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        w.write_all(&self.sample_count.to_be_bytes())?;
        w.write_all(&self.sample_delta.to_be_bytes())?;
        Ok(())
    }
    async fn marshal2(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        w.write_all(&self.sample_count.to_be_bytes()).await?;
        w.write_all(&self.sample_delta.to_be_bytes()).await?;
        Ok(())
    }
}

impl ImmutableBox for Stts {
    fn box_type(&self) -> BoxType {
        TYPE_STTS
    }

    fn size(&self) -> usize {
        8 + self.entries.len() * 8
    }
}

impl ImmutableBoxSync for Stts {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        self.full_box.marshal_field(w)?;
        w.write_all(
            &u32::try_from(self.entries.len())
                .map_err(|e| Mp4Error::FromInt("stts".to_owned(), e))?
                .to_be_bytes(),
        )?;
        for entry in &self.entries {
            entry.marshal(w)?;
        }
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Stts {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        self.full_box.marshal_field2(w).await?;
        w.write_all(
            &u32::try_from(self.entries.len())
                .map_err(|e| Mp4Error::FromInt("stts".to_owned(), e))?
                .to_be_bytes(),
        )
        .await?;
        for entry in &self.entries {
            entry.marshal2(w).await?;
        }
        Ok(())
    }
}

/*************************** tfdt ****************************/

pub const TYPE_TFDT: BoxType = *b"tfdt";

pub struct Tfdt {
    pub flags: [u8; 3],
    pub base_media_decode_time: TfdtBaseMediaDecodeTime,
}
impl_from!(Tfdt);

pub enum TfdtBaseMediaDecodeTime {
    V1(u64),
}

impl ImmutableBox for Tfdt {
    fn box_type(&self) -> BoxType {
        TYPE_TFDT
    }

    fn size(&self) -> usize {
        match self.base_media_decode_time {
            TfdtBaseMediaDecodeTime::V1(_) => 12,
        }
    }
}

impl ImmutableBoxSync for Tfdt {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        match self.base_media_decode_time {
            TfdtBaseMediaDecodeTime::V1(v) => {
                w.write_all(&[1])?;
                w.write_all(&self.flags)?;
                w.write_all(&v.to_be_bytes())?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Tfdt {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        match self.base_media_decode_time {
            TfdtBaseMediaDecodeTime::V1(v) => {
                w.write_all(&[1]).await?;
                w.write_all(&self.flags).await?;
                w.write_all(&v.to_be_bytes()).await?;
            }
        }
        Ok(())
    }
}

/*************************** tfhd ****************************/

pub const TYPE_TFHD: BoxType = *b"tfhd";

#[derive(Default)]
pub struct Tfhd {
    pub full_box: FullBox,
    pub track_id: u32,

    // optional
    pub base_data_offset: u64,
    pub sample_description_index: u32,
    pub default_sample_duration: u32,
    pub default_sample_size: u32,
    pub default_sample_flags: u32,
}
impl_from!(Tfhd);

pub const TFHD_BASE_DATA_OFFSET_PRESENT: u32 = 0x0000_0001;
pub const TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT: u32 = 0x0000_0002;
pub const TFHD_DEFAULT_SAMPLE_DURATION_PRESENT: u32 = 0x0000_0008;
pub const TFHD_DEFAULT_SAMPLE_SIZE_PRESENT: u32 = 0x0000_0010;
pub const TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT: u32 = 0x0000_0020;
pub const TFHD_DURATION_IS_EMPTY: u32 = 0x0001_0000;
pub const TFHD_DEFAULT_BASE_IS_MOOF: u32 = 0x0002_0000;

impl ImmutableBox for Tfhd {
    fn box_type(&self) -> BoxType {
        TYPE_TFHD
    }

    fn size(&self) -> usize {
        let mut total: usize = 8;
        if self.full_box.check_flag(TFHD_BASE_DATA_OFFSET_PRESENT) {
            total += 8;
        }
        if self
            .full_box
            .check_flag(TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT)
        {
            total += 4;
        }
        if self
            .full_box
            .check_flag(TFHD_DEFAULT_SAMPLE_DURATION_PRESENT)
        {
            total += 4;
        }
        if self.full_box.check_flag(TFHD_DEFAULT_SAMPLE_SIZE_PRESENT) {
            total += 4;
        }
        if self.full_box.check_flag(TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT) {
            total += 4;
        }
        total
    }
}

impl ImmutableBoxSync for Tfhd {
    // Marshal box to writer.
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        self.full_box.marshal_field(w)?;
        w.write_all(&self.track_id.to_be_bytes())?;
        if self.full_box.check_flag(TFHD_BASE_DATA_OFFSET_PRESENT) {
            w.write_all(&self.base_data_offset.to_be_bytes())?;
        }
        if self
            .full_box
            .check_flag(TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT)
        {
            w.write_all(&self.sample_description_index.to_be_bytes())?;
        }
        if self
            .full_box
            .check_flag(TFHD_DEFAULT_SAMPLE_DURATION_PRESENT)
        {
            w.write_all(&self.default_sample_duration.to_be_bytes())?;
        }
        if self.full_box.check_flag(TFHD_DEFAULT_SAMPLE_SIZE_PRESENT) {
            w.write_all(&self.default_sample_size.to_be_bytes())?;
        }
        if self.full_box.check_flag(TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT) {
            w.write_all(&self.default_sample_flags.to_be_bytes())?;
        }
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Tfhd {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        self.full_box.marshal_field2(w).await?;
        w.write_all(&self.track_id.to_be_bytes()).await?;
        if self.full_box.check_flag(TFHD_BASE_DATA_OFFSET_PRESENT) {
            w.write_all(&self.base_data_offset.to_be_bytes()).await?;
        }
        if self
            .full_box
            .check_flag(TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT)
        {
            w.write_all(&self.sample_description_index.to_be_bytes())
                .await?;
        }
        if self
            .full_box
            .check_flag(TFHD_DEFAULT_SAMPLE_DURATION_PRESENT)
        {
            w.write_all(&self.default_sample_duration.to_be_bytes())
                .await?;
        }
        if self.full_box.check_flag(TFHD_DEFAULT_SAMPLE_SIZE_PRESENT) {
            w.write_all(&self.default_sample_size.to_be_bytes()).await?;
        }
        if self.full_box.check_flag(TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT) {
            w.write_all(&self.default_sample_flags.to_be_bytes())
                .await?;
        }
        Ok(())
    }
}

/*************************** tkhd ****************************/

pub const TYPE_TKHD: BoxType = *b"tkhd";

#[derive(Default)]
pub struct Tkhd {
    pub flags: [u8; 3],
    pub version: TkhdVersion,
    pub track_id: u32,
    pub reserved0: u32,
    pub reserved1: [u32; 2],
    pub layer: i16,           // template=0
    pub alternate_group: i16, // template=0
    pub volume: i16,          // template={if track_is_audio 0x0100 else 0}
    pub reserved2: u16,
    pub matrix: [i32; 9], // template={ 0x00010000,0,0,0,0x00010000,0,0,0,0x40000000 };
    pub width: u32,       // fixed-point 16.16
    pub height: u32,      // fixed-point 16.16
}
impl_from!(Tkhd);

pub enum TkhdVersion {
    V0(TkhdV0),
    V1(TkhdV1),
}

impl Default for TkhdVersion {
    fn default() -> Self {
        Self::V0(TkhdV0::default())
    }
}

#[derive(Default)]
pub struct TkhdV0 {
    pub creation_time: u32,
    pub modification_time: u32,
    pub duration: u32,
}

pub struct TkhdV1 {
    pub creation_time: u64,
    pub modification_time: u64,
    pub duration: u64,
}

impl ImmutableBox for Tkhd {
    fn box_type(&self) -> BoxType {
        TYPE_TKHD
    }

    fn size(&self) -> usize {
        match self.version {
            TkhdVersion::V0(_) => 84,
            TkhdVersion::V1(_) => 96,
        }
    }
}

impl ImmutableBoxSync for Tkhd {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        match &self.version {
            TkhdVersion::V0(v) => {
                w.write_all(&[0])?;
                w.write_all(&self.flags)?;
                w.write_all(&v.creation_time.to_be_bytes())?;
                w.write_all(&v.modification_time.to_be_bytes())?;
                w.write_all(&self.track_id.to_be_bytes())?;
                w.write_all(&self.reserved0.to_be_bytes())?;
                w.write_all(&v.duration.to_be_bytes())?;
            }
            TkhdVersion::V1(v) => {
                w.write_all(&[1])?;
                w.write_all(&self.flags)?;
                w.write_all(&v.creation_time.to_be_bytes())?;
                w.write_all(&v.modification_time.to_be_bytes())?;
                w.write_all(&self.track_id.to_be_bytes())?;
                w.write_all(&self.reserved0.to_be_bytes())?;
                w.write_all(&v.duration.to_be_bytes())?;
            }
        }

        for reserved in &self.reserved1 {
            w.write_all(&reserved.to_be_bytes())?;
        }
        w.write_all(&self.layer.to_be_bytes())?;
        w.write_all(&self.alternate_group.to_be_bytes())?;
        w.write_all(&self.volume.to_be_bytes())?;
        w.write_all(&self.reserved2.to_be_bytes())?;
        for matrix in &self.matrix {
            w.write_all(&matrix.to_be_bytes())?;
        }
        w.write_all(&self.width.to_be_bytes())?;
        w.write_all(&self.height.to_be_bytes())?;

        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Tkhd {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        match &self.version {
            TkhdVersion::V0(v) => {
                w.write_all(&[0]).await?;
                w.write_all(&self.flags).await?;
                w.write_all(&v.creation_time.to_be_bytes()).await?;
                w.write_all(&v.modification_time.to_be_bytes()).await?;
                w.write_all(&self.track_id.to_be_bytes()).await?;
                w.write_all(&self.reserved0.to_be_bytes()).await?;
                w.write_all(&v.duration.to_be_bytes()).await?;
            }
            TkhdVersion::V1(v) => {
                w.write_all(&[1]).await?;
                w.write_all(&self.flags).await?;
                w.write_all(&v.creation_time.to_be_bytes()).await?;
                w.write_all(&v.modification_time.to_be_bytes()).await?;
                w.write_all(&self.track_id.to_be_bytes()).await?;
                w.write_all(&self.reserved0.to_be_bytes()).await?;
                w.write_all(&v.duration.to_be_bytes()).await?;
            }
        }

        for reserved in &self.reserved1 {
            w.write_all(&reserved.to_be_bytes()).await?;
        }
        w.write_all(&self.layer.to_be_bytes()).await?;
        w.write_all(&self.alternate_group.to_be_bytes()).await?;
        w.write_all(&self.volume.to_be_bytes()).await?;
        w.write_all(&self.reserved2.to_be_bytes()).await?;
        for matrix in &self.matrix {
            w.write_all(&matrix.to_be_bytes()).await?;
        }
        w.write_all(&self.width.to_be_bytes()).await?;
        w.write_all(&self.height.to_be_bytes()).await?;

        Ok(())
    }
}

/*************************** traf ****************************/

pub const TYPE_TRAF: BoxType = *b"traf";

pub struct Traf;
impl_from!(Traf);

impl ImmutableBox for Traf {
    fn box_type(&self) -> BoxType {
        TYPE_TRAF
    }

    fn size(&self) -> usize {
        0
    }
}

impl ImmutableBoxSync for Traf {
    fn marshal(&self, _: &mut dyn Write) -> Result<(), Mp4Error> {
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Traf {
    async fn marshal(
        &self,
        _: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        Ok(())
    }
}

/*************************** trak ****************************/

pub const TYPE_TRAK: BoxType = *b"trak";

pub struct Trak;
impl_from!(Trak);

impl ImmutableBox for Trak {
    fn box_type(&self) -> BoxType {
        TYPE_TRAK
    }

    fn size(&self) -> usize {
        0
    }
}

impl ImmutableBoxSync for Trak {
    fn marshal(&self, _: &mut dyn Write) -> Result<(), Mp4Error> {
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Trak {
    async fn marshal(
        &self,
        _: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        Ok(())
    }
}

/*************************** trex ****************************/

pub const TYPE_TREX: BoxType = *b"trex";

#[derive(Default)]
pub struct Trex {
    pub full_box: FullBox,
    pub track_id: u32,
    pub default_sample_description_index: u32,
    pub default_sample_duration: u32,
    pub default_sample_size: u32,
    pub default_sample_flags: u32,
}
impl_from!(Trex);

impl ImmutableBox for Trex {
    fn box_type(&self) -> BoxType {
        TYPE_TREX
    }

    fn size(&self) -> usize {
        24
    }
}

impl ImmutableBoxSync for Trex {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        self.full_box.marshal_field(w)?;
        w.write_all(&self.track_id.to_be_bytes())?;
        w.write_all(&self.default_sample_description_index.to_be_bytes())?;
        w.write_all(&self.default_sample_duration.to_be_bytes())?;
        w.write_all(&self.default_sample_size.to_be_bytes())?;
        w.write_all(&self.default_sample_flags.to_be_bytes())?;
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Trex {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        self.full_box.marshal_field2(w).await?;
        w.write_all(&self.track_id.to_be_bytes()).await?;
        w.write_all(&self.default_sample_description_index.to_be_bytes())
            .await?;
        w.write_all(&self.default_sample_duration.to_be_bytes())
            .await?;
        w.write_all(&self.default_sample_size.to_be_bytes()).await?;
        w.write_all(&self.default_sample_flags.to_be_bytes())
            .await?;
        Ok(())
    }
}

/*************************** trun ****************************/

pub const TRUN_DATA_OFFSET_PRESENT: u32 = 0b0000_0000_0001;
pub const TRUN_FIRST_SAMPLE_FLAGS_PRESENT: u32 = 0b0000_0000_0100;
pub const TRUN_SAMPLE_DURATION_PRESENT: u32 = 0b0001_0000_0000;
pub const TRUN_SAMPLE_SIZE_PRESENT: u32 = 0b0010_0000_0000;
pub const TRUN_SAMPLE_FLAGS_PRESENT: u32 = 0b0100_0000_0000;
pub const TRUN_SAMPLE_COMPOSITION_TIME_OFFSET_PRESENT: u32 = 0b1000_0000_0000;

pub enum TrunEntries {
    V1(Vec<TrunEntryV1>),
}

impl TrunEntries {
    fn len(&self) -> usize {
        match self {
            TrunEntries::V1(entries) => entries.len(),
        }
    }
}

pub struct TrunEntryV1 {
    pub sample_duration: u32,
    pub sample_size: u32,
    pub sample_flags: u32,
    pub sample_composition_time_offset: i32,
}

impl TrunEntryV1 {
    fn marshal_field(&self, w: &mut dyn Write, flags: [u8; 3]) -> Result<(), Mp4Error> {
        if check_fullbox_flag(flags, TRUN_SAMPLE_DURATION_PRESENT) {
            w.write_all(&self.sample_duration.to_be_bytes())?;
        }
        if check_fullbox_flag(flags, TRUN_SAMPLE_SIZE_PRESENT) {
            w.write_all(&self.sample_size.to_be_bytes())?;
        }
        if check_fullbox_flag(flags, TRUN_SAMPLE_FLAGS_PRESENT) {
            w.write_all(&self.sample_flags.to_be_bytes())?;
        }
        if check_fullbox_flag(flags, TRUN_SAMPLE_COMPOSITION_TIME_OFFSET_PRESENT) {
            w.write_all(&self.sample_composition_time_offset.to_be_bytes())?;
        }
        Ok(())
    }
    async fn marshal_field2(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
        flags: [u8; 3],
    ) -> Result<(), Mp4Error> {
        if check_fullbox_flag(flags, TRUN_SAMPLE_DURATION_PRESENT) {
            w.write_all(&self.sample_duration.to_be_bytes()).await?;
        }
        if check_fullbox_flag(flags, TRUN_SAMPLE_SIZE_PRESENT) {
            w.write_all(&self.sample_size.to_be_bytes()).await?;
        }
        if check_fullbox_flag(flags, TRUN_SAMPLE_FLAGS_PRESENT) {
            w.write_all(&self.sample_flags.to_be_bytes()).await?;
        }
        if check_fullbox_flag(flags, TRUN_SAMPLE_COMPOSITION_TIME_OFFSET_PRESENT) {
            w.write_all(&self.sample_composition_time_offset.to_be_bytes())
                .await?;
        }
        Ok(())
    }
}

pub const TYPE_TRUN: BoxType = *b"trun";

pub struct Trun {
    pub flags: [u8; 3],

    pub data_offset: i32,
    pub first_sample_flags: u32,
    pub entries: TrunEntries,
}
impl_from!(Trun);

fn trun_field_size(fullbox_flags: [u8; 3]) -> usize {
    let mut total = 0;
    if check_fullbox_flag(fullbox_flags, TRUN_SAMPLE_DURATION_PRESENT) {
        total += 4;
    }
    if check_fullbox_flag(fullbox_flags, TRUN_SAMPLE_SIZE_PRESENT) {
        total += 4;
    }
    if check_fullbox_flag(fullbox_flags, TRUN_SAMPLE_FLAGS_PRESENT) {
        total += 4;
    }
    if check_fullbox_flag(fullbox_flags, TRUN_SAMPLE_COMPOSITION_TIME_OFFSET_PRESENT) {
        total += 4;
    }
    total
}

impl ImmutableBox for Trun {
    fn box_type(&self) -> BoxType {
        TYPE_TRUN
    }

    fn size(&self) -> usize {
        let mut total = 8;
        if check_fullbox_flag(self.flags, TRUN_DATA_OFFSET_PRESENT) {
            total += 4;
        }
        if check_fullbox_flag(self.flags, TRUN_FIRST_SAMPLE_FLAGS_PRESENT) {
            total += 4;
        }
        let field_size = trun_field_size(self.flags);
        total += field_size * self.entries.len();
        total
    }
}

impl ImmutableBoxSync for Trun {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        match &self.entries {
            TrunEntries::V1(_) => w.write_all(&[1])?,
        }
        w.write_all(&self.flags)?;
        w.write_all(
            &u32::try_from(self.entries.len())
                .map_err(|e| Mp4Error::FromInt("trun".to_owned(), e))?
                .to_be_bytes(),
        )?;
        if check_fullbox_flag(self.flags, TRUN_DATA_OFFSET_PRESENT) {
            w.write_all(&self.data_offset.to_be_bytes())?;
        }
        if check_fullbox_flag(self.flags, TRUN_FIRST_SAMPLE_FLAGS_PRESENT) {
            w.write_all(&self.first_sample_flags.to_be_bytes())?;
        }
        match &self.entries {
            TrunEntries::V1(entries) => {
                for entry in entries {
                    entry.marshal_field(w, self.flags)?;
                }
            }
        };
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Trun {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        match &self.entries {
            TrunEntries::V1(_) => w.write_all(&[1]).await?,
        }
        w.write_all(&self.flags).await?;
        w.write_all(
            &u32::try_from(self.entries.len())
                .map_err(|e| Mp4Error::FromInt("trun".to_owned(), e))?
                .to_be_bytes(),
        )
        .await?;
        if check_fullbox_flag(self.flags, TRUN_DATA_OFFSET_PRESENT) {
            w.write_all(&self.data_offset.to_be_bytes()).await?;
        }
        if check_fullbox_flag(self.flags, TRUN_FIRST_SAMPLE_FLAGS_PRESENT) {
            w.write_all(&self.first_sample_flags.to_be_bytes()).await?;
        }
        match &self.entries {
            TrunEntries::V1(entries) => {
                for entry in entries {
                    entry.marshal_field2(w, self.flags).await?;
                }
            }
        };
        Ok(())
    }
}

/*************************** vmhd ****************************/

pub const TYPE_VMHD: BoxType = *b"vmhd";

#[derive(Default)]
pub struct Vmhd {
    pub full_box: FullBox,
    pub graphics_mode: u16, // template=0
    pub opcolor: [u16; 3],  // template={0, 0, 0}
}
impl_from!(Vmhd);

impl ImmutableBox for Vmhd {
    fn box_type(&self) -> BoxType {
        TYPE_VMHD
    }

    fn size(&self) -> usize {
        12
    }
}

impl ImmutableBoxSync for Vmhd {
    fn marshal(&self, w: &mut dyn Write) -> Result<(), Mp4Error> {
        self.full_box.marshal_field(w)?;
        w.write_all(&self.graphics_mode.to_be_bytes())?;
        for color in &self.opcolor {
            w.write_all(&color.to_be_bytes())?;
        }
        Ok(())
    }
}

#[async_trait]
impl ImmutableBoxAsync for Vmhd {
    async fn marshal(
        &self,
        w: &mut (dyn AsyncWrite + Unpin + Send + Sync),
    ) -> Result<(), Mp4Error> {
        self.full_box.marshal_field2(w).await?;
        w.write_all(&self.graphics_mode.to_be_bytes()).await?;
        for color in &self.opcolor {
            w.write_all(&color.to_be_bytes()).await?;
        }
        Ok(())
    }
}
