use crate::app::slr;
use crate::dictionary::{BaseType, Dictionary};
use crate::encoders::Encoding;
use std::fmt;
use std::io;
use std::str;

/// A (de)serializer for the classic FIX tag-value encoding.
///
/// The FIX tag-value encoding is designed to be both human-readable and easy for
/// machines to parse.
///
/// Please reach out to the FIX official documentation[^1][^2] for more information.
///
/// [^1]: [FIX TagValue Encoding: Online reference.](https://www.fixtrading.org/standards/tagvalue-online)
///
/// [^2]: [FIX TagValue Encoding: PDF.](https://www.fixtrading.org/standards/tagvalue/)
pub struct TagValue<Z: Transmuter> {
    dict: Dictionary,
    transmuter: Z,
}

pub trait Transmuter: Clone {
    fn soh_separator(&self) -> u8 {
        0x1u8
    }

    fn validate_checksum(&self) -> bool {
        true
    }
}

impl<Z> Encoding<slr::Message> for TagValue<Z>
where
    Z: Transmuter,
{
    type EncodeErr = Error;
    type DecodeErr = Error;

    fn decode(
        &self,
        source: &mut impl io::BufRead,
    ) -> Result<slr::Message, <Self as Encoding<slr::Message>>::DecodeErr> {
        let tag_lookup = StandardTagLookup::new(&self.dict);
        let checksum = Checksum::new();
        let mut field_iter = FieldIter {
            handle: source,
            checksum,
            designator: tag_lookup,
            length: std::u32::MAX,
            is_last: false,
            data_length: 0,
            transmuter: self.transmuter.clone(),
        };
        let mut message = slr::Message::new();
        {
            // `BeginString(8)`.
            let f = field_iter.next().ok_or(Error::Eof)??;
            if f.tag == 8 {
                message.fields.insert(f.tag, f.value);
            } else {
                return Err(Error::InvalidStandardHeader);
            }
        };
        {
            // `BodyLength(9)`.
            let f = field_iter.next().ok_or(Error::InvalidStandardHeader)??;
            if f.tag == 9 {
                message.fields.insert(f.tag, f.value);
            } else {
                return Err(Error::InvalidStandardHeader);
            }
        };
        {
            // `MsgType(35)`.
            let f = field_iter.next().ok_or(Error::InvalidStandardHeader)??;
            if f.tag == 35 {
                message.fields.insert(f.tag, f.value);
            } else {
                return Err(Error::InvalidStandardHeader);
            }
        };
        let mut last_tag = 35;
        for f_result in field_iter {
            let f = f_result?;
            message.fields.insert(f.tag, f.value);
            last_tag = f.tag;
        }
        if last_tag == 10 {
            Ok(message)
        } else {
            Err(Error::InvalidStandardTrailer)
        }
    }

    fn encode(&self, message: slr::Message) -> Result<Vec<u8>, Self::EncodeErr> {
        let mut target = Vec::new();
        for (tag, value) in message.fields {
            let field = slr::Field {
                tag,
                value,
                checksum: 0,
                len: 0,
            };
            field.encode(&mut target)?;
        }
        Ok(target)
    }
}

type DecodeResult<T, Z> = Result<T, <TagValue<Z> as Encoding<slr::Message>>::DecodeErr>;
type EncodeResult<T, Z> = Result<T, <TagValue<Z> as Encoding<slr::Message>>::EncodeErr>;

impl<Z: Transmuter> TagValue<Z> {
    /// Builds a new `TagValue` encoding device with an empty FIX dictionary.
    pub fn new(transmuter: Z) -> Self {
        TagValue {
            dict: Dictionary::empty(),
            transmuter,
        }
    }

    pub fn with_dict(transmuter: Z, dict: Dictionary) -> Self {
        TagValue { dict, transmuter }
    }

    //fn decode_checksum(
    //    &self,
    //    source: &mut impl io::BufRead,
    //    message: &mut slr::Message,
    //) -> DecodeResult<u8> {
    //    let field = parse_field(source, self.separator, &|_: i64| BaseType::Int)?;
    //    if let slr::FixFieldValue::Int(checksum) = field.value {
    //        message.fields.insert(field.tag, field.value);
    //        Ok(checksum as u8)
    //    } else {
    //        Err(Error::Syntax)
    //    }
    //}
}

impl From<io::Error> for Error {
    fn from(_err: io::Error) -> Self {
        Error::Eof // FIXME
    }
}

/// A rolling checksum over a byte array. Sums over each byte wrapping around at
/// 256.
#[derive(Copy, Clone, Debug)]
struct Checksum(u8, usize);

impl Checksum {
    fn new() -> Self {
        Checksum(0, 0)
    }

    fn roll(&mut self, window: &[u8]) {
        for byte in window {
            self.roll_byte(*byte);
        }
    }

    fn roll_byte(&mut self, byte: u8) {
        self.0 = self.0.wrapping_add(byte);
        self.1 += 1;
    }

    fn window_length(&self) -> usize {
        self.1
    }

    fn result(self) -> u8 {
        self.0
    }
}

trait TagLookup {
    fn lookup(&mut self, tag: u32) -> BaseType;
}

struct StandardTagLookup<'d> {
    dictionary: &'d Dictionary,
    data_length: usize,
}

impl<'d> StandardTagLookup<'d> {
    fn new(dict: &'d Dictionary) -> Self {
        StandardTagLookup {
            dictionary: dict,
            data_length: 0,
        }
    }
}

