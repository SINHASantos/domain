//! DNS message headers.

use core::fmt;

use crate::new::edns::EdnsRecord;
use crate::utils::dst::UnsizedCopy;

use super::build::{BuildInMessage, NameCompressor};
use super::parse::MessageParser;
use super::wire::{
    AsBytes, BuildBytes, ParseBytes, ParseBytesZC, SplitBytes, SplitBytesZC,
    TruncationError, U16,
};
use super::{Question, Record};

//----------- Message --------------------------------------------------------

/// A DNS message.
#[derive(AsBytes, BuildBytes, ParseBytesZC, UnsizedCopy)]
#[repr(C, packed)]
pub struct Message {
    /// The message header.
    pub header: Header,

    /// The message contents.
    pub contents: [u8],
}

//--- Inspection

impl Message {
    /// Represent this as a mutable byte sequence.
    ///
    /// Given `&mut self`, it is already possible to individually modify the
    /// message header and contents; since neither has invalid instances, it
    /// is safe to represent the entire object as mutable bytes.
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY:
        // - 'Self' has no padding bytes and no interior mutability.
        // - Its size in memory is exactly 'size_of_val(self)'.
        unsafe {
            core::slice::from_raw_parts_mut(
                self as *mut Self as *mut u8,
                core::mem::size_of_val(self),
            )
        }
    }
}

//--- Parsing

impl Message {
    /// Parse the questions and records in this message.
    ///
    /// This returns a fallible iterator of [`MessageItem`]s.
    pub const fn parse(&self) -> MessageParser<'_> {
        MessageParser::for_message(self)
    }
}

//--- Interaction

impl Message {
    /// Truncate the contents of this message to the given size.
    ///
    /// The returned value will have a `contents` field of the given size.
    pub fn truncate(&self, size: usize) -> &Self {
        let bytes = &self.as_bytes()[..12 + size];
        // SAFETY: 'bytes' is at least 12 bytes, making it a valid 'Message'.
        unsafe { Self::parse_bytes_by_ref(bytes).unwrap_unchecked() }
    }

    /// Truncate the contents of this message to the given size, mutably.
    ///
    /// The returned value will have a `contents` field of the given size.
    pub fn truncate_mut(&mut self, size: usize) -> &mut Self {
        let bytes = &mut self.as_bytes_mut()[..12 + size];
        // SAFETY: 'bytes' is at least 12 bytes, making it a valid 'Message'.
        unsafe { Self::parse_bytes_in(bytes).unwrap_unchecked() }
    }

    /// Truncate the contents of this message to the given size, by pointer.
    ///
    /// The returned value will have a `contents` field of the given size.
    ///
    /// # Safety
    ///
    /// This method uses `pointer::offset()`: `self` must be "derived from a
    /// pointer to some allocated object".  There must be at least 12 bytes
    /// between `self` and the end of that allocated object.  A reference to
    /// `Message` will always result in a pointer satisfying this.
    pub unsafe fn truncate_ptr(this: *mut Message, size: usize) -> *mut Self {
        // Extract the metadata from 'this'.  We know it's slice metadata.
        //
        // SAFETY: '[()]' is a zero-sized type and references to it can be
        // created from arbitrary pointers, since every pointer is valid for
        // zero-sized reads.
        let len = unsafe { &*(this as *mut [()]) }.len();
        // Replicate the range check performed by normal indexing operations.
        debug_assert!(size <= len);
        core::ptr::slice_from_raw_parts_mut(this.cast::<u8>(), size)
            as *mut Self
    }
}

//--- Cloning

#[cfg(feature = "alloc")]
impl Clone for alloc::boxed::Box<Message> {
    fn clone(&self) -> Self {
        (*self).unsized_copy_into()
    }
}

//----------- Header ---------------------------------------------------------

/// A DNS message header.
#[derive(
    Copy,
    Clone,
    Debug,
    Hash,
    AsBytes,
    BuildBytes,
    ParseBytes,
    ParseBytesZC,
    SplitBytes,
    SplitBytesZC,
    UnsizedCopy,
)]
#[repr(C)]
pub struct Header {
    /// A unique identifier for the message.
    pub id: U16,