impl<'d> TagLookup for StandardTagLookup<'d> {
    fn lookup(&mut self, tag: u32) -> BaseType {
        self.dictionary
            .get_field(tag)
            .map(|f| f.basetype())
            .unwrap_or(BaseType::String)
    }
}

pub enum TypeInfo {
    Int,
    Float,
    Char,
    String,
    Data(usize),
}

struct FieldIter<'d, R: io::Read, D: TagLookup, Z: Transmuter> {
    handle: &'d mut R,
    checksum: Checksum,
    designator: D,
    length: u32,
    is_last: bool,
    data_length: u32,
    transmuter: Z,
}

impl<'d, R, D, Z> Iterator for FieldIter<'d, R, D, Z>
where
    R: io::BufRead,
    D: TagLookup,
    Z: Transmuter,
{
    type Item = DecodeResult<slr::Field, Z>;

    fn next(&mut self) -> Option<Self::Item> {
        let soh_separator: u8 = self.transmuter.soh_separator();
        if self.is_last {
            return None;
        }
        let mut buffer: Vec<u8> = Vec::new();
        self.handle.read_until(b'=', &mut buffer).unwrap();
        if let None = buffer.pop() {
            return None;
        }
        //println!("{:?}", std::str::from_utf8(&buffer[..]).unwrap());
        let tag = std::str::from_utf8(&buffer[..])
            .unwrap()
            .parse::<i64>()
            .unwrap();
        if tag == 10 {
            self.is_last = true;
        }
        let datatype = self.designator.lookup(tag as u32);
        if let BaseType::Data = datatype {
            buffer = vec![0u8; self.data_length as usize];
            self.handle.read_exact(&mut buffer).unwrap();
            self.checksum.roll(&buffer[..]);
            self.checksum.roll_byte(soh_separator);
            self.handle.read_exact(&mut buffer[0..1]).unwrap();
        } else {
            buffer = vec![];
            self.handle.read_until(soh_separator, &mut buffer).unwrap();
            match buffer.last() {
                Some(b) if *b == soh_separator => buffer.pop(),
                _ => return Some(Err(Error::Eof)),
            };
            self.checksum.roll(&buffer[..]);
        }
        let field_value = field_value(datatype, &buffer[..]).unwrap();
        if let slr::FixFieldValue::Int(l) = field_value {
            self.data_length = l as u32;
        }
        Some(Ok(slr::Field {
            tag,
            value: field_value,
            checksum: self.checksum.0,
            len: self.checksum.window_length(),
        }))
    }
}

fn field_value(datatype: BaseType, buf: &[u8]) -> Result<slr::FixFieldValue, Error> {
    Ok(match datatype {
        BaseType::Char => slr::FixFieldValue::Char(buf[0] as char),
        BaseType::String => {
            slr::FixFieldValue::String(str::from_utf8(buf).map_err(|_| Error::Syntax)?.to_string())
        }
        BaseType::Data => slr::FixFieldValue::Data(buf.to_vec()),
        BaseType::Float => slr::FixFieldValue::Float(
            str::from_utf8(buf)
                .map_err(|_| Error::Syntax)?
                .parse::<f64>()
                .map_err(|_| Error::Syntax)?,
        ),
        BaseType::Int => slr::FixFieldValue::Int(
            str::from_utf8(buf)
                .map_err(|_| Error::Syntax)?
                .parse::<i64>()
                .map_err(|_| Error::Syntax)?,
        ),
    })
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct InvalidChecksum {
    pub expected: u8,
    pub actual: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    FieldWithoutValue(u32),
    RepeatedTag(u32),
    Eof,
    InvalidStandardHeader,
    InvalidStandardTrailer,
    InvalidChecksum(InvalidChecksum),
    Syntax,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SuperError is here!")
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[derive(Clone)]
    struct SimpleTransmuter;

    impl Transmuter for SimpleTransmuter {
        fn soh_separator(&self) -> u8 {
            '|' as u8
        }
    }

    fn encoder() -> TagValue<SimpleTransmuter> {
        TagValue::new(SimpleTransmuter)
    }

    #[test]
    fn can_parse_simple_message() {
        let msg = "8=FIX.4.2|9=251|35=D|49=AFUNDMGR|56=ABROKERt|15=USD|59=0|10=127|";
        let result = encoder().decode(&mut msg.as_bytes());
        assert!(result.is_ok());
    }

    #[test]
    fn message_must_end_with_separator() {
        let msg = "8=FIX.4.2|9=251|35=D|49=AFUNDMGR|56=ABROKERt|15=USD|59=0|10=127";
        let result = encoder().decode(&mut msg.as_bytes());
        assert_eq!(result, Err(Error::Eof));
    }

    #[test]
    fn message_without_checksum() {
        let msg = "8=FIX.4.4|9=251|35=D|49=AFUNDMGR|56=ABROKERt|15=USD|59=0|";
        let result = encoder().decode(&mut msg.as_bytes());
        assert_eq!(result, Err(Error::InvalidStandardTrailer));
    }

    #[test]
    fn message_without_standard_header() {
        let msg = "35=D|49=AFUNDMGR|56=ABROKERt|15=USD|59=0|10=000|";
        let result = encoder().decode(&mut msg.as_bytes());
        assert_eq!(result, Err(Error::InvalidStandardHeader));
    }

    #[test]
    fn detect_incorrect_checksum() {
        let msg = "8=FIX.4.2|9=251|35=D|49=AFUNDMGR|56=ABROKER|15=USD|59=0|10=126|";
        let _result = encoder().decode(&mut msg.as_bytes());
    }
}