    /// Properties of the message.
    pub flags: HeaderFlags,

    /// Counts of objects in the message.
    pub counts: SectionCounts,
}

//--- Formatting

impl fmt::Display for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} of ID {:04X} ({})",
            self.flags,
            self.id.get(),
            self.counts
        )
    }
}

//----------- HeaderFlags ----------------------------------------------------

/// DNS message header flags.
///
/// This 16-bit field provides information about the containing DNS message.
/// Its contents define the purpose of the message, e.g. whether it is a query
/// or a response.  Due to its small size, it doesn't cover everything; the
/// OPT record may provide additional information, if it is present.
///
/// # Specification
///
// TODO: Update regularly.
//
/// The header field has been updated by several RFCs and the interpretation
/// of its bits has changed in some places.  The following is a collection of
/// the relevant RFC notes; it is up-to-date as of *2025-04-03*.
///
/// The descriptions here are specific to the `QUERY` opcode, which is by far
/// the most common.  Other opcodes can change the interpretation of the bits
/// here.
///
/// ```text
///   15   14   13   12   11   10    9    8
/// +----+----+----+----+----+----+----+----+
/// | QR |       OPCODE      | AA | TC | RD |  } MSB
/// +----+----+----+----+----+----+----+----+
/// | RA |    | AD | CD |       RCODE       |  } LSB
/// +----+----+----+----+----+----+----+----+
///    7    6    5    4    3    2    1    0
/// ```
///
/// Here is a short description of each field.
///
/// - `QR` (Query or Response): set if and only if the message is a response.
///
///   Specified by [RFC 1035, section 4.1.1].
///
/// - `OPCODE`: the specific operation requested by the DNS client.
///
///   Specified by [RFC 1035, section 4.1.1].
///
/// - `AA`: whether the DNS server is authoritative for the primary answer.
///
///   Specified by [RFC 1035, section 4.1.1].
///
/// - `TC`: whether the response is truncated (due to channel limitations).
///
///   Specified by [RFC 1035, section 4.1.1].  Behaviour clarified by [RFC
///   2181, section 9].  Behaviour for DNSSEC servers specified by [RFC 4035,
///   section 3.1].
///
/// - `RD`: whether the DNS client wishes for a recursively resolved answer.
///
///   Specified by [RFC 1035, section 4.1.1].
///
/// - `RA`: whether the DNS server supports recursive resolution.
///
///   Specified by [RFC 1035, section 4.1.1].
///
/// - `AD`: whether the DNS server has authenticated the answer.
///
///   Defined by [RFC 2535, section 6.1].  Behaviour for authoritative name
///   servers specified by [RFC 4035, section 3.1.6].  Behaviour for recursive
///   name servers specified by [RFC 4035, section 3.2.3] and updated by [RFC
///   6840, section 5.8].  Behaviour for DNS clients specified by [RFC 6840,
///   section 5.7].
///
/// - `CD`: whether the DNS server should avoid authenticating the answer.
///
///   Defined by [RFC 2535, section 6.1].  Behaviour for authoritative name
///   servers specified by [RFC 4035, section 3.1.6].  Behaviour for recursive
///   name servers specified by [RFC 4035, section 3.2.2] and updated by [RFC
///   6840, section 5.9].
///
/// - `RCODE`: the response status of the DNS server.
///
///   Specified by [RFC 1035, section 4.1.1].
///
/// [RFC 1035, section 4.1.1]: https://datatracker.ietf.org/doc/html/rfc1035#section-4.1.1
/// [RFC 2181, section 9]: https://datatracker.ietf.org/doc/html/rfc2181#section-9
/// [RFC 2535, section 6.1]: https://datatracker.ietf.org/doc/html/rfc2535#section-6.1
/// [RFC 4035, section 3.1]: https://datatracker.ietf.org/doc/html/rfc4035#section-3.1
/// [RFC 4035, section 3.1.6]: https://datatracker.ietf.org/doc/html/rfc4035#section-3.1.6
/// [RFC 4035, section 3.2.2]: https://datatracker.ietf.org/doc/html/rfc4035#section-3.2.2
/// [RFC 4035, section 3.2.3]: https://datatracker.ietf.org/doc/html/rfc4035#section-3.2.3
/// [RFC 6840, section 5.7]: https://datatracker.ietf.org/doc/html/rfc6840#section-5.7
/// [RFC 6840, section 5.8]: https://datatracker.ietf.org/doc/html/rfc6840#section-5.8
/// [RFC 6840, section 5.9]: https://datatracker.ietf.org/doc/html/rfc6840#section-5.9
#[derive(
    Copy,
    Clone,
    Default,
    Hash,
    AsBytes,
    BuildBytes,
    ParseBytes,
    ParseBytesZC,
    SplitBytes,
    SplitBytesZC,
    UnsizedCopy,
)]
#[repr(transparent)]
pub struct HeaderFlags {
    /// The raw flag bits.
    inner: U16,
}

//--- Interaction

impl HeaderFlags {
    /// Get the specified flag bit.
    const fn get_flag(&self, pos: u32) -> bool {
        self.inner.get() & (1 << pos) != 0
    }

    /// Set the specified flag bit.
    fn set_flag(&mut self, pos: u32, value: bool) -> &mut Self {
        self.inner &= !(1 << pos);
        self.inner |= (value as u16) << pos;
        self
    }

    /// The raw flags bits.
    pub const fn bits(&self) -> u16 {
        self.inner.get()
    }

    /// The QR bit.
    pub const fn qr(&self) -> bool {
        self.get_flag(15)
    }

    /// Set the QR bit.
    pub fn set_qr(&mut self, value: bool) -> &mut Self {
        self.set_flag(15, value)
    }

    /// The OPCODE field.
    pub const fn opcode(&self) -> u8 {
        (self.inner.get() >> 11) as u8 & 0xF
    }

    /// Set the OPCODE field.
    pub fn set_opcode(&mut self, value: u8) -> &mut Self {
        debug_assert!(value < 16);
        self.inner &= !(0xF << 11);
        self.inner |= (value as u16) << 11;
        self
    }

    /// The AA bit.
    pub fn aa(&self) -> bool {
        self.get_flag(10)
    }

    /// Set the AA bit.
    pub fn set_aa(&mut self, value: bool) -> &mut Self {
        self.set_flag(10, value)
    }

    /// The TC bit.
    pub fn tc(&self) -> bool {
        self.get_flag(9)
    }

    /// Set the TC bit.
    pub fn set_tc(&mut self, value: bool) -> &mut Self {
        self.set_flag(9, value)
    }

    /// The RD bit.
    pub fn rd(&self) -> bool {
        self.get_flag(8)
    }

    /// Set the RD bit.
    pub fn set_rd(&mut self, value: bool) -> &mut Self {
        self.set_flag(8, value)
    }

    /// The RA bit.
    pub fn ra(&self) -> bool {
        self.get_flag(7)
    }

    /// Set the RA bit.
    pub fn set_ra(&mut self, value: bool) -> &mut Self {
        self.set_flag(7, value)
    }

    /// The AD bit.
    pub fn ad(&self) -> bool {
        self.get_flag(5)
    }

    /// Set the AD bit.
    pub fn set_ad(&mut self, value: bool) -> &mut Self {
        self.set_flag(5, value)
    }

    /// The CD bit.
    pub fn cd(&self) -> bool {
        self.get_flag(4)
    }

    /// Set the CD bit.
    pub fn set_cd(&mut self, value: bool) -> &mut Self {
        self.set_flag(4, value)
    }

    /// The RCODE field.
    pub const fn rcode(&self) -> u8 {
        self.inner.get() as u8 & 0xF
    }

    /// Set the RCODE field.
    pub fn set_rcode(&mut self, value: u8) -> &mut Self {
        debug_assert!(value < 16);
        self.inner &= !0xF;
        self.inner |= value as u16;
        self
    }
}

//--- Formatting

impl fmt::Debug for HeaderFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeaderFlags")
            .field("qr", &self.qr())
            .field("opcode", &self.opcode())
            .field("aa", &self.aa())
            .field("tc", &self.tc())
            .field("rd", &self.rd())
            .field("ra", &self.ra())
            .field("rcode", &self.rcode())
            .field("bits", &self.bits())
            .finish()
    }
}

impl fmt::Display for HeaderFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.qr() {
            if self.rd() {
                f.write_str("recursive ")?;
            }
            write!(f, "query (opcode {})", self.opcode())?;
            if self.cd() {
                f.write_str(" (checking disabled)")?;
            }
        } else {
            if self.ad() {
                f.write_str("authentic ")?;
            }
            if self.aa() {
                f.write_str("authoritative ")?;
            }
            if self.rd() && self.ra() {
                f.write_str("recursive ")?;
            }
            write!(f, "response (rcode {})", self.rcode())?;
        }

        if self.tc() {
            f.write_str(" (message truncated)")?;
        }

        Ok(())
    }
}

//----------- SectionCounts --------------------------------------------------

/// Counts of objects in a DNS message.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    AsBytes,
    BuildBytes,
    ParseBytes,
    ParseBytesZC,
    SplitBytes,
    SplitBytesZC,
    UnsizedCopy,
)]
#[repr(C)]
pub struct SectionCounts {
    /// The number of questions in the message.
    pub questions: U16,

    /// The number of answer records in the message.
    pub answers: U16,

    /// The number of name server records in the message.
    pub authorities: U16,

    /// The number of additional records in the message.
    pub additionals: U16,
}

//--- Interaction

impl SectionCounts {
    /// Represent these counts as an array.
    pub fn as_array(&self) -> &[U16; 4] {
        // SAFETY: 'SectionCounts' has the same layout as '[U16; 4]'.
        unsafe { core::mem::transmute(self) }
    }

    /// Represent these counts as a mutable array.
    pub fn as_array_mut(&mut self) -> &mut [U16; 4] {
        // SAFETY: 'SectionCounts' has the same layout as '[U16; 4]'.
        unsafe { core::mem::transmute(self) }
    }
}

//--- Formatting

impl fmt::Display for SectionCounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut some = false;

        for (num, single, many) in [
            (self.questions.get(), "question", "questions"),
            (self.answers.get(), "answer", "answers"),
            (self.authorities.get(), "authority", "authorities"),
            (self.additionals.get(), "additional", "additionals"),
        ] {
            // Add a comma if we have printed something before.
            if some && num > 0 {
                f.write_str(", ")?;
            }

            // Print a count of this section.
            match num {
                0 => {}
                1 => write!(f, "1 {single}")?,
                n => write!(f, "{n} {many}")?,
            }

            some |= num > 0;
        }

        if !some {
            f.write_str("empty")?;
        }

        Ok(())
    }
}

//----------- MessageItem ----------------------------------------------------

/// A question or a record.
///
/// This is useful for building and parsing the contents of a [`Message`]
/// ergonomically and efficiently.  An iterator of [`MessageItem`]s can be
/// retrieved using [`Message::parse()`].
#[derive(Clone, Debug)]
pub enum MessageItem<N, RD, ED> {
    /// A question.
    Question(Question<N>),

    /// An answer record.
    Answer(Record<N, RD>),

    /// An authority record.
    Authority(Record<N, RD>),

    /// An additional record.
    ///
    /// This does not include EDNS records.
    Additional(Record<N, RD>),

    /// An EDNS record.
    ///
    /// This is a record in the additional section.  It uses a distinct type
    /// as the class and TTL fields of the record are interpreted differently.
    Edns(EdnsRecord<ED>),
}

//--- Transformation

impl<N, RD, ED> MessageItem<N, RD, ED> {
    /// Transform this type's generic parameters.
    pub fn transform<NN, NRD, NED>(
        self,
        name_map: impl FnOnce(N) -> NN,
        rdata_map: impl FnOnce(RD) -> NRD,
        edata_map: impl FnOnce(ED) -> NED,
    ) -> MessageItem<NN, NRD, NED> {
        match self {
            Self::Question(this) => {
                MessageItem::Question(this.transform(name_map))
            }
            Self::Answer(this) => {
                MessageItem::Answer(this.transform(name_map, rdata_map))
            }
            Self::Authority(this) => {
                MessageItem::Authority(this.transform(name_map, rdata_map))
            }
            Self::Additional(this) => {
                MessageItem::Additional(this.transform(name_map, rdata_map))
            }
            Self::Edns(this) => MessageItem::Edns(this.transform(edata_map)),
        }
    }

    /// Transform this type's generic parameters by reference.
    pub fn transform_ref<'a, NN, NRD, NED>(
        &'a self,
        name_map: impl FnOnce(&'a N) -> NN,
        rdata_map: impl FnOnce(&'a RD) -> NRD,
        edata_map: impl FnOnce(&'a ED) -> NED,
    ) -> MessageItem<NN, NRD, NED> {
        match self {
            Self::Question(this) => {
                MessageItem::Question(this.transform_ref(name_map))
            }
            Self::Answer(this) => {
                MessageItem::Answer(this.transform_ref(name_map, rdata_map))
            }
            Self::Authority(this) => MessageItem::Authority(
                this.transform_ref(name_map, rdata_map),
            ),
            Self::Additional(this) => MessageItem::Additional(
                this.transform_ref(name_map, rdata_map),
            ),
            Self::Edns(this) => {
                MessageItem::Edns(this.transform_ref(edata_map))
            }
        }
    }
}

//--- Equality

impl<N, RD, LED, RED> PartialEq<MessageItem<N, RD, RED>>
    for MessageItem<N, RD, LED>
where
    N: PartialEq,
    RD: PartialEq,
    LED: PartialEq<RED>,
{
    fn eq(&self, other: &MessageItem<N, RD, RED>) -> bool {
        match (self, other) {
            (MessageItem::Question(l), MessageItem::Question(r)) => l == r,
            (MessageItem::Answer(l), MessageItem::Answer(r)) => l == r,
            (MessageItem::Authority(l), MessageItem::Authority(r)) => l == r,
            (MessageItem::Additional(l), MessageItem::Additional(r)) => {
                l == r
            }
            (MessageItem::Edns(l), MessageItem::Edns(r)) => l == r,
            _ => false,
        }
    }
}

impl<N: Eq, RD: Eq, ED: Eq> Eq for MessageItem<N, RD, ED> {}

//--- Building into DNS messages

impl<N, RD, ED> BuildInMessage for MessageItem<N, RD, ED>
where
    N: BuildInMessage,
    RD: BuildInMessage,
    ED: BuildBytes,
{
    fn build_in_message(
        &self,
        contents: &mut [u8],
        start: usize,
        compressor: &mut NameCompressor,
    ) -> Result<usize, TruncationError> {
        match self {
            Self::Question(i) => {
                i.build_in_message(contents, start, compressor)
            }
            Self::Answer(i) => {
                i.build_in_message(contents, start, compressor)
            }
            Self::Authority(i) => {
                i.build_in_message(contents, start, compressor)
            }
            Self::Additional(i) => {
                i.build_in_message(contents, start, compressor)
            }
            Self::Edns(i) => i.build_in_message(contents, start, compressor),
        }
    }
}

//--- Building into bytes

impl<N, RD, ED> BuildBytes for MessageItem<N, RD, ED>
where
    N: BuildBytes,
    RD: BuildBytes,
    ED: BuildBytes,
{
    fn build_bytes<'b>(
        &self,
        bytes: &'b mut [u8],
    ) -> Result<&'b mut [u8], TruncationError> {
        match self {
            Self::Question(this) => this.build_bytes(bytes),
            Self::Answer(this) => this.build_bytes(bytes),
            Self::Authority(this) => this.build_bytes(bytes),
            Self::Additional(this) => this.build_bytes(bytes),
            Self::Edns(this) => this.build_bytes(bytes),
        }
    }

    fn built_bytes_size(&self) -> usize {
        match self {
            Self::Question(this) => this.built_bytes_size(),
            Self::Answer(this) => this.built_bytes_size(),
            Self::Authority(this) => this.built_bytes_size(),
            Self::Additional(this) => this.built_bytes_size(),
            Self::Edns(this) => this.built_bytes_size(),
        }
    }
}
